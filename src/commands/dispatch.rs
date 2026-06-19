//! Command dispatch module.
//!
//! Contains the CommandContext and dispatches CLI commands to their handlers.

use crate::cli::{Cli, Commands};
use crate::commands::helpers::*;
use crate::config;
use crate::github;
use crate::logger;
use crate::stack;
use crate::terminal;
use crate::writer;

use colored::*;
use futures::future::join_all;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// Shared context for command execution.
pub struct CommandContext {
    pub cli: Cli,
    pub cfg: config::Config,
    pub reviews: Vec<github::PendingReview>,
    pub output_dir: Option<PathBuf>,
    pub include_mine: bool,
    pub include_drafts: bool,
    pub crew_members: Vec<String>,
}

impl CommandContext {
    /// Build context by loading config and fetching reviews.
    pub async fn build(cli: Cli) -> anyhow::Result<Self> {
        let cfg = config::Config::from_env()?;
        let include_mine = cli.include_mine || cli.crew;
        let include_drafts = cli.include_drafts || cli.crew;
        let crew_members = if cli.crew {
            cfg.crew_members.clone()
        } else {
            vec![]
        };

        // Only pay for the (network-bound) pending-reviews prefetch when the
        // command actually consumes `ctx.reviews`. Commands that fetch their own
        // data (TUI, monitor, clean) or that target a specific PR by number would
        // otherwise block on a wasted GitHub round-trip before their UI appears —
        // which is what reads as input lag.
        let reviews = if command_needs_prefetch(&cli) {
            github::fetch_pending_reviews(
                &cfg.github_token,
                &cfg.github_org,
                &cfg.github_repos,
                &cfg.github_username,
                &cfg.github_teams,
                include_mine,
                include_drafts,
                &cli.exclude_prefix,
                &crew_members,
                cfg.max_pr_age_days,
            )
            .await?
        } else {
            Vec::new()
        };

        let output_dir = cli.output_dir.clone().or_else(|| Some(reviews_dir()));

        Ok(Self {
            cli,
            cfg,
            reviews,
            output_dir,
            include_mine,
            include_drafts,
            crew_members,
        })
    }
}

/// Default reviews directory.
pub fn reviews_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("prctrl")
        .join("reviews")
}

/// Whether a command relies on the pending-reviews list being prefetched into
/// `CommandContext::reviews`.
///
/// Returns `false` for commands that fetch their own data and never read
/// `ctx.reviews` (so the eager fetch in `build()` is pure latency), and for the
/// `list --pr/--pr-numbers` paths that resolve PRs by number directly. Defaults
/// to `true` for everything else so we never skip a fetch a command depends on.
fn command_needs_prefetch(cli: &Cli) -> bool {
    match &cli.command {
        // Self-sufficient commands: they fetch (or don't need) data on their own.
        Commands::Tui { .. }
        | Commands::Monitor { .. }
        | Commands::MonitorStop
        | Commands::MonitorStatus
        | Commands::Clean { .. } => false,
        // `list` with explicit PR target fetches by number and ignores ctx.reviews.
        Commands::List {
            pr_numbers: Some(_),
            ..
        }
        | Commands::List { pr: Some(_), .. } => false,
        Commands::List { .. } if cli.pr.is_some() => false,
        _ => true,
    }
}

/// Dispatch to the appropriate command handler based on the CLI command.
pub async fn dispatch(ctx: CommandContext) -> anyhow::Result<()> {
    match ctx.cli.command {
        Commands::List {
            json,
            since_days,
            priority,
            repo,
            author,
            pr,
            pr_numbers,
            commented,
        } => {
            // Handle --commented flag: fetch PRs where user has commented
            if commented {
                let username = ctx.cfg.github_username.clone();
                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();

                match github::fetch_prs_user_commented_on(
                    &token,
                    &org,
                    &ctx.cfg.github_repos,
                    &username,
                )
                .await
                {
                    Ok(commented_prs) => {
                        if commented_prs.is_empty() {
                            println!("\n💬 No PRs found where you have commented.\n");
                        } else {
                            println!(
                                "\n💬 PRs you have commented on ({} total):\n",
                                commented_prs.len()
                            );
                            // Apply filters
                            let filtered: Vec<_> = commented_prs
                                .into_iter()
                                .filter(|r| {
                                    repo.as_ref()
                                        .map(|p| r.repo.to_lowercase().contains(&p.to_lowercase()))
                                        .unwrap_or(true)
                                })
                                .filter(|r| {
                                    author
                                        .as_ref()
                                        .map(|p| {
                                            r.pr_author.to_lowercase().contains(&p.to_lowercase())
                                        })
                                        .unwrap_or(true)
                                })
                                .filter(|r| {
                                    since_days
                                        .map(|d| {
                                            let cutoff = chrono::Utc::now()
                                                - chrono::Duration::days(d as i64);
                                            r.created_at >= cutoff
                                        })
                                        .unwrap_or(true)
                                })
                                .collect();

                            for pr in filtered {
                                let age = (chrono::Utc::now() - pr.created_at).num_days();
                                println!(
                                    "  #{} {} by {} ({}d)",
                                    pr.pr_number,
                                    pr.pr_title.bold(),
                                    pr.pr_author.cyan(),
                                    age
                                );
                            }
                            println!();
                        }
                    }
                    Err(e) => {
                        println!("\n❌ Failed to fetch commented PRs: {}\n", e);
                    }
                }
                return Ok(());
            }

            // Priority: global --pr flag > local --pr > local --pr_numbers
            let target_pr = ctx.cli.pr.or(pr);

            // Handle batch PR numbers first
            let base_reviews: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let nums: Vec<u64> = nums
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = nums.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                // Combine: PRs where user is requested + PRs where user commented
                // Split into two groups
                let requested: Vec<_> = ctx.reviews.clone();
                let mut commented: Vec<_> = Vec::new();

                // Fetch PRs where user has commented
                match github::fetch_prs_user_commented_on(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    &ctx.cfg.github_username,
                )
                .await
                {
                    Ok(commented_prs) => {
                        for cpr in commented_prs {
                            if !requested
                                .iter()
                                .any(|r| r.pr_number == cpr.pr_number && r.repo == cpr.repo)
                            {
                                commented.push(cpr);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Could not fetch commented PRs: {}", e);
                    }
                }

                // Print in two sections
                if !requested.is_empty() {
                    println!("\n📋 {} PR(s) requested for review:", requested.len());
                    logger::print_reviews(&requested, priority);
                }
                if !commented.is_empty() {
                    println!("\n💬 {} PR(s) you have commented on:", commented.len());
                    logger::print_reviews(&commented, priority);
                }
                if requested.is_empty() && commented.is_empty() {
                    println!("\n✅ No pending reviews. You're all clear.\n");
                }
                return Ok(());
            };

            // Apply --since filter
            let filtered: Vec<_> = match since_days {
                Some(days) => {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    base_reviews
                        .iter()
                        .filter(|r| r.created_at >= cutoff)
                        .cloned()
                        .collect()
                }
                None => base_reviews.clone(),
            };

            // Apply --repo filter (partial match, case-insensitive)
            let filtered: Vec<_> = match repo {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.repo.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Apply --author filter (partial match, case-insensitive)
            let filtered: Vec<_> = match author {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.pr_author.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Filter out snoozed PRs (unless --pr or --pr_numbers is specified)
            let skip_snooze = target_pr.is_some() || pr_numbers.is_some();
            let filtered: Vec<_> = if !skip_snooze {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let snoozed_prs = read_snoozed_prs(&snooze_file);

                let _snoozed_count = snoozed_prs.len();
                filtered
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .inspect(|_| ())
                    .collect()
            } else {
                filtered
            };

            if json {
                let json = serde_json::to_string_pretty(&filtered)?;
                println!("{}", json);
            } else {
                logger::print_reviews(&filtered, priority);
            }

            // Write files only in non-JSON mode (JSON is typically for scripting)
            if !json {
                if let Some(ref dir) = ctx.output_dir {
                    let index_path = writer::write_index(dir, &filtered)?;
                    for review in &filtered {
                        writer::write_review(dir, review, None)?;
                    }
                    let index_name = index_path
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_else(|| "INDEX.md".into());
                    println!(
                        "\n📁 Written to {}  (index: {})",
                        dir.display().to_string().cyan(),
                        index_name.dimmed()
                    );
                }
            }
        }

        Commands::Mine {
            json,
            all,
            priority,
            since_days,
            repo,
            author,
            pr_number,
            pr_numbers,
        } => {
            let my_prs = github::fetch_my_open_prs(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &ctx.cfg.github_repos,
                &ctx.cfg.github_username,
                ctx.cli.include_drafts,
                &ctx.cli.exclude_prefix,
                ctx.cfg.max_pr_age_days,
            )
            .await?;

            // Apply --pr / --pr-numbers filter first (global --pr takes precedence)
            let filtered: Vec<_> = {
                let target = ctx.cli.pr.or(pr_number);
                let pr_nums = pr_numbers.as_ref().map(|s| {
                    s.split(',')
                        .filter_map(|p| p.trim().parse::<u64>().ok())
                        .collect::<Vec<u64>>()
                });

                match (target, pr_nums) {
                    (Some(num), _) => my_prs
                        .iter()
                        .filter(|r| r.pr_number == num)
                        .cloned()
                        .collect(),
                    (None, Some(nums)) => my_prs
                        .iter()
                        .filter(|r| nums.contains(&r.pr_number))
                        .cloned()
                        .collect(),
                    (None, None) => my_prs.clone(),
                }
            };

            // Apply --since filter
            let filtered: Vec<_> = match since_days {
                Some(days) => {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    filtered
                        .into_iter()
                        .filter(|r| r.created_at >= cutoff)
                        .collect()
                }
                None => filtered,
            };

            // Apply --repo filter (partial match, case-insensitive)
            let filtered: Vec<_> = match repo {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.repo.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Apply --author filter (partial match, case-insensitive)
            let filtered: Vec<_> = match author {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.pr_author.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Filter out snoozed PRs (consistent with list/delegate/search commands)
            // Skip snooze filtering when --pr, --pr_number, or --pr_numbers is specified
            let skip_snooze_filter =
                ctx.cli.pr.is_some() || pr_number.is_some() || pr_numbers.is_some();
            let filtered: Vec<_> = if !skip_snooze_filter {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let snoozed_prs = read_snoozed_prs(&snooze_file);

                filtered
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            } else {
                filtered
            };

            if filtered.is_empty() {
                if ctx.cli.pr.or(pr_number).is_some() {
                    println!(
                        "No PR #{} found in your open PRs.",
                        ctx.cli.pr.or(pr_number).unwrap()
                    );
                } else {
                    println!("No matching PRs found.");
                }
                return Ok(());
            }

            // Show stacked PRs (always show, regardless of --all flag)
            let detected_stacks = stack::detect_stacks(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &ctx.cfg.github_repos,
                Some(&ctx.cfg.github_username),
                None,
                ctx.cli.include_drafts,
                None,
            )
            .await;
            if let Ok(ref stacks) = detected_stacks {
                if !stacks.is_empty() {
                    print!("{}", stack::render_stacks(stacks));
                }
            } else if let Err(ref e) = detected_stacks {
                eprintln!("Error detecting stacked PRs: {}", e);
            }

            // If --all flag is set, show all without prompting
            if all {
                if json {
                    println!("{}", serde_json::to_string_pretty(&filtered)?);
                } else {
                    logger::print_reviews(&filtered, priority);
                }

                // Show stacked PRs if requested (reuse already-fetched stacks)
                if ctx.cli.show_stacks {
                    if let Ok(ref stacks) = detected_stacks {
                        if !stacks.is_empty() {
                            print!("{}", stack::render_stacks(stacks));
                        }
                    }
                }
            } else {
                // Interactive selection
                logger::print_reviews(&filtered, priority);

                // Show stacked PRs if requested (reuse already-fetched stacks)
                if ctx.cli.show_stacks {
                    if let Ok(ref stacks) = detected_stacks {
                        if !stacks.is_empty() {
                            print!("{}", stack::render_stacks(stacks));
                        }
                    }
                }
                print!(
                    "\n{} ",
                    "Select PRs [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => {
                        let selected: Vec<_> =
                            indices.into_iter().map(|i| filtered[i].clone()).collect();
                        if json {
                            println!("{}", serde_json::to_string_pretty(&selected)?);
                        } else {
                            // Re-print with details
                            for review in &selected {
                                let age_days = (chrono::Utc::now() - review.created_at).num_days();
                                let priority_display = if priority {
                                    let score = logger::calculate_priority_score(review);
                                    format!("  ⭐ {}/5", logger::priority_stars(score))
                                } else {
                                    String::new()
                                };
                                println!(
                                    "  #{} {}  ({}){}\n    👤 {}  •  📦 +{}/-{}  •  ⏱️ {} days\n    🔗 {}",
                                    review.pr_number,
                                    review.pr_title.bold(),
                                    review.repo.cyan(),
                                    priority_display,
                                    review.pr_author.cyan(),
                                    review.additions,
                                    review.deletions,
                                    age_days,
                                    review.pr_url.blue().underline()
                                );
                            }
                        }
                    }
                }
            }

            if let Some(ref dir) = ctx.output_dir {
                let index_path = writer::write_index(dir, &filtered)?;
                for review in &filtered {
                    writer::write_review(dir, review, None)?;
                }
                let index_name = index_path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_else(|| "INDEX.md".into());
                println!(
                    "\n📁 Written to {}  (index: {})",
                    dir.display().to_string().cyan(),
                    index_name.dimmed()
                );
            }
        }

        Commands::Delegate {
            json,
            dry_run,
            priority,
            since_days,
            repo,
            author,
            all,
            quiet,
            pr_numbers,
        } => {
            let pr_number = ctx.cli.pr;

            // Apply filters to ctx.reviews (same logic as List command) unless --pr is specified
            let filtered_reviews: Vec<_> = if pr_number.is_some() {
                // When --pr is specified, bypass filters and use ctx.reviews as-is
                ctx.reviews.clone()
            } else {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Filter out snoozed PRs (unless --pr is specified, consistent with list command)
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                result
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            };

            let targets: Vec<_> = match pr_number {
                Some(num) => {
                    // First, check if the PR exists in filtered_reviews (respects --repo, --author, --since filters + snooze)
                    let from_list: Vec<_> = filtered_reviews
                        .iter()
                        .filter(|r| r.pr_number == num)
                        .cloned()
                        .collect();

                    if !from_list.is_empty() {
                        // Found in filtered list - use that (respects filters)
                        if from_list.len() == 1 {
                            from_list
                        } else {
                            // Same PR number in multiple repos — show matches + prompt
                            println!("PR #{} found in multiple repos:", num);
                            for (i, r) in from_list.iter().enumerate() {
                                println!("  [{}] {} ({})", i + 1, r.pr_title, r.repo);
                            }
                            print!("\n{} ", "Select repo [e.g. 1] (q to quit):".bold());
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match parse_selection(input.trim(), from_list.len()) {
                                Selection::Quit => return Ok(()),
                                Selection::Indices(indices) => {
                                    indices.into_iter().map(|i| from_list[i].clone()).collect()
                                }
                            }
                        }
                    } else {
                        // Not found in filtered list - fetch directly from GitHub as fallback
                        let direct = github::fetch_pr_by_number(
                            &ctx.cfg.github_token,
                            &ctx.cfg.github_org,
                            &ctx.cfg.github_repos,
                            num,
                        )
                        .await?;
                        if direct.is_empty() {
                            println!("No PR #{} found across configured repos.", num);
                            return Ok(());
                        }
                        if direct.len() == 1 {
                            direct
                        } else {
                            // Same PR number in multiple repos — show matches + prompt
                            println!("PR #{} found in multiple repos:", num);
                            for (i, r) in direct.iter().enumerate() {
                                println!("  [{}] {} ({})", i + 1, r.pr_title, r.repo);
                            }
                            print!("\n{} ", "Select repo [e.g. 1] (q to quit):".bold());
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match parse_selection(input.trim(), direct.len()) {
                                Selection::Quit => return Ok(()),
                                Selection::Indices(indices) => {
                                    indices.into_iter().map(|i| direct[i].clone()).collect()
                                }
                            }
                        }
                    }
                }
                None => {
                    // Handle --pr-numbers first (consistent with Assign/Unassign/Approve/Claim)
                    if let Some(ref nums) = pr_numbers {
                        let mut results = Vec::new();
                        for part in nums.split(',') {
                            if let Ok(num) = part.trim().parse::<u64>() {
                                results.push(num);
                            }
                        }
                        if results.is_empty() {
                            println!("❌ No valid PR numbers provided.");
                            return Ok(());
                        }
                        // Fetch all specified PRs in parallel
                        let fetch_futures = results.iter().map(|num| {
                            github::fetch_pr_by_number(
                                &ctx.cfg.github_token,
                                &ctx.cfg.github_org,
                                &ctx.cfg.github_repos,
                                *num,
                            )
                        });
                        let all_prs: Vec<_> = join_all(fetch_futures)
                            .await
                            .into_iter()
                            .filter_map(|r| r.ok())
                            .flatten()
                            .collect();

                        if all_prs.is_empty() {
                            println!("No PRs found for the specified numbers.");
                            return Ok(());
                        }

                        // Apply filters to fetched PRs
                        let filtered: Vec<_> = all_prs
                            .into_iter()
                            .filter(|r| {
                                let mut matches = true;
                                if let Some(ref repo_filter) = repo {
                                    let pattern = repo_filter.to_lowercase();
                                    matches = matches && r.repo.to_lowercase().contains(&pattern);
                                }
                                if let Some(ref author_filter) = author {
                                    let pattern = author_filter.to_lowercase();
                                    matches =
                                        matches && r.pr_author.to_lowercase().contains(&pattern);
                                }
                                matches
                            })
                            .collect();

                        if filtered.is_empty() {
                            println!("No matching reviews found among specified PRs.");
                            return Ok(());
                        }

                        filtered
                    } else if filtered_reviews.is_empty() {
                        println!("No matching reviews found.");
                        return Ok(());
                    } else if all {
                        filtered_reviews.into_iter().collect()
                    } else {
                        logger::print_reviews(&filtered_reviews, false);

                        print!(
                            "\n{} ",
                            "Select PRs to delegate [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                        );
                        io::stdout().flush()?;

                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        let input = input.trim();

                        match parse_selection(input, filtered_reviews.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => indices
                                .into_iter()
                                .map(|i| filtered_reviews[i].clone())
                                .collect(),
                        }
                    }
                }
            };

            if targets.is_empty() {
                println!("No matching reviews found.");
                return Ok(());
            }

            // Dry-run mode: just show what would be delegated
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be delegated:\n");
                for (i, review) in targets.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                }
                println!("\n  Total: {} PR(s)\n", targets.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct DelegateResult {
                pr_number: u64,
                pr_title: String,
                repo: String,
                url: String,
                success: bool,
                summary: Option<String>,
                error: Option<String>,
            }

            // Parallel delegation for both modes
            let delegate_futures = targets.iter().map(|review| {
                let instruction_path = ctx.cli.instruction_path.clone();
                async move {
                    let summary = crate::dispatcher::delegate_to_claude(review, instruction_path);
                    (review.clone(), summary)
                }
            });
            let results: Vec<(github::PendingReview, Result<String, anyhow::Error>)> =
                join_all(delegate_futures).await;

            if json {
                let output: Vec<DelegateResult> = results
                    .into_iter()
                    .map(|(review, summary)| match summary {
                        Ok(s) => DelegateResult {
                            pr_number: review.pr_number,
                            pr_title: review.pr_title,
                            repo: review.repo,
                            url: review.pr_url,
                            success: true,
                            summary: Some(s),
                            error: None,
                        },
                        Err(e) => DelegateResult {
                            pr_number: review.pr_number,
                            pr_title: review.pr_title,
                            repo: review.repo,
                            url: review.pr_url,
                            success: false,
                            summary: None,
                            error: Some(e.to_string()),
                        },
                    })
                    .collect();

                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                // Show results as they complete with progress
                let total = targets.len();
                for (i, (review, summary)) in results.into_iter().enumerate() {
                    if !quiet {
                        print!(
                            "\n[{:>2}/{:<2}] #{} {}... ",
                            i + 1,
                            total,
                            review.pr_number,
                            review.pr_title
                        );
                        io::stdout().flush()?;
                    }

                    match summary {
                        Ok(summary) => {
                            if !quiet {
                                println!("{}", "✅ Done".green());
                                logger::print_delegate_result(&review, &summary);

                                if let Some(ref dir) = ctx.output_dir {
                                    let path = writer::write_review(dir, &review, Some(&summary))?;
                                    println!(
                                        "   💾 Saved → {}",
                                        path.display().to_string().dimmed()
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            if !quiet {
                                println!("{}", "❌ Failed".red());
                                eprintln!("   Error: {}", e);
                            }
                        }
                    }
                }

                if !quiet {
                    println!();
                }

                if let Some(ref dir) = ctx.output_dir {
                    writer::write_index(dir, &ctx.reviews)?;
                }
            }
        }

        Commands::Stats {
            json,
            all,
            dry_run,
            pr_numbers: _pr_numbers,
            pr_number,
            repo,
            author,
            priority,
            since_days,
        } => {
            use chrono::Duration;
            use std::collections::HashMap;

            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr_number);

            // Apply filters (same logic as List command)
            let filtered: Vec<_> = {
                let mut result = match target_pr {
                    Some(num) => ctx
                        .reviews
                        .iter()
                        .filter(|r| r.pr_number == num)
                        .cloned()
                        .collect(),
                    None => ctx.reviews.clone(),
                };
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }
                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }
                result
            };

            // Dry-run mode: preview which PRs would be included
            if dry_run {
                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                println!(
                    "\n📊 {} PR(s) would be included in stats:\n",
                    filtered.len()
                );
                for (i, review) in filtered.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!("  ⭐ {}/5", score)
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}  {}#{} — {}{}",
                        i + 1,
                        review.repo.dimmed(),
                        review.pr_number,
                        review.pr_title.bold(),
                        priority_label
                    );
                }
                println!();
                return Ok(());
            }

            // --all flag: show stats without interactive selection
            let stats_reviews: Vec<_> = if all {
                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered
            } else if target_pr.is_none() {
                // Interactive mode: let user select PRs
                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered, false);
                print!(
                    "\n{} ",
                    "Select PRs to include in stats [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => {
                        indices.into_iter().map(|i| filtered[i].clone()).collect()
                    }
                }
            } else {
                filtered
            };

            let mut repo_counts: HashMap<String, usize> = HashMap::new();
            let mut author_counts: HashMap<String, usize> = HashMap::new();
            let mut total_additions = 0u64;
            let mut total_deletions = 0u64;

            for review in &stats_reviews {
                *repo_counts.entry(review.repo.clone()).or_insert(0) += 1;
                if !review.pr_author.is_empty() {
                    *author_counts.entry(review.pr_author.clone()).or_insert(0) += 1;
                }
                total_additions += review.additions;
                total_deletions += review.deletions;
            }

            if json {
                #[derive(serde::Serialize)]
                struct StatsOutput<'a> {
                    total: usize,
                    total_additions: u64,
                    total_deletions: u64,
                    avg_wait_days: f64,
                    oldest_pr_number: Option<u64>,
                    oldest_pr_age_days: Option<i64>,
                    by_repository: &'a HashMap<String, usize>,
                    by_author: &'a HashMap<String, usize>,
                }

                let now = chrono::Utc::now();
                let avg_wait_days = if stats_reviews.is_empty() {
                    0.0
                } else {
                    let total_wait: Duration =
                        stats_reviews.iter().map(|r| now - r.created_at).sum();
                    (total_wait / stats_reviews.len() as i32).num_hours() as f64 / 24.0
                };

                let oldest_pr = stats_reviews.first().map(|r| {
                    let age = (now - r.created_at).num_days();
                    (r.pr_number, age)
                });

                let output = StatsOutput {
                    total: stats_reviews.len(),
                    total_additions,
                    total_deletions,
                    avg_wait_days: avg_wait_days.round(),
                    oldest_pr_number: oldest_pr.map(|(n, _)| n),
                    oldest_pr_age_days: oldest_pr.map(|(_, a)| a),
                    by_repository: &repo_counts,
                    by_author: &author_counts,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("\n📊 Review Statistics\n{}", "─".repeat(40));
                println!("  Total pending reviews: {}", stats_reviews.len());
                println!(
                    "  Total lines changed:   +{} / -{}",
                    total_additions.to_string().green(),
                    total_deletions.to_string().red()
                );

                if !stats_reviews.is_empty() {
                    // Average wait time
                    let now = chrono::Utc::now();
                    let total_wait: Duration =
                        stats_reviews.iter().map(|r| now - r.created_at).sum();
                    let avg_wait = total_wait / stats_reviews.len() as i32;
                    println!(
                        "  Avg time waiting:      {} days",
                        (avg_wait.num_hours() as f64 / 24.0).round()
                    );

                    // Oldest PR
                    if let Some(oldest) = stats_reviews.first() {
                        let age = now - oldest.created_at;
                        println!(
                            "  Oldest PR:             #{} ({} ago)",
                            oldest.pr_number,
                            format_duration(age)
                        );
                    }

                    // Breakdown by repo
                    if !repo_counts.is_empty() {
                        println!("\n  By repository:");
                        let mut repo_vec: Vec<_> = repo_counts.iter().collect();
                        repo_vec.sort_by(|a, b| b.1.cmp(a.1));
                        for (repo, count) in repo_vec {
                            println!("    {}: {}", repo, count);
                        }
                    }

                    // Breakdown by author
                    if !author_counts.is_empty() {
                        println!("\n  By author:");
                        let mut author_vec: Vec<_> = author_counts.iter().collect();
                        author_vec.sort_by(|a, b| b.1.cmp(a.1));
                        for (author, count) in author_vec {
                            let bar = "█".repeat(*count).cyan();
                            println!("    {}  {}", author.bold(), bar);
                        }
                    }

                    // Priority breakdown if --priority flag is set
                    if priority && !stats_reviews.is_empty() {
                        let mut scored: Vec<_> = stats_reviews
                            .iter()
                            .map(|r| {
                                let score = logger::calculate_priority_score(r);
                                (r, score)
                            })
                            .collect();
                        scored.sort_by_key(|b| std::cmp::Reverse(b.1)); // highest priority first

                        // Highlight the single most urgent PR
                        if let Some((most_urgent, top_score)) = scored.first() {
                            let age_days = (chrono::Utc::now() - most_urgent.created_at).num_days();
                            let age_str = if age_days == 0 {
                                "today".to_string()
                            } else if age_days == 1 {
                                "1 day".to_string()
                            } else {
                                format!("{} days ago", age_days)
                            };
                            let total = most_urgent.additions + most_urgent.deletions;
                            println!("\n  🚨 Most Urgent:");
                            println!(
                                "    {}  #{}  {}",
                                most_urgent.pr_title.bold(),
                                most_urgent.pr_number,
                                logger::priority_stars(*top_score).red()
                            );
                            println!(
                                "    👤 {}  •  📦 {} lines  •  ⏱️ {}  •  {}",
                                most_urgent.pr_author.cyan(),
                                total,
                                age_str.red(),
                                most_urgent.repo.dimmed()
                            );
                            println!("    🔗 {}", most_urgent.pr_url.blue().underline());
                        }

                        // Group by score for breakdown
                        let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> =
                            HashMap::new();
                        for (review, score) in &scored {
                            score_groups.entry(*score).or_default().push(review);
                        }

                        println!("\n  Priority breakdown:");
                        for score in (1..=5).rev() {
                            if let Some(prs) = score_groups.get(&score) {
                                let stars = logger::priority_stars(score);
                                let age_days = (chrono::Utc::now() - prs[0].created_at).num_days();
                                let age_str = if age_days == 0 {
                                    "today".to_string()
                                } else if age_days == 1 {
                                    "1 day".to_string()
                                } else {
                                    format!("{} days", age_days)
                                };
                                println!(
                                    "    ⭐{}  {} PR(s)  •  oldest: {}  •  +{}/-{} lines",
                                    stars,
                                    prs.len(),
                                    age_str,
                                    prs.iter().map(|r| r.additions).sum::<u64>(),
                                    prs.iter().map(|r| r.deletions).sum::<u64>()
                                );
                            }
                        }
                    }
                } else {
                    println!("\n  😴 No pending reviews. You're all clear!");
                }

                println!();
            }
        }

        Commands::TeamSummary {
            json,
            all: _all,
            pr,
            pr_numbers: _pr_numbers,
            pr_number,
            repo,
            author,
            priority,
            since_days,
        } => {
            use serde::Serialize;
            use std::collections::HashMap;

            // Priority: global --pr flag > local --pr > local --pr_number
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply filters (same logic as List command)
            let filtered: Vec<_> = {
                let mut result = match target_pr {
                    Some(num) => ctx
                        .reviews
                        .iter()
                        .filter(|r| r.pr_number == num)
                        .cloned()
                        .collect(),
                    None => ctx.reviews.clone(),
                };
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }
                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }
                result
            };

            let mut team_counts: HashMap<String, usize> = HashMap::new();
            let mut unassigned = 0usize;

            for review in &filtered {
                if review.pr_author.is_empty() {
                    unassigned += 1;
                } else {
                    *team_counts.entry(review.pr_author.clone()).or_insert(0) += 1;
                }
            }

            // Show breakdown by repository
            let mut repo_counts: HashMap<String, usize> = HashMap::new();
            for review in &filtered {
                *repo_counts.entry(review.repo.clone()).or_insert(0) += 1;
            }

            if json {
                #[derive(Serialize)]
                struct TeamSummaryOutput {
                    total_pending: usize,
                    by_author: HashMap<String, usize>,
                    unassigned: usize,
                    by_repository: HashMap<String, usize>,
                }

                let output = TeamSummaryOutput {
                    total_pending: filtered.len(),
                    by_author: team_counts,
                    unassigned,
                    by_repository: repo_counts,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("\n👥 Team Review Summary\n{}", "─".repeat(40));
                println!("  Total pending reviews: {}", filtered.len());

                if !team_counts.is_empty() {
                    println!("\n  By author:");
                    let mut sorted: Vec<_> = team_counts.iter().collect();
                    sorted.sort_by(|a, b| b.1.cmp(a.1)); // descending by count
                    for (author, count) in sorted {
                        let bar = "█".repeat(*count).cyan();
                        println!("    {}  {}", author.bold(), bar);
                    }
                }

                if unassigned > 0 {
                    println!("\n  Unassigned/Unknown: {}", unassigned);
                }

                println!("\n  By repository:");
                let mut repo_sorted: Vec<_> = repo_counts.iter().collect();
                repo_sorted.sort_by(|a, b| b.1.cmp(a.1));
                for (repo, count) in repo_sorted {
                    println!("    {}: {}", repo, count);
                }

                // Priority breakdown if --priority flag is set
                if priority && !filtered.is_empty() {
                    println!("\n  Priority breakdown:");
                    let mut scored: Vec<_> = filtered
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            (r, score)
                        })
                        .collect();
                    scored.sort_by_key(|b| std::cmp::Reverse(b.1)); // highest priority first

                    // Group by score
                    let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> = HashMap::new();
                    for (review, score) in &scored {
                        score_groups.entry(*score).or_default().push(review);
                    }

                    for score in (1..=5).rev() {
                        if let Some(prs) = score_groups.get(&score) {
                            let stars = logger::priority_stars(score);
                            let age_days = (chrono::Utc::now() - prs[0].created_at).num_days();
                            let age_str = if age_days == 0 {
                                "today".to_string()
                            } else if age_days == 1 {
                                "1 day".to_string()
                            } else {
                                format!("{} days", age_days)
                            };
                            println!(
                                "    ⭐{}  {} PR(s)  •  oldest: {}  •  +{}/-{} lines",
                                stars,
                                prs.len(),
                                age_str,
                                prs.iter().map(|r| r.additions).sum::<u64>(),
                                prs.iter().map(|r| r.deletions).sum::<u64>()
                            );
                        }
                    }
                }

                println!();
            }
        }

        Commands::Load {
            threshold,
            repo,
            author,
            since_days,
            priority,
            json,
            pr,
            pr_numbers,
        } => {
            use serde::Serialize;
            use std::collections::HashMap;

            let min_threshold = threshold.unwrap_or(3) as usize;

            #[derive(Debug, Clone, Serialize)]
            struct MemberLoad {
                author: String,
                pr_count: usize,
                total_lines: u64,
                total_additions: u64,
                total_deletions: u64,
                avg_age_days: f64,
                oldest_pr_age_days: i64,
                draft_count: usize,
                repos: Vec<String>,
                overloaded: bool,
            }

            // Priority: global --pr flag > local --pr > local --pr_number > --pr-numbers
            let target_pr = ctx.cli.pr.or(pr);

            // Handle batch PR numbers first
            let base_reviews: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let nums: Vec<u64> = nums
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = nums.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                ctx.reviews.clone()
            };

            // Apply --repo, --author, and --since-days filters to ctx.reviews first
            let filtered: Vec<_> = {
                let mut result = base_reviews;

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            let mut by_author: HashMap<String, Vec<&github::PendingReview>> = HashMap::new();
            for r in &filtered {
                by_author.entry(r.pr_author.clone()).or_default().push(r);
            }

            let now = chrono::Utc::now();
            let mut loads: Vec<MemberLoad> = Vec::new();

            for (author, prs) in &by_author {
                let total_lines = prs.iter().map(|r| r.additions + r.deletions).sum::<u64>();
                let total_additions: u64 = prs.iter().map(|r| r.additions).sum();
                let total_deletions: u64 = prs.iter().map(|r| r.deletions).sum();
                let ages: Vec<i64> = prs
                    .iter()
                    .map(|r| (now - r.created_at).num_days())
                    .collect();
                let avg_age = if ages.is_empty() {
                    0.0
                } else {
                    ages.iter().sum::<i64>() as f64 / ages.len() as f64
                };
                let oldest_age = ages.iter().max().copied().unwrap_or(0);
                let draft_count = prs.iter().filter(|r| r.draft).count();
                let mut repos: Vec<String> = prs.iter().map(|r| r.repo.clone()).collect();
                repos.sort();
                repos.dedup();

                loads.push(MemberLoad {
                    author: author.clone(),
                    pr_count: prs.len(),
                    total_lines,
                    total_additions,
                    total_deletions,
                    avg_age_days: avg_age,
                    oldest_pr_age_days: oldest_age,
                    draft_count,
                    repos,
                    overloaded: prs.len() >= min_threshold,
                });
            }

            // Sort by pr_count descending
            loads.sort_by_key(|b| std::cmp::Reverse(b.pr_count));

            let total_prs = filtered.len();
            let total_load_members = loads.len();

            if json {
                #[derive(Serialize)]
                struct LoadOutput<'a> {
                    total_prs: usize,
                    total_members: usize,
                    threshold: usize,
                    members: Vec<&'a MemberLoad>,
                }
                let output = LoadOutput {
                    total_prs,
                    total_members: total_load_members,
                    threshold: min_threshold,
                    members: loads.iter().collect(),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("\n⚖️  Review Load Distribution\n{}", "─".repeat(50));
                println!(
                    "  Total pending PRs: {} | Team members: {} | Overload threshold: {} PRs\n",
                    total_prs.to_string().cyan(),
                    total_load_members,
                    min_threshold
                );

                if loads.is_empty() {
                    println!("  No review requests found.\n");
                    return Ok(());
                }

                // Summary bar
                let max_count = loads.first().map(|l| l.pr_count).unwrap_or(1);
                println!("  Workload bar (max {} PRs):", max_count);
                print!("  ");
                for load in &loads {
                    let bar_len =
                        ((load.pr_count as f64 / max_count as f64) * 20.0).round() as usize;
                    let bar = if load.overloaded {
                        "█".repeat(bar_len).red()
                    } else {
                        "█".repeat(bar_len).cyan()
                    };
                    print!("{}", bar);
                }
                println!();
                print!("  ");
                for load in &loads {
                    let ch = if load.overloaded { '🔴' } else { '🟢' };
                    print!("{} ", ch);
                    let spaces = load.pr_count.to_string().len();
                    print!("{}", " ".repeat(spaces));
                }
                println!("\n");

                // Detailed table
                println!(
                    "  {:<20} {:>4} {:>8} {:>8} {:>10} {:>10}",
                    "Author".bold(),
                    "PRs",
                    "+add",
                    "-del",
                    "Avg Age",
                    "Status"
                );
                println!("  {}", "─".repeat(70));

                let overloaded_count = loads.iter().filter(|l| l.overloaded).count();
                let healthy_count = total_load_members - overloaded_count;

                for load in &loads {
                    let status = if load.overloaded {
                        "🔴 OVERLOADED".red().to_string()
                    } else {
                        "🟢 OK".green().to_string()
                    };
                    let age_str = if load.avg_age_days < 1.0 {
                        "<1d".to_string()
                    } else {
                        format!("{:.0}d", load.avg_age_days)
                    };
                    println!(
                        "  {:<20} {:>4} {:>+8} {:>+8} {:>10} {}",
                        load.author.bold(),
                        load.pr_count.to_string().cyan(),
                        load.total_additions.to_string().green(),
                        load.total_deletions.to_string().red(),
                        age_str.yellow(),
                        status
                    );
                    if !load.repos.is_empty() {
                        println!("  {:<20} repos: {}", "", load.repos.join(", ").dimmed());
                    }
                }

                println!("  {}", "─".repeat(70));
                println!(
                    "  Summary: {} healthy | {} overloaded",
                    healthy_count,
                    overloaded_count.to_string().red()
                );

                // Recommendations
                println!("\n  💡 Recommendations:");
                if overloaded_count > 0 {
                    let overloaded_members: Vec<_> =
                        loads.iter().filter(|l| l.overloaded).collect();
                    if let Some(top) = overloaded_members.first() {
                        println!(
                            "  • {} has the most pending PRs ({}), consider reassigning some",
                            top.author.bold(),
                            top.pr_count
                        );
                    }
                    let avg = total_prs as f64 / total_load_members.max(1) as f64;
                    println!("  • Average load: {:.1} PRs per member", avg);
                    let underloaded: Vec<_> = loads
                        .iter()
                        .filter(|l| l.pr_count < (avg as usize * 2 / 3))
                        .collect();
                    if !underloaded.is_empty() {
                        println!(
                            "  • Consider delegating to: {}",
                            underloaded
                                .iter()
                                .map(|l| l.author.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                } else {
                    println!("  • Team workload is balanced! 🎉");
                }

                // Priority breakdown if --priority flag is set
                if priority && !filtered.is_empty() {
                    println!("\n  Priority breakdown:");
                    let mut scored: Vec<_> = filtered
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            (r, score)
                        })
                        .collect();
                    scored.sort_by_key(|b| std::cmp::Reverse(b.1)); // highest priority first

                    // Group by score
                    let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> = HashMap::new();
                    for (review, score) in &scored {
                        score_groups.entry(*score).or_default().push(review);
                    }

                    for score in (1..=5).rev() {
                        if let Some(prs) = score_groups.get(&score) {
                            let stars = logger::priority_stars(score);
                            let age_days = (chrono::Utc::now() - prs[0].created_at).num_days();
                            let age_str = if age_days == 0 {
                                "today".to_string()
                            } else if age_days == 1 {
                                "1 day".to_string()
                            } else {
                                format!("{} days", age_days)
                            };
                            println!(
                                "    ⭐{}  {} PR(s)  •  oldest: {}  •  +{}/-{} lines",
                                stars,
                                prs.len(),
                                age_str,
                                prs.iter().map(|r| r.additions).sum::<u64>(),
                                prs.iter().map(|r| r.deletions).sum::<u64>()
                            );
                        }
                    }
                }

                println!();
            }
        }

        Commands::Clean { dry_run, quiet } => {
            if let Some(ref dir) = ctx.output_dir {
                if dir.exists() {
                    let files: Vec<_> = std::fs::read_dir(dir)?
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().is_file())
                        .collect();
                    let count = files.len();

                    if dry_run {
                        if !quiet {
                            println!("\n🔍 Dry-run mode — the following would be deleted:\n");
                            for f in &files {
                                println!("  🗑️  {}", f.path().display().to_string().red());
                            }
                            println!("\n  Total: {} file(s)\n", count);
                            println!("  (dry-run — no files were deleted)");
                        }
                        // Quiet mode: suppress all output for scripting compatibility
                    } else {
                        std::fs::remove_dir_all(dir)?;
                        if !quiet {
                            println!(
                                "🧹 Removed {} file(s) from {}",
                                count,
                                dir.display().to_string().cyan()
                            );
                        } else {
                            println!("Removed {} file(s)", count);
                        }
                    }
                } else {
                    if !quiet {
                        println!("Nothing to clean — {} does not exist.", dir.display());
                    }
                }
            }
        }

        Commands::Monitor {
            interval,
            notify,
            auto_open,
            no_auto_open,
            interactive,
        } => {
            let effective_auto_open = auto_open && !no_auto_open;

            println!(
                "👀 Starting PR monitor (polling every {} seconds)...",
                interval
            );
            if interactive {
                println!("🎮 Interactive mode enabled - will prompt for actions on new PRs");
            }
            if notify {
                if effective_auto_open {
                    println!("🔔 Notifications enabled with auto-open in Chrome");
                } else {
                    println!("🔔 Notifications enabled (URLs shown in message)");
                }
            } else {
                println!("🔔 Notifications disabled");
            }
            println!("Press Ctrl+C to stop.");

            crate::dispatcher::monitor_new_prs(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &ctx.cfg.github_repos,
                &ctx.cfg.github_username,
                &ctx.cfg.github_teams,
                ctx.include_mine,
                ctx.include_drafts,
                &ctx.cli.exclude_prefix,
                &ctx.crew_members,
                interval,
                notify,
                effective_auto_open,
                interactive,
                ctx.output_dir.clone(),
            )
            .await?;
        }

        Commands::MonitorStop => {
            println!("🛑 Stopping monitor process...");
            match crate::dispatcher::kill_existing_monitor() {
                Ok(true) => println!("✓ Monitor process stopped successfully"),
                Ok(false) => println!("⚠ No running monitor process found"),
                Err(e) => println!("❌ Error stopping monitor: {}", e),
            }
        }

        Commands::MonitorStatus => {
            if crate::dispatcher::is_monitor_running() {
                println!("✅ Monitor process is running");
                if let Ok(pid_str) = fs::read_to_string(crate::dispatcher::PID_FILE) {
                    println!("   PID: {}", pid_str.trim());
                }
            } else {
                println!("❌ Monitor process is not running");
            }
        }

        Commands::Tui { interval } => {
            crate::tui::run_tui(ctx.cfg, interval).await?;
        }

        Commands::Diff {
            pr_number,
            pr_numbers,
            pr,
            all,
            dry_run,
            json,
            priority,
            repo,
            author,
            since_days,
        } => {
            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo and --author and --since-days filters when using interactive selection or --all
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let targets: Vec<_> = if all {
                // --all flag: use all pending ctx.reviews (already filtered)
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(num) = target_pr {
                // Single PR via --pr or positional
                let prs = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;
                prs
            } else if let Some(ref nums) = pr_numbers {
                // Parse comma-separated PR numbers
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                // Interactive: show list and let user pick (using filtered ctx.reviews)
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, false);
                print!(
                    "\n{} ",
                    "Select PRs to diff [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if targets.is_empty() {
                println!("No PRs found to diff.");
                return Ok(());
            }

            // Dry-run mode: just show what would be diffed
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be diffed:\n");
                for (i, review) in targets.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                    println!("     🔗 {}", review.pr_url.blue());
                }
                println!("\n  Total: {} PR(s)\n", targets.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct DiffOutput<'a> {
                pr_number: u64,
                pr_title: &'a str,
                repo: &'a str,
                author: &'a str,
                branch: &'a str,
                url: &'a str,
                age_days: i64,
                age_category: String,
                size_lines: u64,
                size_category: &'a str,
                additions: u64,
                deletions: u64,
                draft: bool,
                priority_score: u8,
            }

            for review in targets {
                let age_days = (chrono::Utc::now() - review.created_at).num_days();
                let total = review.additions + review.deletions;
                let size_label: &'static str = if total < 50 {
                    "XS"
                } else if total < 200 {
                    "S"
                } else if total < 500 {
                    "M"
                } else if total < 1000 {
                    "L"
                } else {
                    "XL"
                };
                let age_category = if age_days == 0 {
                    "HOT".to_string()
                } else if age_days <= 2 {
                    "FRESH".to_string()
                } else if age_days <= 7 {
                    "WEEK_OLD".to_string()
                } else if age_days <= 14 {
                    "STALE".to_string()
                } else {
                    "OLD".to_string()
                };
                let priority_score = logger::calculate_priority_score(&review);

                if json {
                    let output = DiffOutput {
                        pr_number: review.pr_number,
                        pr_title: &review.pr_title,
                        repo: &review.repo,
                        author: &review.pr_author,
                        branch: &review.branch,
                        url: &review.pr_url,
                        age_days,
                        age_category,
                        size_lines: total,
                        size_category: size_label,
                        additions: review.additions,
                        deletions: review.deletions,
                        draft: review.draft,
                        priority_score,
                    };
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    let age_label = match age_days {
                        0 => "today".green(),
                        1 => "1 day ago".normal(),
                        _ => format!("{} days ago", age_days).red(),
                    };

                    println!("\n{}", "─".repeat(60));
                    println!("📄 {}  #{}", review.pr_title.bold(), review.pr_number);
                    println!(
                        "   👤 {}  •  📅 {}  •  🌿 {}",
                        review.pr_author.cyan(),
                        age_label,
                        review.branch.dimmed()
                    );
                    println!(
                        "   📊 {}  •  +{} additions  •  -{} deletions",
                        if review.draft {
                            "DRAFT".yellow()
                        } else {
                            "READY".green()
                        },
                        review.additions.to_string().green(),
                        review.deletions.to_string().red()
                    );
                    println!("   🔗 {}", review.pr_url.blue().underline());
                    println!("{}", "─".repeat(60));

                    let size_color: colored::Color = match size_label {
                        "XS" | "S" => colored::Color::Green,
                        "M" => colored::Color::Yellow,
                        "L" => colored::Color::Red,
                        _ => colored::Color::Magenta,
                    };
                    println!(
                        "   📦 Size: {} ({} lines)",
                        size_label.color(size_color),
                        total
                    );

                    let age_cat = if age_days == 0 {
                        "🔥 HOT"
                    } else if age_days <= 2 {
                        "⚡ FRESH"
                    } else if age_days <= 7 {
                        "📅 WEEK OLD"
                    } else if age_days <= 14 {
                        "⚠️  STALE"
                    } else {
                        "🚨 OLD"
                    };
                    println!("   ⏱️  Age: {} ({} days)", age_cat, age_days);

                    if priority {
                        println!(
                            "   ⭐ Priority: {}/5  {}",
                            priority_score,
                            logger::priority_stars(priority_score)
                        );
                    }

                    println!("   📁 Repository: {}", review.repo);
                    println!("{}", "─".repeat(60));
                    println!();
                }
            }
        }

        Commands::Info {
            pr_number,
            json,
            priority,
            repo,
            author,
            since_days,
            all,
            pr_numbers,
            pr,
        } => {
            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else {
                Vec::new()
            };

            // Determine base PR list based on targeting mode
            let base_reviews: Vec<github::PendingReview> = if let Some(num) = target_pr {
                // Single PR via --pr or positional
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if pr_numbers.is_some() {
                // Batch mode via --pr-numbers
                prs_from_numbers
            } else if all {
                // --all flag: use all pending ctx.reviews
                ctx.reviews.clone()
            } else {
                // Interactive mode: use filtered ctx.reviews from main fetch
                ctx.reviews.clone()
            };

            // Apply --repo, --author, and --since-days filters to ctx.reviews
            let filtered_reviews: Vec<_> = {
                let mut result = base_reviews;

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // Apply snooze filtering (unless --pr, --pr-numbers, or --all is specified)
            let filtered_reviews: Vec<github::PendingReview> =
                if target_pr.is_none() && pr_numbers.is_none() && !all {
                    let snooze_file = ctx
                        .output_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("./reviews"))
                        .join(".snoozed.json");

                    let now = chrono::Utc::now();
                    let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                        if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                            if let Ok(entries) =
                                serde_json::from_str::<Vec<serde_json::Value>>(&content)
                            {
                                entries
                                    .into_iter()
                                    .filter_map(|e| {
                                        let repo = e.get("repo")?.as_str()?.to_string();
                                        let pr_number = e.get("pr_number")?.as_u64()?;
                                        let until_str = e.get("snoozed_until")?.as_str()?;
                                        if let Ok(until) =
                                            chrono::DateTime::parse_from_rfc3339(until_str)
                                        {
                                            if until.with_timezone(&chrono::Utc) > now {
                                                return Some((repo, pr_number));
                                            }
                                        }
                                        None
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };

                    filtered_reviews
                        .into_iter()
                        .filter(|r| {
                            !snoozed_prs
                                .iter()
                                .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                        })
                        .collect()
                } else {
                    filtered_reviews
                };

            // Handle interactive selection if no targeting flags provided
            let prs: Vec<github::PendingReview> =
                if target_pr.is_none() && pr_numbers.is_none() && !all {
                    if filtered_reviews.is_empty() {
                        println!("No matching reviews found.");
                        return Ok(());
                    }
                    logger::print_reviews(&filtered_reviews, false);
                    print!(
                        "\n{} ",
                        "Select PR to info [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                    );
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    match parse_selection(input.trim(), filtered_reviews.len()) {
                        Selection::Quit => return Ok(()),
                        Selection::Indices(indices) => indices
                            .into_iter()
                            .map(|i| filtered_reviews[i].clone())
                            .collect(),
                    }
                } else {
                    filtered_reviews
                };

            if prs.is_empty() {
                println!("No PR found.");
                return Ok(());
            }

            // Phase 1: Fetch all PR details in parallel
            let detail_futures = prs.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;

                async move { client.pulls(&org, &repo).get(pr_number).await }
            });

            let detail_results: Vec<Result<_, _>> = join_all(detail_futures).await;

            // Phase 2: Process results sequentially (already fetched, just display)
            for (review, result) in prs.into_iter().zip(detail_results) {
                let full_pr = match result {
                    Ok(pr) => pr,
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to fetch details for PR #{}: {}",
                            review.pr_number, e
                        );
                        continue;
                    }
                };

                let body = full_pr
                    .body
                    .clone()
                    .unwrap_or_else(|| "No description provided.".to_string());
                let mut lines: Vec<&str> = body.lines().collect();
                if lines.len() > 50 {
                    lines.truncate(50);
                    lines.push("... (truncated)");
                }
                let body_preview = lines.join("\n");

                let requested_reviewers = full_pr
                    .requested_reviewers
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|r| r.login.clone())
                    .collect::<Vec<_>>();
                let requested_teams = full_pr
                    .requested_teams
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|t| t.slug.clone())
                    .collect::<Vec<_>>();

                let _age_days = (chrono::Utc::now() - review.created_at).num_days();
                let created_str = review.created_at.format("%Y-%m-%d %H:%M UTC").to_string();
                let updated_str = full_pr
                    .updated_at
                    .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "Unknown".to_string());

                let info = serde_json::json!({
                    "number": review.pr_number,
                    "title": review.pr_title,
                    "author": review.pr_author,
                    "body": body,
                    "repo": review.repo,
                    "url": review.pr_url,
                    "branch": review.branch,
                    "state": if review.draft { "draft" } else { "open" },
                    "created_at": created_str,
                    "updated_at": updated_str,
                    "additions": review.additions,
                    "deletions": review.deletions,
                    "requested_reviewers": requested_reviewers,
                    "requested_teams": requested_teams,
                    "labels": full_pr.labels.as_ref().map(|l| l.iter().map(|label| label.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                    "assignees": full_pr.assignees.as_ref().map(|a| a.iter().map(|u| u.login.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                    "priority_score": logger::calculate_priority_score(&review),
                });

                if json {
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    println!("\n{}", "═".repeat(60));
                    println!("📋 PR #{} — {}", review.pr_number, review.pr_title.bold());
                    println!("{}", "═".repeat(60));
                    println!();
                    println!("  👤 Author:     {}", review.pr_author.cyan());
                    println!("  📅 Created:   {}", created_str);
                    println!("  🔄 Updated:   {}", updated_str);
                    println!("  🌿 Branch:    {}", review.branch.dimmed());
                    println!(
                        "  📊 State:     {}",
                        if review.draft {
                            "DRAFT".yellow()
                        } else {
                            "OPEN".green()
                        }
                    );
                    println!("  📁 Repository: {}", review.repo);
                    if priority {
                        let score = logger::calculate_priority_score(&review);
                        println!(
                            "  ⭐ Priority:  {}/5  {}",
                            score,
                            logger::priority_stars(score)
                        );
                    }
                    println!();
                    println!("  👥 Requested Reviewers:");
                    if requested_reviewers.is_empty() && requested_teams.is_empty() {
                        println!("     (none)");
                    } else {
                        for reviewer in &requested_reviewers {
                            println!("     @{}", reviewer.cyan());
                        }
                        for team in &requested_teams {
                            println!("     @{}", team.yellow());
                        }
                    }
                    println!();
                    println!("  🏷️  Labels:");
                    let labels = full_pr
                        .labels
                        .as_ref()
                        .map(|l| l.iter().map(|label| label.name.clone()).collect::<Vec<_>>())
                        .unwrap_or_default();
                    if labels.is_empty() {
                        println!("     (none)");
                    } else {
                        for label in &labels {
                            println!("     • {}", label);
                        }
                    }
                    println!();
                    println!("  📝 Description:");
                    println!("{}", "─".repeat(60));
                    for line in body_preview.lines() {
                        println!("  {}", line);
                    }
                    println!("{}", "─".repeat(60));
                    println!();
                    println!("  🔗 {}", review.pr_url.blue().underline());
                    println!("{}", "═".repeat(60));
                    println!();
                }
            }
        }

        Commands::Timeline {
            pr_number,
            json,
            pr,
            pr_numbers,
            all,
            dry_run,
            repo,
            author,
            priority,
            since_days,
        } => {
            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                Vec::new()
            };

            // If --pr-numbers was provided, use those PRs directly
            if pr_numbers.is_some() {
                if prs_from_numbers.is_empty() {
                    println!("No PRs found for the specified numbers.");
                    return Ok(());
                }

                // --dry-run: preview which PRs would be shown
                if dry_run {
                    println!("\n📜 {} PR(s) would be shown:\n", prs_from_numbers.len());
                    for (i, pr) in prs_from_numbers.iter().enumerate() {
                        let priority_display = if priority {
                            let score = logger::calculate_priority_score(pr);
                            format!("  ⭐ {}/5", score)
                        } else {
                            String::new()
                        };
                        println!(
                            "  {}  {}#{} — {}{}",
                            i + 1,
                            pr.repo.dimmed(),
                            pr.pr_number,
                            pr.pr_title.bold(),
                            priority_display
                        );
                    }
                    println!();
                    return Ok(());
                }

                // Fetch all timelines in parallel
                let futures = prs_from_numbers.iter().map(|review| {
                    github::fetch_pr_timeline(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &review.repo,
                        review.pr_number,
                    )
                });
                let timeline_results: Vec<Result<Vec<github::TimelineEvent>, anyhow::Error>> =
                    futures::future::join_all(futures).await;
                let total_prs = prs_from_numbers.len();

                for (i, (review, timeline_result)) in prs_from_numbers
                    .iter()
                    .zip(timeline_results)
                    .enumerate()
                {
                    let timeline = match timeline_result {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!(
                                "Warning: Failed to fetch timeline for PR #{}: {}",
                                review.pr_number, e
                            );
                            continue;
                        }
                    };
                    display_timeline(review, &timeline, json, priority, total_prs, i)?;
                }
                return Ok(());
            }

            // When --pr is specified, bypass filters and fetch directly
            let prs = if let Some(pr_num) = target_pr {
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    pr_num,
                )
                .await?
            } else {
                // Apply --repo and --author filters to ctx.reviews, then let user pick
                let mut filtered = ctx.reviews.clone();

                // Apply --since-days filter (before repo/author filters)
                if let Some(since) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(since as i64);
                    filtered.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    filtered.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    filtered.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }

                // If --all flag is set, show all without prompting
                if all {
                    // --dry-run: preview which PRs would be shown
                    if dry_run {
                        println!("\n📜 {} PR(s) would be shown:\n", filtered.len());
                        for (i, pr) in filtered.iter().enumerate() {
                            let priority_display = if priority {
                                let score = logger::calculate_priority_score(pr);
                                format!("  ⭐ {}/5", score)
                            } else {
                                String::new()
                            };
                            println!(
                                "  {}  {}#{} — {}{}",
                                i + 1,
                                pr.repo.dimmed(),
                                pr.pr_number,
                                pr.pr_title.bold(),
                                priority_display
                            );
                        }
                        println!();
                        return Ok(());
                    }

                    // Fetch all timelines in parallel
                    let futures = filtered.iter().map(|review| {
                        github::fetch_pr_timeline(
                            &ctx.cfg.github_token,
                            &ctx.cfg.github_org,
                            &review.repo,
                            review.pr_number,
                        )
                    });
                    let timeline_results: Vec<Result<Vec<github::TimelineEvent>, anyhow::Error>> =
                        futures::future::join_all(futures).await;
                    let total_prs = filtered.len();

                    for (i, (review, timeline_result)) in filtered
                        .iter()
                        .zip(timeline_results)
                        .enumerate()
                    {
                        let timeline = match timeline_result {
                            Ok(t) => t,
                            Err(e) => {
                                eprintln!(
                                    "Warning: Failed to fetch timeline for PR #{}: {}",
                                    review.pr_number, e
                                );
                                continue;
                            }
                        };
                        display_timeline(review, &timeline, json, priority, total_prs, i)?;
                    }
                    return Ok(());
                }

                logger::print_reviews(&filtered, false);
                print!(
                    "\n{} ",
                    "Select PR to show timeline [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => {
                        indices.into_iter().map(|i| filtered[i].clone()).collect()
                    }
                }
            };

            if prs.is_empty() {
                println!("No PR found.");
                return Ok(());
            }

            // --dry-run: preview which PRs would be shown
            if dry_run {
                println!("\n📜 {} PR(s) would be shown:\n", prs.len());
                for (i, pr) in prs.iter().enumerate() {
                    let priority_display = if priority {
                        let score = logger::calculate_priority_score(pr);
                        format!("  ⭐ {}/5", score)
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}  {}#{} — {}{}",
                        i + 1,
                        pr.repo.dimmed(),
                        pr.pr_number,
                        pr.pr_title.bold(),
                        priority_display
                    );
                }
                println!();
                return Ok(());
            }

            // Fetch all timelines in parallel
            let futures = prs.iter().map(|review| {
                github::fetch_pr_timeline(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &review.repo,
                    review.pr_number,
                )
            });
            let timeline_results: Vec<Result<Vec<github::TimelineEvent>, anyhow::Error>> =
                futures::future::join_all(futures).await;
            let total_prs = prs.len();

            for (i, (review, timeline_result)) in
                prs.iter().zip(timeline_results).enumerate()
            {
                let timeline = match timeline_result {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to fetch timeline for PR #{}: {}",
                            review.pr_number, e
                        );
                        continue;
                    }
                };
                display_timeline(review, &timeline, json, priority, total_prs, i)?;
            }
        }

        Commands::Browse {
            pr_number,
            pr_numbers,
            pr,
            all,
            dry_run,
            quiet,
            json,
            priority,
            repo,
            author,
            since_days,
        } => {
            // In JSON mode, also suppress per-PR output
            let quiet = quiet || json;
            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo and --author and --since-days filters when using interactive selection or --all
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // Filter out snoozed PRs (unless --pr or --pr-numbers is specified, consistent with list/delegate)
            let filtered_reviews: Vec<_> = if target_pr.is_none() {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                filtered_reviews
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            } else {
                filtered_reviews
            };

            let targets: Vec<_> = if all {
                // --all flag: use all pending ctx.reviews (already filtered)
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(num) = target_pr {
                // Single PR via --pr or positional
                let prs = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;
                prs
            } else if let Some(ref nums) = pr_numbers {
                // Parse comma-separated PR numbers
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                let all_prs_len = all_prs.len();

                // Apply --repo and --author filters to fetched PRs (consistent with --all and interactive)
                let filtered: Vec<_> = if all_prs.is_empty() {
                    Vec::new()
                } else {
                    all_prs
                        .into_iter()
                        .filter(|r| {
                            let mut matches = true;
                            if let Some(ref repo_filter) = repo {
                                let pattern = repo_filter.to_lowercase();
                                matches = matches && r.repo.to_lowercase().contains(&pattern);
                            }
                            if let Some(ref author_filter) = author {
                                let pattern = author_filter.to_lowercase();
                                matches = matches && r.pr_author.to_lowercase().contains(&pattern);
                            }
                            matches
                        })
                        .collect()
                };

                if filtered.is_empty() && all_prs_len > 0 {
                    println!("❌ No matching reviews found among specified PRs.");
                    return Ok(());
                }
                filtered
            } else {
                // Interactive: show list and let user pick (using filtered ctx.reviews)
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, false);
                print!(
                    "\n{} ",
                    "Select PRs to open [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if targets.is_empty() {
                println!("No PRs found to open.");
                return Ok(());
            }

            // Dry-run mode: just show what would be opened
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be opened:\n");
                for (i, review) in targets.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                    println!("     🔗 {}", review.pr_url.blue());
                }
                println!("\n  Total: {} PR(s)\n", targets.len());
                return Ok(());
            }

            // JSON mode: output URLs without opening browser
            if json {
                #[derive(serde::Serialize)]
                struct BrowseOutput<'a> {
                    pr_number: u64,
                    pr_title: &'a str,
                    repo: &'a str,
                    url: &'a str,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    priority_score: Option<u8>,
                }
                let output: Vec<BrowseOutput> = targets
                    .iter()
                    .map(|r| BrowseOutput {
                        pr_number: r.pr_number,
                        pr_title: &r.pr_title,
                        repo: &r.repo,
                        url: &r.pr_url,
                        priority_score: if priority {
                            Some(logger::calculate_priority_score(r))
                        } else {
                            None
                        },
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                );
                return Ok(());
            }

            if !quiet {
                println!("\n🌐 Opening {} PR(s) in browser...\n", targets.len());
            }
            let mut success_count = 0usize;
            let mut fail_count = 0usize;
            for review in &targets {
                let priority_label = if priority {
                    let score = logger::calculate_priority_score(review);
                    format!(" {} ", logger::priority_stars(score))
                } else {
                    String::new()
                };
                match open::that(&review.pr_url) {
                    Ok(_) => {
                        success_count += 1;
                        if !quiet {
                            println!(
                                "  ✅ {}{}({})",
                                review.pr_title.dimmed(),
                                priority_label,
                                review.pr_url.cyan()
                            );
                        }
                    }
                    Err(e) => {
                        fail_count += 1;
                        if !quiet {
                            println!(
                                "  ❌ Failed to open {}{}: {}",
                                review.pr_title.dimmed(),
                                priority_label,
                                e
                            );
                        }
                    }
                }
            }
            if !quiet {
                println!();
            } else {
                // In quiet mode, just show summary
                println!(
                    "Opened {} PR(s) ({} success, {} failed)",
                    targets.len(),
                    success_count,
                    fail_count
                );
            }
        }

        Commands::Assign {
            all,
            pr_numbers,
            pr_number,
            since_days,
            dry_run,
            json,
            quiet,
            repo,
            author,
            priority,
        } => {
            let target_pr = ctx.cli.pr.or(pr_number);

            // Apply --repo and --author filters (consistent with other batch commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let prs: Vec<github::PendingReview> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews.clone()
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, priority);
                print!(
                    "\n{} ",
                    "Select PRs to assign yourself [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if prs.is_empty() {
                println!("No PR found to assign.");
                return Ok(());
            }

            // Dry-run mode: just show what would be assigned
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be assigned:\n");
                for (i, review) in prs.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                }
                println!("\n  Total: {} PR(s)\n", prs.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct AssignResult<'a> {
                pr_number: u64,
                pr_title: &'a str,
                repo: &'a str,
                url: &'a str,
                success: bool,
                error: Option<String>,
            }

            let mut results: Vec<AssignResult> = Vec::new();

            // Parallelize assign requests
            let assign_futures = prs.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;
                let username = ctx.cfg.github_username.clone();

                async move {
                    client
                        .pulls(&org, &repo)
                        .request_reviews(pr_number, vec![username], Vec::<String>::new())
                        .await
                }
            });

            let assign_results: Vec<Result<_, _>> = join_all(assign_futures).await;

            for (review, result) in prs.iter().zip(assign_results) {
                if !json && !quiet {
                    print!(
                        "\n⏳ Requesting review on #{} {}... ",
                        review.pr_number, review.pr_title
                    );
                    io::stdout().flush()?;
                }

                match result {
                    Ok(_) => {
                        if json {
                            results.push(AssignResult {
                                pr_number: review.pr_number,
                                pr_title: &review.pr_title,
                                repo: &review.repo,
                                url: &review.pr_url,
                                success: true,
                                error: None,
                            });
                        } else if !quiet {
                            println!("{}", "✅ Assigned".green());
                            println!(
                                "   👤 You're now a reviewer on {} ({})",
                                review.pr_title, review.repo
                            );
                            println!("   🔗 {}", review.pr_url.blue().underline());
                        }
                    }
                    Err(e) => {
                        if json {
                            results.push(AssignResult {
                                pr_number: review.pr_number,
                                pr_title: &review.pr_title,
                                repo: &review.repo,
                                url: &review.pr_url,
                                success: false,
                                error: Some(e.to_string()),
                            });
                        } else if !quiet {
                            println!("{}", "❌ Failed".red());
                            println!("   Error: {}", e);
                        }
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            }

            println!();
        }

        Commands::Unassign {
            all,
            pr_numbers,
            pr_number,
            since_days,
            dry_run,
            json,
            quiet,
            repo,
            author,
            priority,
        } => {
            let target_pr = ctx.cli.pr.or(pr_number);

            // Apply --repo and --author filters (consistent with other batch commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let prs: Vec<github::PendingReview> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews.clone()
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, priority);
                print!(
                    "\n{} ",
                    "Select PRs to unassign yourself [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if prs.is_empty() {
                println!("No PR found to unassign.");
                return Ok(());
            }

            // Dry-run mode: just show what would be unassigned
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be unassigned:\n");
                for (i, review) in prs.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                }
                println!("\n  Total: {} PR(s)\n", prs.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct UnassignResult<'a> {
                pr_number: u64,
                pr_title: &'a str,
                repo: &'a str,
                url: &'a str,
                success: bool,
                error: Option<String>,
            }

            let mut results: Vec<UnassignResult> = Vec::new();

            // Parallelize unassign requests
            let unassign_futures = prs.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;
                let username = ctx.cfg.github_username.clone();

                async move {
                    client
                        .pulls(&org, &repo)
                        .request_reviews(pr_number, Vec::<String>::new(), vec![username])
                        .await
                }
            });

            let unassign_results: Vec<Result<_, _>> = join_all(unassign_futures).await;

            for (review, result) in prs.iter().zip(unassign_results) {
                if !json && !quiet {
                    print!(
                        "\n⏳ Removing yourself from review on #{} {}... ",
                        review.pr_number, review.pr_title
                    );
                    io::stdout().flush()?;
                }

                match result {
                    Ok(_) => {
                        if json {
                            results.push(UnassignResult {
                                pr_number: review.pr_number,
                                pr_title: &review.pr_title,
                                repo: &review.repo,
                                url: &review.pr_url,
                                success: true,
                                error: None,
                            });
                        } else if !quiet {
                            println!("{}", "✅ Unassigned".green());
                            println!(
                                "   👤 You're no longer a reviewer on {} ({})",
                                review.pr_title, review.repo
                            );
                            println!("   🔗 {}", review.pr_url.blue().underline());
                        }
                    }
                    Err(e) => {
                        if json {
                            results.push(UnassignResult {
                                pr_number: review.pr_number,
                                pr_title: &review.pr_title,
                                repo: &review.repo,
                                url: &review.pr_url,
                                success: false,
                                error: Some(e.to_string()),
                            });
                        } else if !quiet {
                            println!("{}", "❌ Failed".red());
                            println!("   Error: {}", e);
                        }
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            }

            println!();
        }

        Commands::Comment {
            all,
            pr_numbers,
            pr_number,
            text,
            since_days,
            dry_run,
            json,
            quiet,
            repo,
            author,
            priority,
        } => {
            let target_pr = ctx.cli.pr.or(pr_number);

            // Apply --repo, --author, and --since-days filters (consistent with other batch commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let prs: Vec<github::PendingReview> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews.clone()
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, priority);
                print!(
                    "\n{} ",
                    "Select PRs to comment on [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if prs.is_empty() {
                println!("No PR found to comment on.");
                return Ok(());
            }

            // Dry-run mode: just show what would be commented
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would receive a comment:\n");
                for (i, review) in prs.iter().enumerate() {
                    println!(
                        "  {}. #{} {}  ({})",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan()
                    );
                }
                println!("\n  Comment text: \"{}\"", text.yellow());
                println!("\n  Total: {} PR(s)\n", prs.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct CommentResult {
                pr_number: u64,
                pr_title: String,
                repo: String,
                url: String,
                success: bool,
                error: Option<String>,
            }

            let mut results: Vec<CommentResult> = Vec::new();
            let comment_text = text.clone(); // Clone for use in async blocks

            // Parallelize comment requests
            let comment_futures = prs.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;
                let text_clone = comment_text.clone();

                async move {
                    client
                        .issues(&org, &repo)
                        .create_comment(pr_number, &text_clone)
                        .await
                }
            });

            let comment_results: Vec<Result<_, _>> = join_all(comment_futures).await;

            for (review, result) in prs.iter().zip(comment_results) {
                if !json && !quiet {
                    print!(
                        "\n💬 Posting comment on #{} {}... ",
                        review.pr_number, review.pr_title
                    );
                    io::stdout().flush()?;
                }

                match result {
                    Ok(_) => {
                        if json {
                            results.push(CommentResult {
                                pr_number: review.pr_number,
                                pr_title: review.pr_title.clone(),
                                repo: review.repo.clone(),
                                url: review.pr_url.clone(),
                                success: true,
                                error: None,
                            });
                        } else if !quiet {
                            println!("{}", "✅ Commented".green());
                            println!("   📝 {} ({})", review.pr_title, review.repo);
                            println!("   💬 \"{}\"", text.yellow());
                            println!("   🔗 {}", review.pr_url.blue().underline());
                        }
                    }
                    Err(e) => {
                        if json {
                            results.push(CommentResult {
                                pr_number: review.pr_number,
                                pr_title: review.pr_title.clone(),
                                repo: review.repo.clone(),
                                url: review.pr_url.clone(),
                                success: false,
                                error: Some(e.to_string()),
                            });
                        } else if !quiet {
                            println!("{}", "❌ Failed".red());
                            println!("   Error: {}", e);
                        }
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            }

            println!();
        }

        Commands::Approve {
            all,
            pr_numbers,
            pr_number,
            message,
            since_days,
            dry_run,
            json,
            priority,
            quiet,
            repo,
            author,
        } => {
            let target_pr = ctx.cli.pr.or(pr_number);

            // Apply --repo and --author filters (consistent with other commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Filter out snoozed PRs (unless --pr is specified, consistent with list/delegate/search)
                if target_pr.is_none() {
                    let snooze_file = ctx
                        .output_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("./reviews"))
                        .join(".snoozed.json");

                    let now = chrono::Utc::now();
                    let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                        if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                            if let Ok(entries) =
                                serde_json::from_str::<Vec<serde_json::Value>>(&content)
                            {
                                entries
                                    .into_iter()
                                    .filter_map(|e| {
                                        let repo = e.get("repo")?.as_str()?.to_string();
                                        let pr_number = e.get("pr_number")?.as_u64()?;
                                        let until_str = e.get("snoozed_until")?.as_str()?;
                                        if let Ok(until) =
                                            chrono::DateTime::parse_from_rfc3339(until_str)
                                        {
                                            if until.with_timezone(&chrono::Utc) > now {
                                                return Some((repo, pr_number));
                                            }
                                        }
                                        None
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };

                    result
                        .into_iter()
                        .filter(|r| {
                            !snoozed_prs
                                .iter()
                                .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                        })
                        .collect()
                } else {
                    result
                }
            };

            let prs: Vec<github::PendingReview> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews.clone()
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else if let Some(num) = target_pr {
                let fetched = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;

                // If PR exists in multiple repos, ask user to choose
                if fetched.len() > 1 {
                    println!("\n📋 PR #{} found in multiple repos:\n", num);
                    for (i, pr) in fetched.iter().enumerate() {
                        println!(
                            "  {}. {} / #{} {}",
                            i + 1,
                            pr.repo.cyan(),
                            pr.pr_number,
                            pr.pr_title.bold()
                        );
                    }
                    print!("\n{} ", "Select repo (q to quit):".bold());
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;

                    if input.trim().to_lowercase() == "q" {
                        return Ok(());
                    }

                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx > 0 && idx <= fetched.len() {
                            vec![fetched[idx - 1].clone()]
                        } else {
                            println!("Invalid selection.");
                            return Ok(());
                        }
                    } else {
                        println!("Invalid input.");
                        return Ok(());
                    }
                } else {
                    fetched
                }
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, priority);
                print!(
                    "\n{} ",
                    "Select PRs to approve [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if prs.is_empty() {
                println!("No PR found to approve.");
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct ApproveResult {
                pr_number: u64,
                pr_title: String,
                repo: String,
                url: String,
                success: bool,
                error: Option<String>,
            }

            let mut results: Vec<ApproveResult> = Vec::new();
            let approval_message = message.clone().unwrap_or_else(|| "LGTM!".to_string());

            // Show what will be approved and ask for confirmation
            if !dry_run {
                println!("\n🔍 About to approve the following PRs:\n");
                for (i, review) in prs.iter().enumerate() {
                    println!(
                        "  {}. #{} {}  ({})",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan()
                    );
                }
                println!("\n  Total: {} PR(s)\n", prs.len());

                print!("{} ", "Continue with approval? (y/n):".yellow().bold());
                io::stdout().flush()?;
                let mut confirm = String::new();
                io::stdin().read_line(&mut confirm)?;
                if !confirm.trim().to_lowercase().starts_with('y') {
                    println!("\n❌ Approval cancelled.\n");
                    return Ok(());
                }
                println!();
            } else {
                println!("\n🔍 Dry-run mode — the following PRs would be approved:\n");
                for (i, review) in prs.iter().enumerate() {
                    println!(
                        "  {}. #{} {}  ({})",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan()
                    );
                }
                println!("\n  Total: {} PR(s)\n", prs.len());
                return Ok(());
            }

            // Phase 1: Fetch PR details (to get commit SHAs) in parallel
            let detail_futures = prs.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;

                async move { client.pulls(&org, &repo).get(pr_number).await }
            });

            let detail_results: Vec<Result<_, _>> = join_all(detail_futures).await;

            // Phase 2: Approve all PRs in parallel using the fetched commit SHAs
            let approve_futures =
                prs.iter()
                    .zip(detail_results.iter())
                    .filter_map(|(review, detail_result)| {
                        let commit_id = match detail_result {
                            Ok(pr) => pr.head.sha.clone(),
                            Err(_) => return None,
                        };

                        let client = octocrab::Octocrab::builder()
                            .personal_token(ctx.cfg.github_token.clone())
                            .build()
                            .expect("failed to build GitHub client");
                        let org = ctx.cfg.github_org.clone();
                        let repo = review.repo.clone();
                        let pr_number = review.pr_number;
                        let msg = approval_message.clone();

                        Some(async move {
                            #[allow(deprecated)]
                            client
                                .pulls(&org, &repo)
                                .pull_number(pr_number)
                                .reviews()
                                .create_review(
                                    commit_id,
                                    msg,
                                    octocrab::models::pulls::ReviewAction::Approve,
                                    Vec::new(),
                                )
                                .await
                        })
                    });

            let approve_results: Vec<Result<_, _>> = join_all(approve_futures).await;

            // Process results, matching them with failed detail fetches
            let mut approve_iter = approve_results.into_iter();
            for (review, detail_result) in prs.iter().zip(detail_results.iter()) {
                if !json && !quiet {
                    print!(
                        "\n⏳ Approving #{} {}... ",
                        review.pr_number, review.pr_title
                    );
                    io::stdout().flush()?;
                }

                match detail_result {
                    Err(e) => {
                        if json {
                            results.push(ApproveResult {
                                pr_number: review.pr_number,
                                pr_title: review.pr_title.clone(),
                                repo: review.repo.clone(),
                                url: review.pr_url.clone(),
                                success: false,
                                error: Some(format!("Failed to get PR details: {}", e)),
                            });
                        } else if !quiet {
                            println!("{}", "❌ Failed".red());
                            println!("   Error getting PR details: {}", e);
                        }
                    }
                    Ok(_) => {
                        // Now get the approval result
                        match approve_iter.next() {
                            Some(Ok(_)) => {
                                if json {
                                    results.push(ApproveResult {
                                        pr_number: review.pr_number,
                                        pr_title: review.pr_title.clone(),
                                        repo: review.repo.clone(),
                                        url: review.pr_url.clone(),
                                        success: true,
                                        error: None,
                                    });
                                } else if !quiet {
                                    let priority_display = if priority {
                                        let score = logger::calculate_priority_score(review);
                                        format!("  ⭐ {}/5  ", score)
                                    } else {
                                        String::new()
                                    };
                                    println!("{}", "✅ Approved".green());
                                    println!(
                                        "   👍 You approved {} ({})",
                                        review.pr_title, review.repo
                                    );
                                    if priority {
                                        println!(
                                            "   {}🔗 {}",
                                            priority_display,
                                            review.pr_url.blue().underline()
                                        );
                                    } else {
                                        println!("   🔗 {}", review.pr_url.blue().underline());
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                if json {
                                    results.push(ApproveResult {
                                        pr_number: review.pr_number,
                                        pr_title: review.pr_title.clone(),
                                        repo: review.repo.clone(),
                                        url: review.pr_url.clone(),
                                        success: false,
                                        error: Some(e.to_string()),
                                    });
                                } else if !quiet {
                                    println!("{}", "❌ Failed".red());
                                    println!("   Error: {}", e);
                                }
                            }
                            None => {
                                // This shouldn't happen but handle gracefully
                                if json {
                                    results.push(ApproveResult {
                                        pr_number: review.pr_number,
                                        pr_title: review.pr_title.clone(),
                                        repo: review.repo.clone(),
                                        url: review.pr_url.clone(),
                                        success: false,
                                        error: Some(
                                            "Internal error: missing approval result".to_string(),
                                        ),
                                    });
                                } else if !quiet {
                                    println!("{}", "❌ Failed".red());
                                    println!("   Internal error",);
                                }
                            }
                        }
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            }

            println!();
        }

        Commands::Claim {
            all,
            pr_numbers,
            since_days,
            dry_run,
            priority,
            repo,
            author,
            json,
            quiet,
        } => {
            // Apply filters to ctx.reviews (consistent with list/delegate commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let targets: Vec<_> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews.into_iter().collect()
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = futures::future::join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, priority);
                print!(
                    "\n{} ",
                    "Select PRs to claim [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if targets.is_empty() {
                println!("No PRs to claim.");
                return Ok(());
            }

            // Dry-run mode: just show what would be claimed
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be claimed:\n");
                for (i, review) in targets.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}. #{} {}  ({}){}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        priority_label
                    );
                }
                println!("\n  Total: {} PR(s)\n", targets.len());
                return Ok(());
            }

            #[derive(serde::Serialize)]
            struct ClaimResult {
                pr_number: u64,
                pr_title: String,
                repo: String,
                url: String,
                success: bool,
                error: Option<String>,
            }

            let mut results: Vec<ClaimResult> = Vec::new();

            if !json && !quiet {
                println!(
                    "\n🎯 Claiming {} PR(s) for review...\n",
                    targets.len().to_string().yellow().bold()
                );
            }

            // Parallelize claim requests
            let claim_futures = targets.iter().map(|review| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;
                let username = ctx.cfg.github_username.clone();

                async move {
                    client
                        .pulls(&org, &repo)
                        .request_reviews(pr_number, vec![username], Vec::<String>::new())
                        .await
                }
            });

            let claim_results: Vec<Result<_, _>> = join_all(claim_futures).await;

            let total = targets.len();
            for (review, result) in targets.into_iter().zip(claim_results) {
                match result {
                    Ok(_) => {
                        if !json && !quiet {
                            println!(
                                "  {} #{} {}",
                                "✅".green(),
                                review.pr_number,
                                review.pr_title.dimmed()
                            );
                        }
                        results.push(ClaimResult {
                            pr_number: review.pr_number,
                            pr_title: review.pr_title,
                            repo: review.repo,
                            url: review.pr_url,
                            success: true,
                            error: None,
                        });
                    }
                    Err(e) => {
                        if !json && !quiet {
                            println!(
                                "  {} #{} {}  ❌ {}",
                                "❌".red(),
                                review.pr_number,
                                review.pr_title.dimmed(),
                                e.to_string().dimmed()
                            );
                        }
                        results.push(ClaimResult {
                            pr_number: review.pr_number,
                            pr_title: review.pr_title,
                            repo: review.repo,
                            url: review.pr_url,
                            success: false,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                let success_count = results.iter().filter(|r| r.success).count();
                let fail_count = results.len() - success_count;
                println!();
                println!(
                    "📊 Claimed {}/{} PRs",
                    success_count.to_string().green(),
                    total.to_string().yellow()
                );
                if fail_count > 0 {
                    println!(
                        "⚠️  {} PR(s) failed - may already be assigned or inaccessible",
                        fail_count.to_string().red()
                    );
                }
                println!();
            }
        }

        Commands::Files {
            pr_number,
            pr_numbers,
            pr,
            all,
            priority,
            json,
            repo,
            author,
            since_days,
        } => {
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo, --author, and --since-days filters when using interactive selection or --all
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // Filter out snoozed PRs (unless --pr is specified, consistent with list/delegate/search/approve/top)
            let filtered: Vec<_> = if target_pr.is_none() {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                filtered_reviews
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            } else {
                filtered_reviews
            };

            let targets: Vec<_> = if all {
                // Show files for all pending ctx.reviews (already filtered)
                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered
            } else if let Some(num) = target_pr {
                // Single PR via --pr or positional
                let prs = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;
                prs
            } else if let Some(ref nums) = pr_numbers {
                // Parse comma-separated PR numbers
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let mut all_prs = Vec::new();
                for prs in join_all(fetch_futures).await.into_iter().flatten() {
                    all_prs.extend(prs);
                }
                all_prs
            } else {
                // Interactive: show list and let user pick (using filtered ctx.reviews)
                if filtered.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered, false);
                print!(
                    "\n{} ",
                    "Select PRs to show files [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => {
                        indices.into_iter().map(|i| filtered[i].clone()).collect()
                    }
                }
            };

            if targets.is_empty() {
                println!("No PRs found.");
                return Ok(());
            }

            // Fetch files for all PRs in parallel
            let file_futures = targets.iter().map(|review| {
                github::fetch_pr_files(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &review.repo,
                    review.pr_number,
                )
            });
            let file_results: Vec<(
                github::PendingReview,
                Result<Vec<github::PullRequestFile>, anyhow::Error>,
            )> = targets
                .iter()
                .cloned()
                .zip(join_all(file_futures).await)
                .collect();

            // Process and display results
            #[derive(serde::Serialize)]
            struct FilesOutput {
                pr_number: u64,
                pr_title: String,
                repo: String,
                url: String,
                total_files: usize,
                total_additions: u64,
                total_deletions: u64,
                files: Vec<github::PullRequestFile>,
            }

            let mut outputs: Vec<FilesOutput> = Vec::new();

            for (review, result) in file_results {
                match result {
                    Ok(files) => {
                        let total_additions: u64 = files.iter().map(|f| f.additions).sum();
                        let total_deletions: u64 = files.iter().map(|f| f.deletions).sum();

                        if json {
                            outputs.push(FilesOutput {
                                pr_number: review.pr_number,
                                pr_title: review.pr_title,
                                repo: review.repo,
                                url: review.pr_url,
                                total_files: files.len(),
                                total_additions,
                                total_deletions,
                                files,
                            });
                        } else {
                            let priority_display = if priority {
                                let score = logger::calculate_priority_score(&review);
                                format!("  ⭐ {}/5", score)
                            } else {
                                String::new()
                            };

                            println!("\n{}", "─".repeat(60));
                            println!(
                                "📄 {}  #{}{}{}",
                                review.pr_title.bold(),
                                review.pr_number,
                                priority_display,
                                format!("  ({} files)", files.len()).dimmed()
                            );
                            println!("{}", "─".repeat(60));

                            if files.is_empty() {
                                println!("  (no file changes)");
                            } else {
                                for file in &files {
                                    let status_icon = match file.status.as_str() {
                                        "added" => "+".green(),
                                        "removed" => "-".red(),
                                        "modified" => "M".yellow(),
                                        "renamed" => "R".cyan(),
                                        _ => "?".normal(),
                                    };
                                    let total = file.additions + file.deletions;
                                    let size_indicator = if total > 500 {
                                        format!(" ({} lines)", total).red()
                                    } else if total > 200 {
                                        format!(" ({} lines)", total).yellow()
                                    } else {
                                        format!(" ({} lines)", total).dimmed()
                                    };
                                    println!(
                                        "  {}  {}{}",
                                        status_icon,
                                        file.filename.dimmed(),
                                        size_indicator
                                    );
                                }
                            }
                            println!("{}", "─".repeat(60));
                        }
                    }
                    Err(e) => {
                        if json {
                            // Skip failed PRs in JSON mode (consistent with other commands)
                        } else {
                            println!(
                                "\n❌ Failed to fetch files for #{} {}: {}",
                                review.pr_number, review.pr_title, e
                            );
                        }
                    }
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&outputs)?);
            }

            println!();
        }

        Commands::Search {
            query,
            pr_numbers,
            pr_number,
            since_days,
            sort_by,
            priority,
            json,
            repo,
            author,
        } => {
            // Apply --pr filter first (targets specific PR even in search)
            // Priority: global --pr flag > local --pr_number > --pr-numbers
            let target_pr = ctx.cli.pr.or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else {
                Vec::new()
            };

            // Determine base PR list based on targeting mode
            let base_reviews: Vec<github::PendingReview> = if let Some(num) = target_pr {
                // Single PR via --pr or --pr-number
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if pr_numbers.is_some() {
                // Batch mode via --pr-numbers
                prs_from_numbers
            } else {
                // Default: use all ctx.reviews for search
                ctx.reviews.clone()
            };

            let filtered: Vec<_> = base_reviews;

            let query_lower = query.to_lowercase();

            // Apply --since-days filter first (consistent with other commands)
            let filtered: Vec<_> = match since_days {
                Some(days) => {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    filtered
                        .into_iter()
                        .filter(|r| r.created_at >= cutoff)
                        .collect()
                }
                None => filtered,
            };

            // Filter by title (query is required)
            let filtered: Vec<_> = filtered
                .into_iter()
                .filter(|r| r.pr_title.to_lowercase().contains(&query_lower))
                .collect();

            // Apply --repo filter (partial match, case-insensitive)
            let filtered: Vec<_> = match repo {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.repo.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Apply --author filter (partial match, case-insensitive)
            let filtered: Vec<_> = match author {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.pr_author.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Filter out snoozed PRs (unless --pr is specified)
            let filtered: Vec<_> = if target_pr.is_none() {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                filtered
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            } else {
                filtered
            };

            if filtered.is_empty() {
                println!("\n🔍 No reviews matching '{}' found.\n", query.yellow());
                return Ok(());
            }

            // Sort results based on --sort-by option (default: priority)
            let sort_field = sort_by.as_deref().unwrap_or("priority");
            let now = chrono::Utc::now();

            let mut scored: Vec<_> = filtered
                .iter()
                .map(|r| {
                    let score = match sort_field {
                        "age" => {
                            // Older PRs first (higher score = older = higher urgency)
                            (now - r.created_at).num_days()
                        }
                        "size" => {
                            // Larger PRs first
                            r.additions as i64 + r.deletions as i64
                        }
                        "title" => {
                            // Alphabetical by title
                            let title_lower = r.pr_title.to_lowercase();
                            title_lower
                                .bytes()
                                .fold(0i64, |acc, b| acc * 256 + b as i64)
                        }
                        _ => {
                            // "priority" (default) - use priority score
                            logger::calculate_priority_score(r) as i64
                        }
                    };
                    (r.clone(), score)
                })
                .collect();

            // Always sort descending (highest priority/age/size first)
            scored.sort_by(|a, b| b.1.cmp(&a.1));

            let sorted: Vec<_> = scored.into_iter().map(|(r, _)| r).collect();

            if json {
                let json_output = serde_json::to_string_pretty(&sorted)?;
                println!("{}", json_output);
            } else {
                println!(
                    "\n🔍 {} review(s) matching '{}' (sorted by {})\n",
                    sorted.len().to_string().yellow().bold(),
                    query.yellow().bold(),
                    sort_field.yellow()
                );
                logger::print_reviews(&sorted, priority);
            }
        }

        Commands::Filter {
            all,
            pr_number,
            pr_numbers,
            repo,
            author,
            min_size,
            max_size,
            min_age,
            max_age,
            since_days,
            drafts_only,
            no_drafts,
            priority,
            json,
        } => {
            // Target specific PR: global --pr flag > local --pr_number
            let target_pr = ctx.cli.pr.or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                Vec::new()
            };

            // If pr_numbers was provided, we're done (skip other filters for batch mode)
            if pr_numbers.is_some() {
                if prs_from_numbers.is_empty() {
                    println!("No PRs found for the specified numbers.");
                    return Ok(());
                }
                if json {
                    println!("{}", serde_json::to_string_pretty(&prs_from_numbers)?);
                } else {
                    println!(
                        "\n🔍 {} PR(s) found\n",
                        prs_from_numbers.len().to_string().yellow().bold()
                    );
                    logger::print_reviews(&prs_from_numbers, priority);
                }
                return Ok(());
            }

            // Apply filters to the ctx.reviews (for non-batch mode)
            let filtered: Vec<_> = ctx
                .reviews
                .iter()
                .filter(|r| {
                    // Filter by specific PR number (if provided via --pr or --pr_number)
                    if let Some(num) = target_pr {
                        if r.pr_number != num {
                            return false;
                        }
                    }

                    // Filter by repo (partial match, case-insensitive)
                    if let Some(ref repo_filter) = repo {
                        if !r.repo.to_lowercase().contains(&repo_filter.to_lowercase()) {
                            return false;
                        }
                    }

                    // Filter by author (partial match, case-insensitive)
                    if let Some(ref author_filter) = author {
                        if !r
                            .pr_author
                            .to_lowercase()
                            .contains(&author_filter.to_lowercase())
                        {
                            return false;
                        }
                    }

                    // Filter by size
                    let total_size = r.additions + r.deletions;
                    if let Some(min) = min_size {
                        if total_size < min {
                            return false;
                        }
                    }
                    if let Some(max) = max_size {
                        if total_size > max {
                            return false;
                        }
                    }

                    // Filter by age
                    let age_days = (chrono::Utc::now() - r.created_at).num_days() as u32;
                    if let Some(min) = min_age {
                        if age_days < min {
                            return false;
                        }
                    }
                    if let Some(max) = max_age {
                        if age_days > max {
                            return false;
                        }
                    }

                    // Filter by since_days (PRs created in the last N days)
                    if let Some(days) = since_days {
                        if age_days >= days {
                            return false;
                        }
                    }

                    // Filter by draft status
                    if drafts_only && !r.draft {
                        return false;
                    }
                    if no_drafts && r.draft {
                        return false;
                    }

                    true
                })
                .cloned()
                .collect();

            // Filter out snoozed PRs (unless --pr is specified, consistent with list/delegate/search)
            let filtered: Vec<_> = if target_pr.is_none() {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                filtered
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .collect()
            } else {
                filtered
            };

            if filtered.is_empty() {
                if let Some(pr_num) = target_pr {
                    println!("\n⚠️  PR #{} not found or doesn't match filters.\n", pr_num);
                } else {
                    println!("\n🔍 No reviews match the specified filters.\n");
                }
                return Ok(());
            }

            // If --all flag is set, show all without prompting
            if all {
                if json {
                    let json_output = serde_json::to_string_pretty(&filtered)?;
                    println!("{}", json_output);
                } else {
                    println!(
                        "\n🔍 {} review(s) match your filters\n",
                        filtered.len().to_string().yellow().bold()
                    );
                    logger::print_reviews(&filtered, priority);
                }
                return Ok(());
            }

            // Interactive selection mode
            logger::print_reviews(&filtered, false);
            print!(
                "\n{} ",
                "Select PRs [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
            );
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match parse_selection(input.trim(), filtered.len()) {
                Selection::Quit => return Ok(()),
                Selection::Indices(indices) => {
                    let selected: Vec<_> =
                        indices.into_iter().map(|i| filtered[i].clone()).collect();
                    if json {
                        println!("{}", serde_json::to_string_pretty(&selected)?);
                    } else {
                        println!(
                            "\n🔍 {} review(s) selected\n",
                            selected.len().to_string().yellow().bold()
                        );
                        logger::print_reviews(&selected, priority);
                    }
                }
            }
        }

        Commands::Labels {
            pr_number,
            pr_numbers,
            pr,
            all,
            filter_by,
            repo,
            author,
            since_days,
            json,
            priority,
        } => {
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo and --author and --since-days filters when using interactive selection or --all
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let targets: Vec<_> = if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(num) = target_pr {
                // Single PR via --pr or positional
                let prs = github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?;
                prs
            } else if let Some(ref nums) = pr_numbers {
                // Parse comma-separated PR numbers
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Parallel fetch for all specified PRs
                let futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_results = futures::future::join_all(futures).await;
                let all_prs: Vec<_> = all_results
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, false);
                print!(
                    "\n{} ",
                    "Select PRs to show labels [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if targets.is_empty() {
                println!("No PRs found.");
                return Ok(());
            }

            // Fetch labels in parallel
            let label_futures = targets.iter().map(|review| {
                github::fetch_pr_labels(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &review.repo,
                    review.pr_number,
                )
            });

            let label_results: Vec<(
                github::PendingReview,
                Result<Vec<github::PullRequestLabel>, anyhow::Error>,
            )> = targets
                .iter()
                .cloned()
                .zip(join_all(label_futures).await)
                .collect();

            // Collect labels for display
            let mut all_labels_data: Vec<(github::PendingReview, Vec<github::PullRequestLabel>)> =
                Vec::new();
            let mut total_labels_count = 0usize;

            for (review, result) in label_results {
                match result {
                    Ok(labels) => {
                        total_labels_count += labels.len();
                        all_labels_data.push((review, labels));
                    }
                    Err(e) => {
                        println!(
                            "\n❌ Failed to fetch labels for #{}: {}",
                            review.pr_number, e
                        );
                    }
                }
            }

            if json {
                #[derive(serde::Serialize)]
                struct LabelOutput<'a> {
                    pr_number: u64,
                    pr_title: &'a str,
                    repo: &'a str,
                    pr_url: &'a str,
                    labels: &'a [github::PullRequestLabel],
                    #[serde(skip_serializing_if = "Option::is_none")]
                    priority_score: Option<u8>,
                }
                let json_output: Vec<LabelOutput> = all_labels_data
                    .iter()
                    .map(|(r, l)| {
                        let priority_score = if priority {
                            Some(logger::calculate_priority_score(r))
                        } else {
                            None
                        };
                        LabelOutput {
                            pr_number: r.pr_number,
                            pr_title: &r.pr_title,
                            repo: &r.repo,
                            pr_url: &r.pr_url,
                            labels: l,
                            priority_score,
                        }
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json_output).unwrap_or_default()
                );
            } else {
                println!("\n🏷️  Labels Summary\n{}", "─".repeat(45));

                // If filter_by is specified, show only matching labels
                if let Some(ref filter) = filter_by {
                    let filter_lower = filter.to_lowercase();
                    let mut found_any = false;
                    for (review, labels) in &all_labels_data {
                        for label in labels {
                            if label.name.to_lowercase().contains(&filter_lower) {
                                found_any = true;
                                let _color_hex = format!("#{}", label.color);
                                println!(
                                    "  {}  {}  (on #{} - {})",
                                    colorize_label(&label.name, &label.color),
                                    review.pr_title.bold(),
                                    review.pr_number,
                                    review.repo.dimmed()
                                );
                            }
                        }
                    }
                    if !found_any {
                        println!("  No labels matching '{}' found.", filter.yellow());
                    }
                } else {
                    // Show all labels grouped by PR
                    for (review, labels) in &all_labels_data {
                        // Show priority score if requested
                        let priority_line = if priority {
                            let score = logger::calculate_priority_score(review);
                            format!(
                                "  ⭐ Priority: {}/5  {}",
                                score,
                                logger::priority_stars(score)
                            )
                        } else {
                            String::new()
                        };
                        println!(
                            "\n📄 #{} {} ({}){}",
                            review.pr_number,
                            review.pr_title.bold(),
                            review.repo,
                            priority_line
                        );
                        if labels.is_empty() {
                            println!("  (no labels)");
                        } else {
                            for label in labels {
                                println!(
                                    "  {}  {}",
                                    colorize_label(&label.name, &label.color),
                                    label.description.as_deref().unwrap_or("").dimmed()
                                );
                            }
                        }
                    }

                    // Show label frequency summary
                    if total_labels_count > 0 {
                        use std::collections::HashMap;
                        let mut label_counts: HashMap<String, (String, usize)> = HashMap::new();
                        for (_, labels) in &all_labels_data {
                            for label in labels {
                                let entry = label_counts
                                    .entry(label.name.clone())
                                    .or_insert_with(|| (label.color.clone(), 0));
                                entry.1 += 1;
                            }
                        }
                        println!("\n📊 Label Frequency:");
                        let mut sorted: Vec<_> = label_counts.iter().collect();
                        sorted.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
                        for (name, (color, count)) in sorted.iter().take(10) {
                            let bar = "█".repeat(*count).cyan();
                            println!("  {}  {}  {}", colorize_label(name, color), bar, count);
                        }
                    }
                }
                println!("{}", "─".repeat(45));
            }
        }

        Commands::Review {
            pr_number,
            pr_numbers,
            pr,
            all,
            dry_run,
            context,
            output_file,
            language,
            priority,
            repo,
            author,
            since_days,
            json,
        } => {
            // Apply --repo, --author, and --since-days filters first (consistent with other commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            // Handle batch PR numbers first
            let prs = if let Some(ref nums) = pr_numbers {
                let nums: Vec<u64> = nums
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Parallel fetch all PRs by number
                let fetch_futures = nums.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<github::PendingReview> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                let target_pr = ctx.cli.pr.or(pr).or(pr_number);
                match target_pr {
                    Some(num) => {
                        github::fetch_pr_by_number(
                            &ctx.cfg.github_token,
                            &ctx.cfg.github_org,
                            &ctx.cfg.github_repos,
                            num,
                        )
                        .await?
                    }
                    None if all && !filtered_reviews.is_empty() => filtered_reviews,
                    None => {
                        if filtered_reviews.is_empty() {
                            println!("No matching reviews found.");
                            return Ok(());
                        }
                        logger::print_reviews(&filtered_reviews, false);
                        print!(
                            "\n{} ",
                            "Select PR to review [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                        );
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        match parse_selection(input.trim(), filtered_reviews.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => indices
                                .into_iter()
                                .map(|i| filtered_reviews[i].clone())
                                .collect(),
                        }
                    }
                }
            };

            if prs.is_empty() {
                println!("No PR found to review.");
                return Ok(());
            }

            // Dry-run mode: just show what would be reviewed
            if dry_run {
                println!("\n🔍 Dry-run mode — the following PRs would be reviewed:\n");
                for (i, review) in prs.iter().enumerate() {
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score).dimmed())
                    } else {
                        String::new()
                    };
                    let total = review.additions + review.deletions;
                    println!(
                        "  {}. #{} {}  ({}) [{} lines]{}",
                        i + 1,
                        review.pr_number,
                        review.pr_title.bold(),
                        review.repo.cyan(),
                        total,
                        priority_label
                    );
                    println!("     🔗 {}", review.pr_url.blue());
                }
                println!("\n  Total: {} PR(s)\n", prs.len());
                return Ok(());
            }

            // Fetch all diffs in parallel
            let diff_futures = prs.iter().map(|review| {
                github::fetch_pr_diff(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &review.repo,
                    review.pr_number,
                )
            });

            let diff_results: Vec<Result<Vec<github::PullRequestDiff>, anyhow::Error>> =
                join_all(diff_futures).await;

            #[derive(serde::Serialize)]
            struct ReviewOutput<'a> {
                pr_number: u64,
                pr_title: &'a str,
                repo: &'a str,
                url: &'a str,
                author: &'a str,
                total_files: usize,
                total_additions: u64,
                total_deletions: u64,
                priority_score: Option<u8>,
                file_summaries: Vec<FileSummary<'a>>,
            }

            #[derive(serde::Serialize)]
            struct FileSummary<'a> {
                filename: &'a str,
                status: &'a str,
                additions: u64,
                deletions: u64,
                language: Option<&'a String>,
            }

            let mut outputs: Vec<ReviewOutput> = Vec::new();

            for (review, result) in prs.iter().zip(diff_results.iter()) {
                let files = match result {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to fetch diff for PR #{}: {}",
                            review.pr_number, e
                        );
                        continue;
                    }
                };

                let total_additions: u64 = files.iter().map(|f| f.additions).sum();
                let total_deletions: u64 = files.iter().map(|f| f.deletions).sum();

                let priority_score = if priority {
                    Some(logger::calculate_priority_score(review))
                } else {
                    None
                };

                if json {
                    outputs.push(ReviewOutput {
                        pr_number: review.pr_number,
                        pr_title: &review.pr_title,
                        repo: &review.repo,
                        url: &review.pr_url,
                        author: &review.pr_author,
                        total_files: files.len(),
                        total_additions,
                        total_deletions,
                        priority_score,
                        file_summaries: files
                            .iter()
                            .map(|f| FileSummary {
                                filename: &f.filename,
                                status: &f.status,
                                additions: f.additions,
                                deletions: f.deletions,
                                language: f.language.as_ref(),
                            })
                            .collect(),
                    });
                } else {
                    let priority_display = if let Some(score) = priority_score {
                        format!("  ⭐ {}/5", score)
                    } else {
                        String::new()
                    };

                    println!("\n{}", "─".repeat(60));
                    println!(
                        "📄 {}  #{}{}",
                        review.pr_title.bold(),
                        review.pr_number,
                        priority_display
                    );
                    println!(
                        "   👤 {}  •  📁 {}  •  +{} / -{} lines",
                        review.pr_author.cyan(),
                        review.repo,
                        total_additions.to_string().green(),
                        total_deletions.to_string().red()
                    );
                    println!("{}", "─".repeat(60));

                    if files.is_empty() {
                        println!("  (no file changes)");
                    } else {
                        // Build unified diff output
                        let mut unified_diff = String::new();
                        for file in files {
                            let status_icon = match file.status.as_str() {
                                "added" => "+",
                                "removed" => "-",
                                "modified" => "M",
                                "renamed" => "R",
                                _ => "?",
                            };
                            let lang = language
                                .as_ref()
                                .or(file.language.as_ref())
                                .map(|s| s.as_str())
                                .unwrap_or("");
                            let header = format!(
                                "diff --git a/{} b/{} {}",
                                file.filename,
                                file.filename,
                                if file.status == "renamed" {
                                    "(renamed)"
                                } else {
                                    ""
                                }
                            );
                            let hunk_header = if file.patch.is_some() {
                                format!(
                                    "@@ -{},{} +{},{} @@ [{}] {}",
                                    1, file.deletions, 1, file.additions, lang, status_icon
                                )
                            } else {
                                format!("@@ -0,0 +0,0 @@ [{}] {}", lang, status_icon)
                            };

                            unified_diff.push_str(&format!("{}\n", header));
                            unified_diff.push_str(&format!("{}{}\n", hunk_header, status_icon));

                            if let Some(ref patch) = file.patch {
                                // Normalize patch context lines
                                let context_str = " ".repeat(context as usize);
                                for line in patch.lines() {
                                    let line = line.trim_end();
                                    if line.is_empty() {
                                        unified_diff
                                            .push_str(&format!("{}{}\n", context_str, line));
                                    } else if line.starts_with('+') && !line.starts_with("+++") {
                                        unified_diff.push_str(&format!(
                                            "{}{}\n",
                                            "+".yellow(),
                                            &line[1..]
                                        ));
                                    } else if line.starts_with('-') && !line.starts_with("---") {
                                        unified_diff.push_str(&format!(
                                            "{}{}\n",
                                            "-".red(),
                                            &line[1..]
                                        ));
                                    } else if line.starts_with(' ') || line.starts_with("@@") {
                                        unified_diff.push_str(&format!(" {}\n", line));
                                    } else {
                                        unified_diff.push_str(&format!("{}\n", line));
                                    }
                                }
                            } else {
                                unified_diff.push_str("  (binary or no preview available)\n");
                            }
                            unified_diff.push('\n');
                        }

                        if let Some(ref path) = output_file {
                            std::fs::write(path, &unified_diff)?;
                            println!(
                                "   💾 Diff written to {} ({:.1} KB)",
                                path.display().to_string().cyan(),
                                unified_diff.len() as f64 / 1024.0
                            );
                        } else {
                            // Try to use bat for syntax highlighting, fallback to plain print
                            let use_bat = std::process::Command::new("which")
                                .arg("bat")
                                .output()
                                .map(|o| o.status.success())
                                .unwrap_or(false);

                            if use_bat {
                                // Pipe diff through bat with appropriate language
                                let mut cmd = std::process::Command::new("bat");
                                cmd.arg("--style=changes")
                                    .arg("--color=always")
                                    .arg("--language=diff");
                                cmd.arg("--");
                                cmd.arg("-");

                                match cmd.stdin(std::process::Stdio::piped()).spawn() {
                                    Ok(child) => {
                                        use std::io::Write;
                                        if let Some(ref stdin) = child.stdin {
                                            let mut w = stdin;
                                            let _ = w.write_all(unified_diff.as_bytes());
                                        }
                                        let _ = child.wait_with_output();
                                    }
                                    Err(_) => {
                                        println!("{}", unified_diff);
                                    }
                                }
                            } else {
                                // Fallback: print with basic coloring
                                println!("\n{}\n", unified_diff);
                            }
                        }
                    }
                    println!("{}", "─".repeat(60));
                }
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&outputs)?);
            }

            println!();
        }

        Commands::Top {
            limit,
            min_score,
            priority,
            repo,
            author,
            since_days,
            json,
        } => {
            let limit = limit.unwrap_or(10);
            let min_score = min_score.unwrap_or(3).min(5);

            // Calculate priority for all ctx.reviews
            let mut scored: Vec<_> = ctx
                .reviews
                .iter()
                .map(|r| {
                    let score = logger::calculate_priority_score(r);
                    (r.clone(), score)
                })
                .filter(|(_, score)| *score >= min_score)
                .collect();

            // Apply --since-days filter (optional)
            if let Some(days) = since_days {
                let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                scored.retain(|(r, _)| r.created_at >= cutoff);
            }

            // Apply --repo filter (partial match, case-insensitive)
            if let Some(ref repo_filter) = repo {
                let pattern = repo_filter.to_lowercase();
                scored.retain(|(r, _)| r.repo.to_lowercase().contains(&pattern));
            }

            // Apply --author filter (partial match, case-insensitive)
            if let Some(ref author_filter) = author {
                let pattern = author_filter.to_lowercase();
                scored.retain(|(r, _)| r.pr_author.to_lowercase().contains(&pattern));
            }

            // Filter out snoozed PRs (consistent with list/delegate/search/summary commands)
            let snooze_file = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"))
                .join(".snoozed.json");

            let now = chrono::Utc::now();
            let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                    if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                        entries
                            .into_iter()
                            .filter_map(|e| {
                                let repo = e.get("repo")?.as_str()?.to_string();
                                let pr_number = e.get("pr_number")?.as_u64()?;
                                let until_str = e.get("snoozed_until")?.as_str()?;
                                if let Ok(until) = chrono::DateTime::parse_from_rfc3339(until_str) {
                                    if until.with_timezone(&chrono::Utc) > now {
                                        return Some((repo, pr_number));
                                    }
                                }
                                None
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            scored.retain(|(r, _)| {
                !snoozed_prs
                    .iter()
                    .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
            });

            // Sort by priority score descending, then by age ascending
            scored.sort_by(|a, b| {
                let score_cmp = b.1.cmp(&a.1);
                if score_cmp == std::cmp::Ordering::Equal {
                    a.0.created_at.cmp(&b.0.created_at)
                } else {
                    score_cmp
                }
            });

            let top_prs: Vec<_> = scored.into_iter().take(limit).collect();

            if top_prs.is_empty() {
                println!(
                    "\n🎯 No high-priority reviews found (min score: {})\n",
                    min_score
                );
                return Ok(());
            }

            if json {
                #[derive(serde::Serialize)]
                struct TopReview<'a> {
                    repo: &'a str,
                    pr_number: u64,
                    pr_title: &'a str,
                    pr_author: &'a str,
                    pr_url: &'a str,
                    priority_score: u8,
                    age_days: i64,
                    additions: u64,
                    deletions: u64,
                    draft: bool,
                }
                let output: Vec<TopReview> = top_prs
                    .iter()
                    .map(|(r, score)| TopReview {
                        repo: &r.repo,
                        pr_number: r.pr_number,
                        pr_title: &r.pr_title,
                        pr_author: &r.pr_author,
                        pr_url: &r.pr_url,
                        priority_score: *score,
                        age_days: (chrono::Utc::now() - r.created_at).num_days(),
                        additions: r.additions,
                        deletions: r.deletions,
                        draft: r.draft,
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "\n🎯 Top {} Priority Reviews (score >= {})\n{}",
                    limit,
                    min_score,
                    "─".repeat(50)
                );
                for (r, score) in &top_prs {
                    let age_days = (chrono::Utc::now() - r.created_at).num_days();
                    let age_label = match age_days {
                        0 => "today".green(),
                        1 => "1 day".normal(),
                        _ => format!("{} days", age_days).red(),
                    };

                    let size = r.additions + r.deletions;
                    let size_label = if size < 50 {
                        "XS".green()
                    } else if size < 200 {
                        "S".green()
                    } else if size < 500 {
                        "M".yellow()
                    } else if size < 1000 {
                        "L".red()
                    } else {
                        "XL".magenta()
                    };

                    let draft_label = if r.draft {
                        " [DRAFT]".yellow()
                    } else {
                        "".normal()
                    };

                    let priority_display = if priority {
                        format!(" {}", logger::priority_stars(*score))
                    } else {
                        String::new()
                    };

                    println!(
                        "  ⭐{}{}  {}  #{} ({}){}",
                        score,
                        priority_display,
                        r.pr_title.bold(),
                        r.pr_number,
                        r.repo.dimmed(),
                        draft_label
                    );
                    println!(
                        "      👤 {}  •  📦 {} ({} lines)  •  ⏱️ {}",
                        r.pr_author.cyan(),
                        size_label,
                        size,
                        age_label
                    );
                    println!("      🔗 {}", r.pr_url.blue().underline());
                    println!();
                }
                println!("{}", "─".repeat(50));
                println!("  💡 Use `--min-score 4` for only critical PRs");
                println!("  💡 Use `--json` for scripting\n");
            }
        }

        Commands::Quick {
            max_lines,
            limit,
            priority,
            repo,
            author,
            json,
            since_days,
        } => {
            let max_lines = max_lines.unwrap_or(200);
            let limit = limit.unwrap_or(10);

            // Apply --since-days filter first (consistent with other commands)
            let after_since: Vec<_> = {
                match since_days {
                    Some(days) => {
                        let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                        ctx.reviews
                            .iter()
                            .filter(|r| r.created_at >= cutoff)
                            .cloned()
                            .collect()
                    }
                    None => ctx.reviews.clone(),
                }
            };

            // Apply --repo filter (partial match, case-insensitive)
            let filtered: Vec<_> = {
                let mut result = after_since;

                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // Filter to small, non-draft PRs
            let quick_wins: Vec<_> = filtered
                .iter()
                .filter(|r| !r.draft && (r.additions + r.deletions) <= max_lines)
                .take(limit)
                .cloned()
                .collect();

            if quick_wins.is_empty() {
                println!(
                    "\n⚡ No quick wins found (max {} lines, non-draft)\n",
                    max_lines
                );
                return Ok(());
            }

            if json {
                #[derive(serde::Serialize)]
                struct QuickWin<'a> {
                    repo: &'a str,
                    pr_number: u64,
                    pr_title: &'a str,
                    pr_author: &'a str,
                    pr_url: &'a str,
                    age_days: i64,
                    additions: u64,
                    deletions: u64,
                }
                let output: Vec<QuickWin> = quick_wins
                    .iter()
                    .map(|r| QuickWin {
                        repo: &r.repo,
                        pr_number: r.pr_number,
                        pr_title: &r.pr_title,
                        pr_author: &r.pr_author,
                        pr_url: &r.pr_url,
                        age_days: (chrono::Utc::now() - r.created_at).num_days(),
                        additions: r.additions,
                        deletions: r.deletions,
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "\n⚡ Quick Wins (≤{} lines, non-draft)\n{}",
                    max_lines,
                    "─".repeat(50)
                );
                for r in &quick_wins {
                    let age_days = (chrono::Utc::now() - r.created_at).num_days();
                    let age_label = match age_days {
                        0 => "today".green(),
                        1 => "1 day".normal(),
                        _ => format!("{} days", age_days).red(),
                    };
                    let total = r.additions + r.deletions;
                    let size_label = if total < 50 {
                        format!("{} lines", total).green()
                    } else {
                        format!("{} lines", total).yellow()
                    };

                    let priority_display = if priority {
                        let score = logger::calculate_priority_score(r);
                        format!("  ⭐ {}/5", logger::priority_stars(score))
                    } else {
                        String::new()
                    };

                    println!(
                        "  ⚡ {} #{} ({}){}\n      👤 {}  •  📦 {}  •  ⏱️ {}\n      🔗 {}",
                        r.pr_title.bold(),
                        r.pr_number,
                        r.repo.dimmed(),
                        priority_display,
                        r.pr_author.cyan(),
                        size_label,
                        age_label,
                        r.pr_url.blue().underline()
                    );
                    println!();
                }
                println!("{}", "─".repeat(50));
                println!("  💡 Use `--max-lines 100` for tiny PRs only");
                println!("  💡 Use `--priority` to show priority scores");
                println!("  💡 Use `--json` for scripting\n");
            }
        }

        Commands::Catchup {
            pr_number,
            min_age,
            limit,
            json,
            priority,
            repo,
            author,
            all,
            since_days,
        } => {
            let min_age = min_age as i64;
            let limit = limit.unwrap_or(10);

            let now = chrono::Utc::now();
            let cutoff = now - chrono::Duration::days(min_age);

            // Handle --pr filter first (shorthand for specific PR)
            let pr_filtered: Vec<_> = match ctx.cli.pr.or(pr_number) {
                Some(num) => ctx
                    .reviews
                    .iter()
                    .filter(|r| r.pr_number == num)
                    .cloned()
                    .collect(),
                None => ctx.reviews.clone(),
            };

            // Apply --since-days filter first (only show PRs created since N days ago)
            let pr_filtered: Vec<_> = match since_days {
                Some(days) => {
                    let since_cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    pr_filtered
                        .into_iter()
                        .filter(|r| r.created_at >= since_cutoff)
                        .collect()
                }
                None => pr_filtered,
            };

            // Filter: only PRs older than min_age, sorted oldest-first
            let mut neglected: Vec<_> = pr_filtered
                .iter()
                .filter(|r| r.created_at <= cutoff)
                .cloned()
                .collect();

            // Apply --repo filter
            if let Some(ref repo_filter) = repo {
                let pattern = repo_filter.to_lowercase();
                neglected.retain(|r| r.repo.to_lowercase().contains(&pattern));
            }

            // Apply --author filter
            if let Some(ref author_filter) = author {
                let pattern = author_filter.to_lowercase();
                neglected.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
            }

            neglected.sort_by_key(|r| r.created_at); // oldest first

            // Apply --all flag (no limit) or use --limit
            let effective_limit = if all { neglected.len() } else { limit };
            let shown: Vec<_> = neglected.iter().take(effective_limit).cloned().collect();

            if shown.is_empty() {
                println!(
                    "\n🎯 No neglected PRs found (all younger than {} days)\n",
                    min_age
                );
                return Ok(());
            }

            if json {
                #[derive(serde::Serialize)]
                struct CatchupItem<'a> {
                    repo: &'a str,
                    pr_number: u64,
                    pr_title: &'a str,
                    pr_author: &'a str,
                    pr_url: &'a str,
                    age_days: i64,
                    additions: u64,
                    deletions: u64,
                    draft: bool,
                    neglect_score: u8,
                }
                let output: Vec<CatchupItem> = shown
                    .iter()
                    .map(|r| {
                        let age_days = (now - r.created_at).num_days();
                        let neglect_score = logger::calculate_priority_score(r);
                        CatchupItem {
                            repo: &r.repo,
                            pr_number: r.pr_number,
                            pr_title: &r.pr_title,
                            pr_author: &r.pr_author,
                            pr_url: &r.pr_url,
                            age_days,
                            additions: r.additions,
                            deletions: r.deletions,
                            draft: r.draft,
                            neglect_score,
                        }
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                let total_neglected = neglected.len();
                println!(
                    "\n🎯 Catchup — {} PR(s) older than {} days, showing {} oldest\n{}",
                    total_neglected.to_string().yellow().bold(),
                    min_age,
                    limit,
                    "─".repeat(50)
                );

                for r in &shown {
                    let age_days = (now - r.created_at).num_days();
                    let neglect_score = logger::calculate_priority_score(r);

                    // Visual urgency bar
                    let bar_len = (neglect_score as usize).min(5);
                    let urgency_bar: String = (0..5)
                        .map(|i| {
                            if i < bar_len {
                                if i < 2 {
                                    "🔵"
                                } else if i < 4 {
                                    "🟡"
                                } else {
                                    "🔴"
                                }
                            } else {
                                "⚪"
                            }
                        })
                        .collect();

                    let priority_display = if priority {
                        format!("  {}", logger::priority_stars(neglect_score))
                    } else {
                        String::new()
                    };

                    let age_label = if age_days == 0 {
                        "today".green()
                    } else if age_days == 1 {
                        "1 day".normal()
                    } else if age_days <= 7 {
                        format!("{} days", age_days).yellow()
                    } else {
                        format!("{} days!", age_days).red()
                    };

                    let total = r.additions + r.deletions;
                    let size_label = if total < 50 {
                        "XS".to_string()
                    } else if total < 200 {
                        "S".to_string()
                    } else if total < 500 {
                        "M".to_string()
                    } else if total < 1000 {
                        "L".to_string()
                    } else {
                        "XL".to_string()
                    };

                    let draft_str = if r.draft { " 📝 DRAFT" } else { "" };

                    println!(
                        "  {}  {} #{} ({}){}{}\n      👤 {}  •  📦 {} ({} lines)  •  ⏱️ {} old",
                        urgency_bar,
                        r.pr_title.bold(),
                        r.pr_number,
                        r.repo.dimmed(),
                        draft_str.yellow(),
                        priority_display,
                        r.pr_author.cyan(),
                        size_label,
                        total,
                        age_label
                    );
                    println!("      🔗 {}", r.pr_url.blue().underline());
                    println!();
                }

                if !all && total_neglected > limit {
                    println!(
                        "  ...and {} more. Use `--all` to see all.\n",
                        total_neglected - limit
                    );
                }
                println!("{}", "─".repeat(50));
                println!("  💡 Use `--min-age 7` to focus on week-old+ PRs");
                println!("  💡 Use `--priority` to show priority scores");
                println!("  💡 Use `--all` to show all neglected PRs without limit\n");
            }
        }

        Commands::Age {
            min_days,
            older_than,
            grouped,
            priority,
            repo,
            author,
            json,
        } => {
            use chrono::Utc;

            let now = Utc::now();

            // Age buckets: (label, emoji, min_days, max_days)
            // max_days of None means +infinity
            #[derive(Clone, Copy)]
            struct Bucket(&'static str, &'static str, Option<i64>, Option<i64>);
            const BUCKETS: [Bucket; 5] = [
                Bucket("New", "🆕", None, Some(2)),
                Bucket("Fresh", "🌱", Some(2), Some(4)),
                Bucket("Aging", "⏳", Some(4), Some(8)),
                Bucket("Stale", "🔥", Some(8), Some(15)),
                Bucket("Overdue", "💀", Some(15), None),
            ];

            let min_days = min_days.map(|d| d as i64);
            let older_than = older_than.map(|d| d as i64);

            #[derive(serde::Serialize)]
            struct AgeItem<'a> {
                repo: &'a str,
                pr_number: u64,
                pr_title: &'a str,
                pr_author: &'a str,
                pr_url: &'a str,
                age_days: i64,
                additions: u64,
                deletions: u64,
                draft: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                priority_score: Option<u8>,
            }

            #[derive(serde::Serialize)]
            struct AgeBucket<'a> {
                label: &'a str,
                emoji: &'a str,
                prs: Vec<AgeItem<'a>>,
            }

            let mut buckets: Vec<(Bucket, Vec<&github::PendingReview>)> =
                BUCKETS.iter().cloned().map(|b| (b, vec![])).collect();

            for r in &ctx.reviews {
                let age_days = (now - r.created_at).num_days();

                // Apply --repo filter
                if let Some(ref repo_filter) = repo {
                    if !r.repo.to_lowercase().contains(&repo_filter.to_lowercase()) {
                        continue;
                    }
                }

                // Apply --author filter
                if let Some(ref author_filter) = author {
                    if !r
                        .pr_author
                        .to_lowercase()
                        .contains(&author_filter.to_lowercase())
                    {
                        continue;
                    }
                }

                // Apply --older-than filter
                if let Some(cutoff) = older_than {
                    if age_days <= cutoff {
                        continue;
                    }
                }

                // Apply --min-days filter
                if let Some(min) = min_days {
                    if age_days < min {
                        continue;
                    }
                }

                // Find matching bucket (first match wins since ranges are non-overlapping)
                let mut matched = false;
                for (bucket, prs) in &mut buckets {
                    let Bucket(_, _, bucket_min, bucket_max) = *bucket;
                    let in_bucket = match (bucket_min, bucket_max) {
                        (Some(min), Some(max)) => age_days >= min && age_days <= max,
                        (Some(min), None) => age_days >= min,
                        (None, Some(max)) => age_days <= max,
                        (None, None) => true,
                    };
                    if in_bucket {
                        prs.push(r);
                        matched = true;
                        break; // PR goes in first matching bucket
                    }
                }
                let _ = matched; // suppress unused warning
            }

            if json {
                let output: Vec<AgeBucket> = buckets
                    .iter()
                    .filter(|(_, prs)| !prs.is_empty())
                    .map(|(bucket, prs)| {
                        let Bucket(label, emoji, _, _) = *bucket;
                        AgeBucket {
                            label,
                            emoji,
                            prs: prs
                                .iter()
                                .map(|r| {
                                    let age_days = (now - r.created_at).num_days();
                                    AgeItem {
                                        repo: &r.repo,
                                        pr_number: r.pr_number,
                                        pr_title: &r.pr_title,
                                        pr_author: &r.pr_author,
                                        pr_url: &r.pr_url,
                                        age_days,
                                        additions: r.additions,
                                        deletions: r.deletions,
                                        draft: r.draft,
                                        priority_score: if priority {
                                            Some(logger::calculate_priority_score(r))
                                        } else {
                                            None
                                        },
                                    }
                                })
                                .collect(),
                        }
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if grouped {
                // Grouped view: one section per bucket
                let total: usize = buckets.iter().map(|(_, p)| p.len()).sum();
                println!(
                    "\n📊 Age Breakdown — {} PRs total\n{}",
                    total,
                    "─".repeat(50)
                );

                let mut any_shown = false;
                for (bucket, prs) in &buckets {
                    if prs.is_empty() {
                        continue;
                    }
                    any_shown = true;
                    let Bucket(label, emoji, _, _) = *bucket;
                    println!("\n{} {} ({} PRs)", emoji, label.bold(), prs.len());
                    println!("{}", "─".repeat(40));

                    for r in prs {
                        let age_days = (now - r.created_at).num_days();
                        let age_str = if age_days == 0 {
                            "today".green().to_string()
                        } else if age_days == 1 {
                            "1 day".yellow().to_string()
                        } else {
                            format!("{} days", age_days).red().to_string()
                        };
                        let draft_str = if r.draft {
                            " 📝DRAFT".yellow().to_string()
                        } else {
                            String::new()
                        };
                        let priority_str = if priority {
                            let score = logger::calculate_priority_score(r);
                            format!("  ⭐ {}/5", score)
                        } else {
                            String::new()
                        };
                        println!(
                            "  #{}  {}{}  •  👤 {}  •  +{}/-{} lines  •  {} old{}\n      📁 {}  🔗 {}",
                            r.pr_number,
                            r.pr_title.bold(),
                            priority_str,
                            r.pr_author.cyan(),
                            r.additions,
                            r.deletions,
                            age_str,
                            draft_str,
                            r.repo.dimmed(),
                            r.pr_url.blue().underline()
                        );
                    }
                }

                if !any_shown {
                    println!("\n  No PRs match the specified age filters.\n");
                }
                println!("\n{}", "─".repeat(50));
                println!("  💡 Use `--older-than 7` to see only week-old+ PRs");
                println!("  💡 Use `--json` for scripting\n");
            } else {
                // Flat view: sorted oldest-first within each bucket, buckets ordered newest→oldest
                let mut all_filtered: Vec<&github::PendingReview> = Vec::new();
                for (_, prs) in &buckets {
                    all_filtered.extend(prs.iter().cloned());
                }
                // Sort oldest-first (most neglected first)
                all_filtered.sort_by_key(|r| r.created_at);

                if all_filtered.is_empty() {
                    println!("\n⏰ No PRs match the specified age filters.\n");
                    return Ok(());
                }

                println!(
                    "\n⏰ Age Report — {} PRs (oldest first)\n{}",
                    all_filtered.len(),
                    "─".repeat(50)
                );

                for r in &all_filtered {
                    let age_days = (now - r.created_at).num_days();

                    // Determine bucket for emoji
                    let (emoji, bucket_label) = if age_days >= 15 {
                        ("💀", "Overdue")
                    } else if age_days >= 8 {
                        ("🔥", "Stale")
                    } else if age_days >= 4 {
                        ("⏳", "Aging")
                    } else if age_days >= 2 {
                        ("🌱", "Fresh")
                    } else {
                        ("🆕", "New")
                    };

                    let age_str = if age_days == 0 {
                        "today".green().to_string()
                    } else if age_days == 1 {
                        "1 day".yellow().to_string()
                    } else if age_days <= 7 {
                        format!("{} days", age_days).yellow().to_string()
                    } else {
                        format!("{} days", age_days).red().to_string()
                    };

                    let draft_str = if r.draft {
                        " 📝DRAFT".yellow().to_string()
                    } else {
                        String::new()
                    };
                    let priority_str = if priority {
                        let score = logger::calculate_priority_score(r);
                        format!(" ⭐ {}/5", score)
                    } else {
                        String::new()
                    };

                    println!(
                        "{}  {}  #{} ({})\n    👤 {}  •  +{}/-{} lines  •  {} old{}{}",
                        emoji,
                        bucket_label.cyan(),
                        r.pr_number,
                        r.repo.dimmed(),
                        r.pr_author.cyan(),
                        r.additions,
                        r.deletions,
                        age_str,
                        draft_str,
                        priority_str
                    );
                    println!("    🔗 {}", r.pr_url.blue().underline());
                    println!();
                }

                println!("{}", "─".repeat(50));
                println!("  Buckets: 🆕 New 0-1d  🌱 Fresh 2-3d  ⏳ Aging 4-7d  🔥 Stale 8-14d  💀 Overdue 15d+");
                println!("  💡 Use `--grouped` to see PRs organized by age bucket");
                println!("  💡 Use `--older-than 7` to focus on week-old+ PRs\n");
            }
        }

        Commands::Size {
            filter_size,
            grouped,
            priority,
            repo,
            author,
            since_days,
            json,
        } => {
            // Apply --repo, --author, and --since-days filters first (consistent with other commands)
            let filtered: Vec<_> = {
                let mut result = ctx.reviews.clone();

                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            // Size buckets: XS, S, M, L, XL (non-overlapping ranges)
            // XS: 0-50, S: 51-200, M: 201-500, L: 501-1000, XL: 1001+
            #[derive(Clone, Copy)]
            struct SizeBucket(&'static str, u64, Option<u64>);
            const SIZE_BUCKETS: [SizeBucket; 5] = [
                SizeBucket("XS", 0, Some(50)),
                SizeBucket("S", 51, Some(200)),
                SizeBucket("M", 201, Some(500)),
                SizeBucket("L", 501, Some(1000)),
                SizeBucket("XL", 1001, None),
            ];

            // Parse filter_size if provided
            let filter_sizes: Option<Vec<String>> = filter_size.as_ref().map(|s| {
                s.split(',')
                    .map(|part| part.trim().to_uppercase())
                    .collect()
            });

            #[derive(serde::Serialize)]
            struct SizeItem<'a> {
                repo: &'a str,
                pr_number: u64,
                pr_title: &'a str,
                pr_author: &'a str,
                pr_url: &'a str,
                additions: u64,
                deletions: u64,
                total_lines: u64,
                draft: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                priority_score: Option<u8>,
            }

            #[derive(serde::Serialize)]
            struct SizeBucketOutput<'a> {
                label: &'a str,
                emoji: &'a str,
                min_lines: u64,
                max_lines: Option<u64>,
                prs: Vec<SizeItem<'a>>,
            }

            let mut buckets: Vec<(SizeBucket, Vec<&github::PendingReview>)> =
                SIZE_BUCKETS.iter().cloned().map(|b| (b, vec![])).collect();

            for r in &filtered {
                let total_lines = r.additions + r.deletions;

                // Apply size filter if specified
                if let Some(ref sizes) = filter_sizes {
                    let mut matched = false;
                    for (bucket, _) in &buckets {
                        let SizeBucket(label, min, max) = *bucket;
                        let in_bucket = if let Some(max) = max {
                            total_lines >= min && total_lines <= max
                        } else {
                            total_lines >= min
                        };
                        if sizes.iter().any(|s| s.as_str() == label) && in_bucket {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        continue;
                    }
                }

                // Find matching bucket
                for (bucket, prs) in &mut buckets {
                    let SizeBucket(_, min, max) = *bucket;
                    let in_bucket = if let Some(max) = max {
                        total_lines >= min && total_lines <= max
                    } else {
                        total_lines >= min
                    };
                    if in_bucket {
                        prs.push(r);
                        break;
                    }
                }
            }

            // Update emoji based on bucket
            let size_emojis = ["⚖️", "🔬", "📊", "📈", "🚀"];

            if json {
                let output: Vec<SizeBucketOutput> = buckets
                    .iter()
                    .filter(|(_, prs)| !prs.is_empty())
                    .map(|(bucket, prs)| {
                        let SizeBucket(label, min, max) = *bucket;
                        SizeBucketOutput {
                            label,
                            emoji: size_emojis
                                [SIZE_BUCKETS.iter().position(|b| b.0 == label).unwrap_or(0)],
                            min_lines: min,
                            max_lines: max,
                            prs: prs
                                .iter()
                                .map(|r| {
                                    let score = if priority {
                                        Some(logger::calculate_priority_score(r))
                                    } else {
                                        None
                                    };
                                    SizeItem {
                                        repo: &r.repo,
                                        pr_number: r.pr_number,
                                        pr_title: &r.pr_title,
                                        pr_author: &r.pr_author,
                                        pr_url: &r.pr_url,
                                        additions: r.additions,
                                        deletions: r.deletions,
                                        total_lines: r.additions + r.deletions,
                                        draft: r.draft,
                                        priority_score: score,
                                    }
                                })
                                .collect(),
                        }
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if grouped {
                // Grouped view: one section per size bucket
                let total: usize = buckets.iter().map(|(_, p)| p.len()).sum();
                println!(
                    "\n📏 Size Breakdown — {} PRs total\n{}",
                    total,
                    "─".repeat(50)
                );

                let mut any_shown = false;
                for (bucket, prs) in &buckets {
                    if prs.is_empty() {
                        continue;
                    }
                    any_shown = true;
                    let SizeBucket(label, min, max) = *bucket;
                    let bucket_idx = SIZE_BUCKETS.iter().position(|b| b.0 == label).unwrap_or(0);
                    let emoji = size_emojis[bucket_idx];
                    let range_str = if let Some(max) = max {
                        format!("{}-{} lines", min, max)
                    } else {
                        format!("{}+ lines", min)
                    };

                    let bucket_additions: u64 = prs.iter().map(|r| r.additions).sum();
                    let bucket_deletions: u64 = prs.iter().map(|r| r.deletions).sum();

                    println!(
                        "\n{} {} {} ({} PRs, +{}/-{} lines)",
                        emoji,
                        label.bold(),
                        range_str.dimmed(),
                        prs.len(),
                        bucket_additions,
                        bucket_deletions
                    );
                    println!("{}", "─".repeat(40));

                    for r in prs {
                        let _total_lines = r.additions + r.deletions;
                        let draft_str = if r.draft {
                            " 📝DRAFT".yellow().to_string()
                        } else {
                            String::new()
                        };
                        let priority_str = if priority {
                            let score = logger::calculate_priority_score(r);
                            let stars = "★".repeat(score as usize);
                            format!(" {}", stars.red())
                        } else {
                            String::new()
                        };
                        println!(
                            "  #{}  {}  •  👤 {}  •  +{}/-{} lines{}\n      📁 {}  🔗 {}",
                            r.pr_number,
                            r.pr_title.bold(),
                            r.pr_author.cyan(),
                            r.additions,
                            r.deletions,
                            format_args!("{}{}", draft_str, priority_str),
                            r.repo.dimmed(),
                            r.pr_url.blue().underline()
                        );
                    }
                }

                if !any_shown {
                    println!("\n  No PRs match the specified size filters.\n");
                }
                println!("\n{}", "─".repeat(50));
                println!("  💡 Use `--filter-size XS,S,M,L,XL` to show only specific sizes");
                println!("  💡 Use `--priority` or `-P` to show priority scores");
                println!("  💡 Use `--json` for scripting\n");
            } else {
                // Flat view: sorted smallest-first within each bucket
                let mut all_filtered: Vec<&github::PendingReview> = Vec::new();
                for (_, prs) in &buckets {
                    all_filtered.extend(prs.iter().cloned());
                }
                // Sort smallest-first (quick wins first)
                all_filtered.sort_by_key(|r| r.additions + r.deletions);

                if all_filtered.is_empty() {
                    println!("\n📏 No PRs match the specified size filters.\n");
                    return Ok(());
                }

                println!(
                    "\n📏 Size Report — {} PRs (smallest first)\n{}",
                    all_filtered.len(),
                    "─".repeat(50)
                );

                // Calculate bucket emoji dynamically
                let get_size_info = |total: u64| -> (&str, &str, &str) {
                    if total <= 30 {
                        ("XS", "⚖️", "tiny")
                    } else if total <= 100 {
                        ("S", "🔬", "small")
                    } else if total <= 300 {
                        ("M", "📊", "medium")
                    } else if total <= 800 {
                        ("L", "📈", "large")
                    } else {
                        ("XL", "🚀", "extra large")
                    }
                };

                for r in &all_filtered {
                    let total_lines = r.additions + r.deletions;
                    let (size_label, emoji, _size_desc) = get_size_info(total_lines);
                    let draft_str = if r.draft {
                        " 📝DRAFT".yellow().to_string()
                    } else {
                        String::new()
                    };
                    let priority_str = if priority {
                        let score = logger::calculate_priority_score(r);
                        let stars = "★".repeat(score as usize);
                        format!(" {}", stars.red())
                    } else {
                        String::new()
                    };

                    println!(
                        "{} {}  #{}  {}  •  👤 {}  •  +{}/-{} lines{}\n    📁 {}  🔗 {}",
                        emoji,
                        size_label.cyan(),
                        r.pr_number,
                        r.pr_title.bold(),
                        r.pr_author.cyan(),
                        r.additions,
                        r.deletions,
                        format_args!("{}{}", draft_str, priority_str),
                        r.repo.dimmed(),
                        r.pr_url.blue().underline()
                    );
                }

                println!("{}", "─".repeat(50));
                println!("  Sizes: ⚖️ XS <30  🔬 S 31-100  📊 M 101-300  📈 L 301-800  🚀 XL 801+");
                println!("  💡 Use `--grouped` to see PRs organized by size bucket");
                println!("  💡 Use `--filter-size S,M` to show only small/medium PRs\n");
            }
        }

        Commands::Digest {
            days,
            raw,
            dry_run,
            repo,
            author,
            priority,
            since_days,
            older_than,
        } => {
            use chrono::{Duration, Utc};
            use std::collections::HashMap;

            let now = Utc::now();
            let _cutoff = now - Duration::days(days as i64);

            // Apply --repo filter (partial match, case-insensitive)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter (consistent with other commands - PRs created in last N days)
                if let Some(since) = since_days {
                    let since_cutoff = now - Duration::days(since as i64);
                    result.retain(|r| r.created_at >= since_cutoff);
                }

                // Apply --older-than filter (only show PRs older than N days)
                if let Some(older) = older_than {
                    let older_cutoff = now - Duration::days(older as i64);
                    result.retain(|r| r.created_at <= older_cutoff);
                }

                result
            };

            // Dry-run mode: show what would be included without generating the digest
            if dry_run {
                if filtered_reviews.is_empty() {
                    println!("\n🔍 No PRs would be included in the digest (dry-run).\n");
                    println!("  💡 Try adjusting your filters or increasing --days\n");
                } else {
                    println!(
                        "\n🔍 Dry-run: {} PR(s) would be included in the digest\n",
                        filtered_reviews.len().to_string().yellow().bold()
                    );
                    println!("  Filters:");
                    if let Some(ref repo_filter) = repo {
                        println!("    • repo: {}", repo_filter.yellow());
                    }
                    if let Some(ref author_filter) = author {
                        println!("    • author: {}", author_filter.yellow());
                    }
                    if let Some(since) = since_days {
                        println!("    • since_days: {}", since.to_string().yellow());
                    }
                    if let Some(older) = older_than {
                        println!("    • older_than: {}", older.to_string().yellow());
                    }
                    println!("    • days: {}", days.to_string().yellow());
                    println!("\n  PRs that would be included:\n");
                    logger::print_reviews(&filtered_reviews, priority);
                }
                println!("\n  (dry-run — no digest generated)\n");
                return Ok(());
            }

            // Compute age buckets (continuous non-overlapping ranges: New 0-1d, Fresh 2-3d, Aging 4-7d, Stale 8-14d, Overdue 15d+)
            #[derive(Clone, Copy)]
            struct Bucket(&'static str, &'static str, i64, i64);
            const BUCKETS: [Bucket; 5] = [
                Bucket("New", "🆕", 0, 1),
                Bucket("Fresh", "🌱", 2, 3),
                Bucket("Aging", "⏳", 4, 7),
                Bucket("Stale", "🔥", 8, 14),
                Bucket("Overdue", "💀", 15, i64::MAX),
            ];

            let mut bucket_counts: Vec<(Bucket, usize)> =
                BUCKETS.iter().copied().map(|b| (b, 0)).collect();
            let mut total_additions = 0u64;
            let mut total_deletions = 0u64;
            let mut by_repo: HashMap<String, usize> = HashMap::new();
            let mut by_author: HashMap<String, usize> = HashMap::new();
            let mut overdue_prs: Vec<&github::PendingReview> = vec![];

            for r in &filtered_reviews {
                let age_days = (now - r.created_at).num_days();
                total_additions += r.additions;
                total_deletions += r.deletions;
                *by_repo.entry(r.repo.clone()).or_insert(0) += 1;
                if !r.pr_author.is_empty() {
                    *by_author.entry(r.pr_author.clone()).or_insert(0) += 1;
                }
                for (bucket, count) in &mut bucket_counts {
                    let Bucket(_label, _emoji, bucket_min, bucket_max) = *bucket;
                    if age_days >= bucket_min && age_days <= bucket_max {
                        *count += 1;
                        if age_days >= 15 {
                            overdue_prs.push(r);
                        }
                        break; // PR belongs to exactly one bucket
                    }
                }
            }

            let total = filtered_reviews.len();
            let overdue_count = overdue_prs.len();

            // Top repos
            let mut top_repos: Vec<_> = by_repo.iter().collect();
            top_repos.sort_by(|a, b| b.1.cmp(a.1));
            let top_repos: Vec<_> = top_repos.into_iter().take(5).collect();

            // Top authors
            let mut top_authors: Vec<_> = by_author.iter().collect();
            top_authors.sort_by(|a, b| b.1.cmp(a.1));
            let top_authors: Vec<_> = top_authors.into_iter().take(5).collect();

            // Build age bar (visual breakdown)
            let age_bar: String = bucket_counts
                .iter()
                .map(|(b, c)| {
                    let Bucket(_label, emoji, _, _) = *b;
                    if *c > 0 {
                        format!("{}:{} ", emoji, c)
                    } else {
                        String::new()
                    }
                })
                .collect::<Vec<_>>()
                .join("");

            if raw {
                // Raw markdown output (for piping to Slack/Teams)
                let days_label = if let Some(s) = since_days {
                    format!(" (last {} days)", s)
                } else {
                    format!(" — last {} days", days)
                };
                println!("## 📋 Review Digest{}\n", days_label);
                println!();
                println!(
                    "**Total:** {} PRs | **+{}** / **-{}** lines | **Overdue:** {}",
                    total, total_additions, total_deletions, overdue_count
                );
                println!();

                if !top_repos.is_empty() {
                    println!("### By Repository");
                    for (repo, count) in &top_repos {
                        println!("- **{}**: {} PR(s)", repo, count);
                    }
                    println!();
                }

                if !top_authors.is_empty() {
                    println!("### By Author");
                    for (author, count) in &top_authors {
                        println!("- **{}**: {} PR(s)", author, count);
                    }
                    println!();
                }

                println!("### Age Breakdown");
                for (b, c) in &bucket_counts {
                    let Bucket(label, emoji, _, _) = *b;
                    println!("- {} **{}**: {} PR(s)", emoji, label, c);
                }
                println!();

                if !overdue_prs.is_empty() {
                    println!("### 🚨 Overdue (15d+)");
                    for r in overdue_prs.iter().take(10) {
                        let age = (now - r.created_at).num_days();
                        let priority_label = if priority {
                            let score = logger::calculate_priority_score(r);
                            format!(" ⭐{}", score)
                        } else {
                            String::new()
                        };
                        println!(
                            "- [{}#{}]({}) *{}* — {}d old{}",
                            r.repo, r.pr_number, r.pr_url, r.pr_title, age, priority_label
                        );
                    }
                    if !priority {
                        println!("\n_Use `--priority` to show priority scores_");
                    }
                }
            } else {
                // Pretty terminal output
                println!(
                    "\n📋 Weekly Review Digest — last {} days\n{}",
                    days,
                    "─".repeat(45)
                );
                println!();
                println!("  📊 Summary");
                println!("     Total PRs:          {}", total);
                println!(
                    "     Lines changed:      +{} / -{}",
                    total_additions.to_string().green(),
                    total_deletions.to_string().red()
                );
                if overdue_count > 0 {
                    println!(
                        "     🚨 Overdue (15d+):  {}",
                        overdue_count.to_string().red().bold()
                    );
                }
                println!();

                if !age_bar.is_empty() {
                    println!("  ⏱️  Age Breakdown");
                    for (b, c) in &bucket_counts {
                        let Bucket(label, emoji, _, _) = *b;
                        if *c > 0 {
                            println!("     {} {}: {}", emoji, label, c);
                        }
                    }
                    println!();
                }

                if !top_repos.is_empty() {
                    println!("  📁 By Repository (top 5)");
                    for (repo, count) in &top_repos {
                        println!("     {}: {}", repo, count);
                    }
                    println!();
                }

                if !top_authors.is_empty() {
                    println!("  👥 By Author (top 5)");
                    for (author, count) in &top_authors {
                        println!("     {}: {}", author.cyan(), count);
                    }
                    println!();
                }

                if !overdue_prs.is_empty() {
                    println!("  🚨 Overdue PRs (15d+)");
                    for r in overdue_prs.iter().take(5) {
                        let age = (now - r.created_at).num_days();
                        let priority_label = if priority {
                            let score = logger::calculate_priority_score(r);
                            format!(" {}", logger::priority_stars(score).dimmed())
                        } else {
                            String::new()
                        };
                        println!(
                            "     #{} {} — {}d old — {}{}",
                            r.pr_number,
                            r.pr_title.bold(),
                            age,
                            r.repo.dimmed(),
                            priority_label
                        );
                    }
                    if overdue_prs.len() > 5 {
                        println!("     ...and {} more", overdue_prs.len() - 5);
                    }
                    println!();
                }

                if priority {
                    // Show priority breakdown
                    let mut scored: Vec<_> = filtered_reviews
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            (r, score)
                        })
                        .collect();
                    scored.sort_by(|a, b| b.1.cmp(&a.1));

                    if let Some((most_urgent, top_score)) = scored.first() {
                        let age_days = (now - most_urgent.created_at).num_days();
                        let total = most_urgent.additions + most_urgent.deletions;
                        println!("  🚨 Most Urgent:");
                        println!(
                            "    {}  #{}  {}",
                            most_urgent.pr_title.bold(),
                            most_urgent.pr_number,
                            logger::priority_stars(*top_score).red()
                        );
                        println!(
                            "    👤 {}  •  📦 {} lines  •  ⏱️ {}d  •  {}",
                            most_urgent.pr_author.cyan(),
                            total,
                            age_days,
                            most_urgent.repo.dimmed()
                        );
                        println!();
                    }

                    // Priority breakdown
                    let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> = HashMap::new();
                    for (review, score) in &scored {
                        score_groups.entry(*score).or_default().push(review);
                    }

                    println!("  ⭐ Priority Breakdown:");
                    for score in (1..=5).rev() {
                        if let Some(prs) = score_groups.get(&score) {
                            let stars = logger::priority_stars(score);
                            let oldest_age = prs
                                .iter()
                                .map(|r| (now - r.created_at).num_days())
                                .max()
                                .unwrap_or(0);
                            println!(
                                "     ⭐{}  {} PR(s)  •  oldest: {}d  •  +{}/-{} lines",
                                stars,
                                prs.len(),
                                oldest_age,
                                prs.iter().map(|r| r.additions).sum::<u64>(),
                                prs.iter().map(|r| r.deletions).sum::<u64>()
                            );
                        }
                    }
                    println!();
                }

                println!("  💡 Use `--raw` to get Markdown output for Slack/Teams");
                if !priority {
                    println!("  💡 Use `--priority` to show priority scores and most urgent PR");
                }
                println!("{}", "─".repeat(45));
            }
        }

        Commands::Snooze {
            action,
            pr_number,
            pr,
            pr_numbers,
            days,
            since_days,
            json,
            priority,
            repo,
            author,
        } => {
            use serde::{Deserialize, Serialize};

            // Snooze storage file
            let snooze_file = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"))
                .join(".snoozed.json");

            #[derive(Debug, Clone, Serialize, Deserialize, Default)]
            struct SnoozeEntry {
                pub repo: String,
                pub pr_number: u64,
                pub pr_title: String,
                pub pr_url: String,             // PR URL for quick access
                pub snoozed_until: String,      // ISO 8601 timestamp
                pub snoozed_at: String,         // ISO 8601 timestamp when snooze was created
                pub additions: u64,             // Lines added
                pub deletions: u64,             // Lines deleted
                pub priority_score: Option<u8>, // Priority score when snoozed
                pub author: String,             // PR author
            }

            // Load existing snooze data
            let mut snoozed: Vec<SnoozeEntry> = if snooze_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                    serde_json::from_str(&content).unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            match action {
                crate::cli::SnoozeAction::Add => {
                    let duration_days = days.unwrap_or(3) as i64;
                    let snooze_until =
                        (chrono::Utc::now() + chrono::Duration::days(duration_days)).to_rfc3339();

                    // Priority: global --pr flag > local --pr > local pr_number > pr_numbers
                    let target_pr = ctx.cli.pr.or(pr).or(pr_number);

                    // If no PR numbers provided, show interactive picker
                    let targets: Vec<_> = if let Some(num) = target_pr {
                        // Single PR via --pr or positional
                        github::fetch_pr_by_number(
                            &ctx.cfg.github_token,
                            &ctx.cfg.github_org,
                            &ctx.cfg.github_repos,
                            num,
                        )
                        .await?
                    } else if let Some(ref nums) = pr_numbers {
                        let mut results = Vec::new();
                        for part in nums.split(',') {
                            if let Ok(num) = part.trim().parse::<u64>() {
                                results.push(num);
                            }
                        }
                        if results.is_empty() {
                            println!("❌ No valid PR numbers provided.");
                            return Ok(());
                        }
                        // Parallel fetch all PRs by number using join_all
                        let fetch_futures = results.iter().map(|num| {
                            github::fetch_pr_by_number(
                                &ctx.cfg.github_token,
                                &ctx.cfg.github_org,
                                &ctx.cfg.github_repos,
                                *num,
                            )
                        });
                        let all_prs: Vec<github::PendingReview> =
                            futures::future::join_all(fetch_futures)
                                .await
                                .into_iter()
                                .filter_map(|r| r.ok())
                                .flatten()
                                .collect();
                        all_prs
                    } else {
                        // Apply --repo, --author, and --since-days filters (consistent with other commands)
                        let mut filtered = ctx.reviews.clone();

                        if let Some(ref repo_filter) = repo {
                            let pattern = repo_filter.to_lowercase();
                            filtered.retain(|r| r.repo.to_lowercase().contains(&pattern));
                        }

                        if let Some(ref author_filter) = author {
                            let pattern = author_filter.to_lowercase();
                            filtered.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                        }

                        // Apply --since-days filter
                        if let Some(days) = since_days {
                            let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                            filtered.retain(|r| r.created_at >= cutoff);
                        }

                        if filtered.is_empty() {
                            println!("No pending reviews found to snooze (or none match filters).");
                            return Ok(());
                        }
                        logger::print_reviews(&filtered, false);
                        print!(
                            "\n{} ",
                            "Select PRs to snooze [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                        );
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        match parse_selection(input.trim(), filtered.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => {
                                indices.into_iter().map(|i| filtered[i].clone()).collect()
                            }
                        }
                    };

                    if targets.is_empty() {
                        println!("No PRs to snooze.");
                        return Ok(());
                    }

                    println!(
                        "\n😴 Snoozing {} PR(s) for {} day(s)...\n",
                        targets.len().to_string().yellow().bold(),
                        duration_days.to_string().cyan()
                    );

                    for review in &targets {
                        // Remove existing entry if present (to update snooze time)
                        snoozed.retain(|e| {
                            !(e.repo == review.repo && e.pr_number == review.pr_number)
                        });

                        snoozed.push(SnoozeEntry {
                            repo: review.repo.clone(),
                            pr_number: review.pr_number,
                            pr_title: review.pr_title.clone(),
                            pr_url: review.pr_url.clone(),
                            snoozed_until: snooze_until.clone(),
                            snoozed_at: chrono::Utc::now().to_rfc3339(),
                            additions: review.additions,
                            deletions: review.deletions,
                            priority_score: Some(logger::calculate_priority_score(review)),
                            author: review.pr_author.clone(),
                        });

                        println!(
                            "  😴 {} ({}) - until {}",
                            review.pr_title.dimmed(),
                            format!("#{}", review.pr_number).dimmed(),
                            snooze_until[..10].dimmed()
                        );
                    }

                    // Save snooze data
                    if let Some(ref dir) = ctx.output_dir {
                        std::fs::create_dir_all(dir).ok();
                    }
                    if let Err(e) =
                        std::fs::write(&snooze_file, serde_json::to_string_pretty(&snoozed)?)
                    {
                        println!("  ⚠️ Failed to save snooze data: {}", e);
                    } else {
                        println!("\n✅ Snooze list saved ({} PRs snoozed)", snoozed.len());
                    }
                    println!();
                }

                crate::cli::SnoozeAction::List => {
                    let now = chrono::Utc::now();
                    let mut active: Vec<_> = snoozed
                        .iter()
                        .filter(|e| {
                            if let Ok(until) =
                                chrono::DateTime::parse_from_rfc3339(&e.snoozed_until)
                            {
                                until.with_timezone(&chrono::Utc) > now
                            } else {
                                false
                            }
                        })
                        .collect();

                    // Apply --repo filter (partial match, case-insensitive)
                    if let Some(ref repo_filter) = repo {
                        let pattern = repo_filter.to_lowercase();
                        active.retain(|e| e.repo.to_lowercase().contains(&pattern));
                    }

                    // Apply --author filter (partial match, case-insensitive)
                    if let Some(ref author_filter) = author {
                        let pattern = author_filter.to_lowercase();
                        active.retain(|e| e.author.to_lowercase().contains(&pattern));
                    }

                    if active.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("\n😴 No currently snoozed PRs.\n");
                        }
                        return Ok(());
                    }

                    // Sort by expiry time
                    active.sort_by(|a, b| {
                        let a_time = chrono::DateTime::parse_from_rfc3339(&a.snoozed_until)
                            .map(|t| t.timestamp())
                            .unwrap_or(0);
                        let b_time = chrono::DateTime::parse_from_rfc3339(&b.snoozed_until)
                            .map(|t| t.timestamp())
                            .unwrap_or(0);
                        a_time.cmp(&b_time)
                    });

                    // JSON output mode
                    if json {
                        #[derive(serde::Serialize)]
                        struct SnoozeListItem<'a> {
                            repo: &'a str,
                            pr_number: u64,
                            pr_title: &'a str,
                            author: &'a str,
                            snoozed_until: &'a str,
                            remaining_hours: i64,
                            additions: u64,
                            deletions: u64,
                            priority_score: Option<u8>,
                        }

                        let output: Vec<SnoozeListItem> = active
                            .iter()
                            .map(|e| {
                                let until = chrono::DateTime::parse_from_rfc3339(&e.snoozed_until)
                                    .map(|t| t.with_timezone(&chrono::Utc))
                                    .unwrap_or(now);
                                let remaining = (until - now).num_hours();
                                SnoozeListItem {
                                    repo: &e.repo,
                                    pr_number: e.pr_number,
                                    pr_title: &e.pr_title,
                                    author: &e.author,
                                    snoozed_until: &e.snoozed_until,
                                    remaining_hours: remaining,
                                    additions: e.additions,
                                    deletions: e.deletions,
                                    priority_score: e.priority_score,
                                }
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&output).unwrap_or_default()
                        );
                        return Ok(());
                    }

                    println!(
                        "\n😴 Currently Snoozed PRs ({} total)\n{}",
                        active.len(),
                        "─".repeat(50)
                    );

                    for entry in &active {
                        let until = chrono::DateTime::parse_from_rfc3339(&entry.snoozed_until)
                            .map(|t| t.with_timezone(&chrono::Utc))
                            .unwrap_or(now);
                        let remaining = (until - now).num_hours();
                        let remaining_label = if remaining < 24 {
                            format!("{}h left", remaining).red()
                        } else {
                            format!("{}d left", remaining / 24).yellow()
                        };

                        let priority_display = if priority {
                            if let Some(score) = entry.priority_score {
                                if score > 0 {
                                    format!("  ⭐{}/5", score)
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        };

                        println!(
                            "  😴 {}  #{} ({}) - {}{}",
                            entry.pr_title.bold(),
                            entry.pr_number,
                            entry.repo.dimmed(),
                            remaining_label,
                            priority_display
                        );
                    }
                    println!("{}", "─".repeat(50));
                    println!("\n💡 Use `snooze remove --pr 123` to wake a PR early\n");
                }

                crate::cli::SnoozeAction::Review => {
                    let now = chrono::Utc::now();
                    let mut active: Vec<_> = snoozed
                        .iter()
                        .filter(|e| {
                            if let Ok(until) =
                                chrono::DateTime::parse_from_rfc3339(&e.snoozed_until)
                            {
                                until.with_timezone(&chrono::Utc) > now
                            } else {
                                false
                            }
                        })
                        .collect();

                    // Apply --repo filter (partial match, case-insensitive)
                    if let Some(ref repo_filter) = repo {
                        let pattern = repo_filter.to_lowercase();
                        active.retain(|e| e.repo.to_lowercase().contains(&pattern));
                    }

                    // Apply --author filter (partial match, case-insensitive)
                    if let Some(ref author_filter) = author {
                        let pattern = author_filter.to_lowercase();
                        active.retain(|e| e.author.to_lowercase().contains(&pattern));
                    }

                    if active.is_empty() {
                        println!("\n😴 No currently snoozed PRs.\n");
                        return Ok(());
                    }

                    // Sort by expiry time (soonest first)
                    active.sort_by(|a, b| {
                        let a_time = chrono::DateTime::parse_from_rfc3339(&a.snoozed_until)
                            .map(|t| t.timestamp())
                            .unwrap_or(0);
                        let b_time = chrono::DateTime::parse_from_rfc3339(&b.snoozed_until)
                            .map(|t| t.timestamp())
                            .unwrap_or(0);
                        a_time.cmp(&b_time)
                    });

                    println!(
                        "\n😴 Snoozed PRs — Detailed View ({} total)\n{}",
                        active.len(),
                        "─".repeat(55)
                    );

                    for entry in &active {
                        let until = chrono::DateTime::parse_from_rfc3339(&entry.snoozed_until)
                            .map(|t| t.with_timezone(&chrono::Utc))
                            .unwrap_or(now);
                        let remaining = (until - now).num_hours();
                        let remaining_label = if remaining < 24 {
                            format!("{}h left", remaining).red()
                        } else {
                            format!("{}d left", remaining / 24).yellow()
                        };

                        // Parse original snooze time to calculate how long ago it was snoozed
                        let snoozed_at = chrono::DateTime::parse_from_rfc3339(&entry.snoozed_at)
                            .map(|t| t.with_timezone(&chrono::Utc))
                            .unwrap_or(now);
                        let snoozed_for = (now - snoozed_at).num_hours();
                        let snoozed_for_label = if snoozed_for < 24 {
                            format!("{}h ago", snoozed_for)
                        } else {
                            format!("{}d ago", snoozed_for / 24)
                        };

                        println!(
                            "  😴 {}  #{}  ({})",
                            entry.pr_title.bold(),
                            entry.pr_number,
                            entry.repo.dimmed()
                        );
                        println!(
                            "      ⏱️  Snoozed {}  •  ⏳ {}  •  📊 +{}/-{} lines",
                            snoozed_for_label.dimmed(),
                            remaining_label,
                            entry.additions,
                            entry.deletions
                        );
                        println!("      🔗 {}", entry.pr_url.blue().underline());

                        // Show priority if available
                        if let Some(score) = entry.priority_score {
                            if score > 0 {
                                let stars = "⭐".repeat(score as usize);
                                println!("      {} Priority score", stars.dimmed());
                            }
                        }
                        println!();
                    }
                    println!("{}", "─".repeat(55));
                    println!("\n💡 Use `snooze remove --pr 123` to wake a PR early");
                    println!("💡 Use `snooze extend --pr 123 --days 5` to extend snooze\n");
                }

                crate::cli::SnoozeAction::Remove => {
                    // Priority: global --pr flag > local --pr > local pr_number > pr_numbers
                    let target_pr = ctx.cli.pr.or(pr).or(pr_number);

                    let to_remove: Vec<u64> = if let Some(ref nums) = pr_numbers {
                        nums.split(',')
                            .filter_map(|p| p.trim().parse().ok())
                            .collect()
                    } else if let Some(num) = target_pr {
                        vec![num]
                    } else {
                        println!("\n❌ Please specify PR numbers to remove: `snooze remove --pr 123,456`\n");
                        return Ok(());
                    };

                    if to_remove.is_empty() {
                        println!("❌ No valid PR numbers provided.");
                        return Ok(());
                    }

                    let initial_len = snoozed.len();
                    snoozed.retain(|e| !to_remove.contains(&e.pr_number));

                    let removed = initial_len - snoozed.len();
                    if removed > 0 {
                        if let Err(e) =
                            std::fs::write(&snooze_file, serde_json::to_string_pretty(&snoozed)?)
                        {
                            println!("  ⚠️ Failed to save snooze data: {}", e);
                        } else {
                            println!(
                                "\n✅ Removed {} PR(s) from snooze list ({} remaining)",
                                removed,
                                snoozed.len()
                            );
                        }
                    } else {
                        println!("\n😶 No matching snoozed PRs found.");
                    }
                }

                crate::cli::SnoozeAction::Clear => {
                    if snoozed.is_empty() {
                        println!("\n😴 Snooze list is already empty.\n");
                        return Ok(());
                    }

                    let count = snoozed.len();
                    snoozed.clear();
                    if snooze_file.exists() {
                        std::fs::remove_file(&snooze_file).ok();
                    }
                    println!("\n🧹 Cleared {} snoozed PR(s) from the list.\n", count);
                }

                crate::cli::SnoozeAction::Expire => {
                    let now = chrono::Utc::now();

                    // Extract expired entries so we can display them before removing
                    let expired: Vec<_> = snoozed
                        .iter()
                        .filter(|e| {
                            if let Ok(until) =
                                chrono::DateTime::parse_from_rfc3339(&e.snoozed_until)
                            {
                                until.with_timezone(&chrono::Utc) <= now
                            } else {
                                false
                            }
                        })
                        .cloned()
                        .collect();

                    if expired.is_empty() {
                        println!("\n✨ No expired snooze entries to clean up.\n");
                        return Ok(());
                    }

                    // Remove expired entries
                    snoozed.retain(|e| {
                        if let Ok(until) = chrono::DateTime::parse_from_rfc3339(&e.snoozed_until) {
                            until.with_timezone(&chrono::Utc) > now
                        } else {
                            true
                        }
                    });

                    println!(
                        "\n🧹 Cleaned up {} expired snooze entry(s):\n",
                        expired.len().to_string().yellow().bold()
                    );

                    for entry in &expired {
                        println!(
                            "  ✨ {} #{} ({})",
                            entry.pr_title.dimmed(),
                            entry.pr_number.to_string().dimmed(),
                            entry.repo.dimmed()
                        );
                    }
                    println!(
                        "\n  ↩️  {} PR(s) have returned to your pending list.",
                        expired.len()
                    );

                    // Save updated snooze data
                    if let Some(ref dir) = ctx.output_dir {
                        std::fs::create_dir_all(dir).ok();
                    }
                    if snoozed.is_empty() {
                        if snooze_file.exists() {
                            std::fs::remove_file(&snooze_file).ok();
                        }
                        println!("\n✅ All snooze entries cleaned (list is now empty).");
                    } else if let Err(e) =
                        std::fs::write(&snooze_file, serde_json::to_string_pretty(&snoozed)?)
                    {
                        println!("  ⚠️ Failed to save snooze data: {}", e);
                    } else {
                        println!("\n✅ {} snoozed PR(s) remain in the list.", snoozed.len());
                    }
                    println!();
                }

                crate::cli::SnoozeAction::Extend => {
                    let duration_days = days.unwrap_or(3) as i64;
                    let new_snooze_until =
                        (chrono::Utc::now() + chrono::Duration::days(duration_days)).to_rfc3339();

                    // Priority: global --pr flag > local --pr > local pr_number > pr_numbers
                    let target_pr = ctx.cli.pr.or(pr).or(pr_number);

                    // Handle direct PR targeting first (consistent with Add/Remove)
                    if let Some(num) = target_pr {
                        let _to_extend = [num];
                        let mut extended_count = 0;
                        let mut not_found_count = 0;

                        for entry in &mut snoozed {
                            if entry.pr_number == num {
                                entry.snoozed_until = new_snooze_until.clone();
                                extended_count += 1;
                            }
                        }

                        if !snoozed.iter().any(|e| e.pr_number == num) {
                            not_found_count += 1;
                        }

                        if extended_count > 0 {
                            if let Err(e) = std::fs::write(
                                &snooze_file,
                                serde_json::to_string_pretty(&snoozed)?,
                            ) {
                                println!("  ⚠️ Failed to save snooze data: {}", e);
                            } else {
                                println!(
                                    "\n✅ Extended {} PR(s) until {} ({} days)",
                                    extended_count.to_string().green().bold(),
                                    &new_snooze_until[..10].cyan(),
                                    duration_days
                                );
                            }
                        }

                        if not_found_count > 0 {
                            println!("  ⚠️ PR #{} was not in the snooze list — use `snooze add` to snooze it first", num);
                        }

                        if extended_count == 0 && not_found_count == 0 {
                            println!("\n😶 No matching snoozed PRs found.");
                        }
                    } else if let Some(ref nums) = pr_numbers {
                        let to_extend: Vec<u64> = nums
                            .split(',')
                            .filter_map(|p| p.trim().parse().ok())
                            .collect();

                        if to_extend.is_empty() {
                            println!("❌ No valid PR numbers provided.");
                            return Ok(());
                        }

                        let mut extended_count = 0;
                        let mut not_found_count = 0;

                        for entry in &mut snoozed {
                            if to_extend.contains(&entry.pr_number) {
                                entry.snoozed_until = new_snooze_until.clone();
                                extended_count += 1;
                            }
                        }

                        // Also handle entries that may not be in the list yet but are in the pr_numbers
                        // If a PR is specified but not in snoozed list, offer to add it
                        for num in &to_extend {
                            if !snoozed.iter().any(|e| e.pr_number == *num) {
                                not_found_count += 1;
                            }
                        }

                        if extended_count > 0 {
                            if let Err(e) = std::fs::write(
                                &snooze_file,
                                serde_json::to_string_pretty(&snoozed)?,
                            ) {
                                println!("  ⚠️ Failed to save snooze data: {}", e);
                            } else {
                                println!(
                                    "\n✅ Extended {} PR(s) until {} ({} days)",
                                    extended_count.to_string().green().bold(),
                                    &new_snooze_until[..10].cyan(),
                                    duration_days
                                );
                            }
                        }

                        if not_found_count > 0 {
                            println!("  ⚠️ {} PR(s) were not in the snooze list — use `snooze add` to snooze them first", not_found_count);
                        }

                        if extended_count == 0 && not_found_count == 0 {
                            println!("\n😶 No matching snoozed PRs found.");
                        }
                    } else {
                        // Interactive: show snoozed list and let user pick
                        let now = chrono::Utc::now();
                        let active: Vec<_> = snoozed
                            .iter()
                            .filter(|e| {
                                if let Ok(until) =
                                    chrono::DateTime::parse_from_rfc3339(&e.snoozed_until)
                                {
                                    until.with_timezone(&chrono::Utc) > now
                                } else {
                                    false
                                }
                            })
                            .collect();

                        if active.is_empty() {
                            println!("\n😴 No currently snoozed PRs to extend.\n");
                            return Ok(());
                        }

                        println!(
                            "\n📋 Currently snoozed PRs (select to extend):\n{}",
                            "─".repeat(50)
                        );
                        for (i, entry) in active.iter().enumerate() {
                            let until = chrono::DateTime::parse_from_rfc3339(&entry.snoozed_until)
                                .map(|t| t.with_timezone(&chrono::Utc))
                                .unwrap_or(now);
                            let remaining = (until - now).num_hours();
                            let remaining_label = if remaining < 24 {
                                format!("{}h left", remaining).red()
                            } else {
                                format!("{}d left", remaining / 24).yellow()
                            };
                            println!(
                                "  [{}] {}  #{} ({}) - {}",
                                i + 1,
                                entry.pr_title.bold(),
                                entry.pr_number,
                                entry.repo.dimmed(),
                                remaining_label
                            );
                        }
                        println!("{}", "─".repeat(50));

                        print!(
                            "\n{} ",
                            "Select PRs to extend [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                        );
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        let input = input.trim();

                        match parse_selection(input, active.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => {
                                let indices_count = indices.len();
                                let prs_to_extend: Vec<(String, u64)> = indices
                                    .iter()
                                    .filter_map(|idx| {
                                        active.get(*idx).map(|e| (e.repo.clone(), e.pr_number))
                                    })
                                    .collect();

                                for (repo, pr_number) in &prs_to_extend {
                                    for e in &mut snoozed {
                                        if e.repo == *repo && e.pr_number == *pr_number {
                                            e.snoozed_until = new_snooze_until.clone();
                                            break;
                                        }
                                    }
                                }

                                if let Err(e) = std::fs::write(
                                    &snooze_file,
                                    serde_json::to_string_pretty(&snoozed)?,
                                ) {
                                    println!("  ⚠️ Failed to save snooze data: {}", e);
                                } else {
                                    println!(
                                        "\n✅ Extended {} PR(s) until {} ({} days)",
                                        indices_count.to_string().green().bold(),
                                        &new_snooze_until[..10].cyan(),
                                        duration_days
                                    );
                                }
                            }
                        }
                    }
                    println!();
                }
            }

            // If listing/showing ctx.reviews, filter out snoozed PRs
            // (The actual filtering happens in the List command below via a shared helper)
        }

        Commands::Follow {
            action,
            pr_number,
            pr_numbers,
            pr,
            json,
            repo,
            author,
            priority,
            since_days,
        } => {
            use serde::{Deserialize, Serialize};

            // Compute target PRs: global --pr > local --pr > local --pr-number > --pr-numbers
            let target_prs: Option<Vec<u64>> = {
                // Global --pr has highest priority
                if let Some(p) = ctx.cli.pr {
                    Some(vec![p])
                // Local --pr is next
                } else if let Some(p) = pr {
                    Some(vec![p])
                // Local --pr-number is next
                } else if let Some(p) = pr_number {
                    Some(vec![p])
                // --pr-numbers for comma-separated values
                } else if let Some(ref nums) = pr_numbers {
                    let parsed: Vec<u64> = nums
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    if parsed.is_empty() {
                        None
                    } else {
                        Some(parsed)
                    }
                } else {
                    None
                }
            };

            // Follow storage file
            let follow_file = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"))
                .join(".followed.json");

            #[derive(Debug, Clone, Serialize, Deserialize)]
            struct FollowedPr {
                pub repo: String,
                pub pr_number: u64,
                pub pr_title: String,
                pub pr_url: String,
                pub followed_at: String,
                pub last_check: String,
                pub last_known_state: String,  // open, merged, closed
                pub last_ci_status: String,    // success, failure, pending, unknown
                pub last_review_state: String, // none, approved, changes_requested, commented
                pub last_commit_sha: String,
                pub additions: u64,
                pub deletions: u64,
                pub author: String,
                pub draft: bool,
            }

            // Load existing followed PRs
            let mut followed: Vec<FollowedPr> = if follow_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&follow_file) {
                    serde_json::from_str(&content).unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            match action {
                crate::cli::FollowAction::Add => {
                    // If target PRs specified via --pr / --pr-number / --pr-numbers, fetch them directly
                    let targets: Vec<_> = if let Some(ref nums) = target_prs {
                        if nums.is_empty() {
                            println!("❌ No valid PR numbers provided.");
                            return Ok(());
                        }
                        // Parallel fetch all PRs by number using join_all
                        let fetch_futures = nums.iter().map(|num| {
                            github::fetch_pr_by_number(
                                &ctx.cfg.github_token,
                                &ctx.cfg.github_org,
                                &ctx.cfg.github_repos,
                                *num,
                            )
                        });
                        let all_prs: Vec<github::PendingReview> = join_all(fetch_futures)
                            .await
                            .into_iter()
                            .filter_map(|r| r.ok())
                            .flatten()
                            .collect();
                        all_prs
                    } else {
                        // Apply --repo, --author, and --since-days filters to pending ctx.reviews for interactive selection
                        let filtered_reviews: Vec<_> = {
                            let mut result = ctx.reviews.clone();

                            // Apply --since-days filter
                            if let Some(days) = since_days {
                                let cutoff =
                                    chrono::Utc::now() - chrono::Duration::days(days as i64);
                                result.retain(|r| r.created_at >= cutoff);
                            }

                            // Apply --repo filter (partial match, case-insensitive)
                            if let Some(ref repo_filter) = repo {
                                let pattern = repo_filter.to_lowercase();
                                result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                            }

                            // Apply --author filter (partial match, case-insensitive)
                            if let Some(ref author_filter) = author {
                                let pattern = author_filter.to_lowercase();
                                result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                            }

                            result
                        };

                        if filtered_reviews.is_empty() {
                            println!("No pending reviews found to follow. Use --pr flag or specify PR numbers.");
                            return Ok(());
                        }
                        logger::print_reviews(&filtered_reviews, false);
                        print!(
                            "\n{} ",
                            "Select PRs to follow [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                        );
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        match parse_selection(input.trim(), filtered_reviews.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => indices
                                .into_iter()
                                .map(|i| filtered_reviews[i].clone())
                                .collect(),
                        }
                    };

                    if targets.is_empty() {
                        println!("No PRs to follow.");
                        return Ok(());
                    }

                    println!(
                        "\n👁️  Following {} PR(s)...\n",
                        targets.len().to_string().yellow().bold()
                    );

                    let now = chrono::Utc::now().to_rfc3339();
                    for review in &targets {
                        // Remove existing entry if present (re-follow with updated state)
                        followed.retain(|e| {
                            !(e.repo == review.repo && e.pr_number == review.pr_number)
                        });

                        followed.push(FollowedPr {
                            repo: review.repo.clone(),
                            pr_number: review.pr_number,
                            pr_title: review.pr_title.clone(),
                            pr_url: review.pr_url.clone(),
                            followed_at: now.clone(),
                            last_check: now.clone(),
                            last_known_state: if review.draft {
                                "draft".to_string()
                            } else {
                                "open".to_string()
                            },
                            last_ci_status: "unknown".to_string(),
                            last_review_state: "none".to_string(),
                            last_commit_sha: review.branch.clone(),
                            additions: review.additions,
                            deletions: review.deletions,
                            author: review.pr_author.clone(),
                            draft: review.draft,
                        });

                        println!(
                            "  👁️  {} ({})",
                            review.pr_title.dimmed(),
                            format!("#{}", review.pr_number).dimmed()
                        );
                    }

                    // Save follow data
                    if let Some(ref dir) = ctx.output_dir {
                        std::fs::create_dir_all(dir).ok();
                    }
                    if let Err(e) =
                        std::fs::write(&follow_file, serde_json::to_string_pretty(&followed)?)
                    {
                        println!("  ⚠️ Failed to save follow data: {}", e);
                    } else {
                        println!("\n✅ Following {} PR(s)", followed.len());
                    }
                    println!();
                }

                crate::cli::FollowAction::List => {
                    if followed.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("\n👁️  Not following any PRs.\n");
                            println!("  Use `prctrl follow add <PR_NUMBER>` to start following.");
                        }
                        return Ok(());
                    }

                    // Apply --repo filter (partial match, case-insensitive)
                    let followed: Vec<_> = match repo {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.repo.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    // Apply --author filter (partial match, case-insensitive)
                    let followed: Vec<_> = match author {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.author.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    if followed.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("\n👁️  No followed PRs match your filters.\n");
                        }
                        return Ok(());
                    }

                    if json {
                        // JSON output for scripting
                        #[derive(Serialize)]
                        struct FollowListItem {
                            repo: String,
                            pr_number: u64,
                            pr_title: String,
                            pr_url: String,
                            followed_at: String,
                            last_check: String,
                            last_known_state: String,
                            last_ci_status: String,
                            last_review_state: String,
                            last_commit_sha: String,
                            additions: u64,
                            deletions: u64,
                            author: String,
                            draft: bool,
                        }
                        let items: Vec<FollowListItem> = followed
                            .iter()
                            .map(|pr| FollowListItem {
                                repo: pr.repo.clone(),
                                pr_number: pr.pr_number,
                                pr_title: pr.pr_title.clone(),
                                pr_url: pr.pr_url.clone(),
                                followed_at: pr.followed_at.clone(),
                                last_check: pr.last_check.clone(),
                                last_known_state: pr.last_known_state.clone(),
                                last_ci_status: pr.last_ci_status.clone(),
                                last_review_state: pr.last_review_state.clone(),
                                last_commit_sha: pr.last_commit_sha.clone(),
                                additions: pr.additions,
                                deletions: pr.deletions,
                                author: pr.author.clone(),
                                draft: pr.draft,
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&items)?);
                    } else {
                        println!(
                            "\n👁️  Following {} PR(s)\n{}",
                            followed.len(),
                            "─".repeat(50)
                        );

                        for pr in &followed {
                            let state_icon = match pr.last_known_state.as_str() {
                                "merged" => "🔀",
                                "closed" => "❌",
                                "draft" => "📝",
                                _ => "🟢",
                            };
                            let ci_icon = match pr.last_ci_status.as_str() {
                                "success" => "✅",
                                "failure" => "❌",
                                "pending" => "⏳",
                                _ => "❓",
                            };
                            let review_icon = match pr.last_review_state.as_str() {
                                "approved" => "✅",
                                "changes_requested" => "🔁",
                                "commented" => "💬",
                                _ => "─",
                            };

                            // Compute rough priority indicator from size (age not stored in FollowedPr)
                            let priority_display = if priority {
                                let total = pr.additions + pr.deletions;
                                let urgency = if total > 1000 {
                                    "🔴 LARGE"
                                } else if total > 500 {
                                    "🟡 MEDIUM"
                                } else {
                                    "🟢 SMALL"
                                };
                                format!("  {} ({} lines)", urgency, total)
                            } else {
                                String::new()
                            };

                            println!(
                                "  {} {} #{} — {}{}",
                                state_icon,
                                pr.repo.bold(),
                                pr.pr_number,
                                pr.pr_title,
                                priority_display
                            );
                            println!(
                                "      📊 +{}/-{} lines  |  CI: {}  |  Review: {}  |  Author: {}",
                                pr.additions,
                                pr.deletions,
                                ci_icon,
                                review_icon,
                                pr.author.dimmed()
                            );
                        }
                        println!();
                    }
                }

                crate::cli::FollowAction::Remove => {
                    if followed.is_empty() {
                        println!("\n👁️  Not following any PRs.\n");
                        return Ok(());
                    }

                    // Apply --repo filter (partial match, case-insensitive)
                    let followed: Vec<_> = match repo {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.repo.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    // Apply --author filter (partial match, case-insensitive)
                    let mut followed: Vec<_> = match author {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.author.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    if followed.is_empty() {
                        println!("\n👁️  No followed PRs match your filters to remove.\n");
                        return Ok(());
                    }

                    let to_remove: Vec<(String, u64)> = if let Some(ref nums) = pr_numbers {
                        nums.split(',')
                            .filter_map(|part| {
                                let part = part.trim();
                                if let Ok(num) = part.parse::<u64>() {
                                    // Try to find in first repo
                                    Some((
                                        ctx.cfg.github_repos.first().cloned().unwrap_or_default(),
                                        num,
                                    ))
                                } else if part.contains('#') {
                                    let parts: Vec<&str> = part.split('#').collect();
                                    if parts.len() == 2 {
                                        let repo = parts[0].trim().to_string();
                                        let num = parts[1].trim().parse::<u64>().ok()?;
                                        Some((repo, num))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        // Interactive removal
                        println!(
                            "\n👁️  Following {} PR(s) — select to remove:\n",
                            followed.len()
                        );
                        for (i, pr) in followed.iter().enumerate() {
                            println!("  {}: {} #{}", i + 1, pr.repo, pr.pr_number);
                        }
                        print!(
                            "\n{} ",
                            "Select PRs to unfollow [e.g. 1,3 or 1-3 or 'all'] (q to quit):".bold()
                        );
                        io::stdout().flush()?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        match parse_selection(input.trim(), followed.len()) {
                            Selection::Quit => return Ok(()),
                            Selection::Indices(indices) => indices
                                .into_iter()
                                .map(|i| (followed[i].repo.clone(), followed[i].pr_number))
                                .collect(),
                        }
                    };

                    let original_len = followed.len();
                    followed.retain(|e| !to_remove.contains(&(e.repo.clone(), e.pr_number)));
                    let removed = original_len - followed.len();

                    // Save updated list
                    if let Err(e) =
                        std::fs::write(&follow_file, serde_json::to_string_pretty(&followed)?)
                    {
                        println!("  ⚠️ Failed to save follow data: {}", e);
                    } else {
                        println!(
                            "\n👁️  Unfollowed {} PR(s) (now following {}).",
                            removed,
                            followed.len()
                        );
                    }
                }

                crate::cli::FollowAction::Clear => {
                    if followed.is_empty() {
                        println!("\n👁️  Not following any PRs.\n");
                        return Ok(());
                    }
                    followed.clear();
                    if let Err(e) =
                        std::fs::write(&follow_file, serde_json::to_string_pretty(&followed)?)
                    {
                        println!("  ⚠️ Failed to clear follow data: {}", e);
                    } else {
                        println!("\n👁️  Cleared all followed PRs.\n");
                    }
                }

                crate::cli::FollowAction::Status => {
                    if followed.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("\n👁️  Not following any PRs. Run `follow add` first.\n");
                        }
                        return Ok(());
                    }

                    // Apply --repo filter (partial match, case-insensitive)
                    let followed: Vec<_> = match repo {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.repo.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    // Apply --author filter (partial match, case-insensitive)
                    let mut followed: Vec<_> = match author {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            followed
                                .into_iter()
                                .filter(|pr| pr.author.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => followed,
                    };

                    if followed.is_empty() {
                        if json {
                            println!("[]");
                        } else {
                            println!("\n👁️  No followed PRs match your filters.\n");
                        }
                        return Ok(());
                    }

                    if json {
                        // JSON output for scripting - just check for changes without human-readable output
                        #[derive(Serialize)]
                        struct StatusChange {
                            repo: String,
                            pr_number: u64,
                            pr_title: String,
                            state_changed: bool,
                            old_state: String,
                            new_state: String,
                            ci_changed: bool,
                            old_ci: String,
                            new_ci: String,
                            has_new_commit: bool,
                            old_commit: String,
                            new_commit_sha: String,
                        }
                        let mut changes = Vec::new();
                        let now = chrono::Utc::now().to_rfc3339();

                        // Phase 1: Fetch all PR details in parallel
                        let client = octocrab::Octocrab::builder()
                            .personal_token(ctx.cfg.github_token.clone())
                            .build()
                            .expect("failed to build GitHub client");

                        let pr_futures = followed.iter().map(|pr| {
                            let client = client.clone();
                            let org = ctx.cfg.github_org.clone();
                            let repo = pr.repo.clone();
                            let pr_number = pr.pr_number;

                            async move {
                                let current = client.pulls(&org, &repo).get(pr_number).await;
                                (pr_number, repo, current)
                            }
                        });

                        let pr_results: Vec<(
                            u64,
                            String,
                            Result<octocrab::models::pulls::PullRequest, octocrab::Error>,
                        )> = join_all(pr_futures).await;

                        // Phase 2: Fetch CI statuses in parallel for successful PR fetches
                        let ci_futures: Vec<_> = pr_results
                            .iter()
                            .filter_map(|(pr_number, repo, result)| {
                                let pr = result.as_ref().ok()?;
                                let current_commit = pr.head.sha.clone();

                                let client = client.clone();
                                let org = ctx.cfg.github_org.clone();
                                let repo = repo.clone();

                                Some(async move {
                                    #[derive(serde::Deserialize)]
                                    struct CombinedStatus {
                                        state: String,
                                    }

                                    let combined_status_url = format!(
                                        "/repos/{}/{}/commits/{}/status",
                                        org, repo, current_commit
                                    );

                                    let current_ci: String = client
                                        .get(&combined_status_url, None::<&str>)
                                        .await
                                        .map(|s: CombinedStatus| s.state)
                                        .unwrap_or_else(|_| "unknown".to_string());

                                    (*pr_number, current_commit, current_ci)
                                })
                            })
                            .collect();

                        let ci_results: Vec<(u64, String, String)> = join_all(ci_futures).await;
                        let ci_map: std::collections::HashMap<u64, (String, String)> = ci_results
                            .into_iter()
                            .map(|(pr_number, commit, ci)| (pr_number, (commit, ci)))
                            .collect();

                        // Process and collect changes
                        for pr in followed.iter_mut() {
                            let pr_result =
                                pr_results.iter().find(|(num, _, _)| *num == pr.pr_number);
                            let current = match pr_result.and_then(|(_, _, r)| r.as_ref().ok()) {
                                Some(current) => current,
                                None => continue,
                            };

                            let current_state = if current.merged.unwrap_or(false) {
                                "merged"
                            } else {
                                match current.state.as_ref() {
                                    Some(octocrab::models::IssueState::Open) => "open",
                                    Some(octocrab::models::IssueState::Closed) => "closed",
                                    _ => "unknown",
                                }
                            };

                            let current_commit = current.head.sha.clone();

                            let current_ci = ci_map
                                .get(&pr.pr_number)
                                .map(|(_, ci)| ci.clone())
                                .unwrap_or_else(|| "unknown".to_string());

                            let state_changed = pr.last_known_state != current_state;
                            let ci_changed = pr.last_ci_status != current_ci;
                            let new_commit = pr.last_commit_sha != current_commit;

                            if state_changed || ci_changed || new_commit {
                                changes.push(StatusChange {
                                    repo: pr.repo.clone(),
                                    pr_number: pr.pr_number,
                                    pr_title: pr.pr_title.clone(),
                                    state_changed,
                                    old_state: pr.last_known_state.clone(),
                                    new_state: current_state.to_string(),
                                    ci_changed,
                                    old_ci: pr.last_ci_status.clone(),
                                    new_ci: current_ci.clone(),
                                    has_new_commit: new_commit,
                                    old_commit: pr.last_commit_sha
                                        [..7.min(pr.last_commit_sha.len())]
                                        .to_string(),
                                    new_commit_sha: current_commit[..7.min(current_commit.len())]
                                        .to_string(),
                                });

                                // Update stored state
                                pr.last_known_state = current_state.to_string();
                                pr.last_ci_status = current_ci;
                                pr.last_commit_sha = current_commit;
                                pr.last_check = now.clone();
                            }
                        }

                        println!("{}", serde_json::to_string_pretty(&changes)?);

                        // Save updated status
                        if let Err(e) =
                            std::fs::write(&follow_file, serde_json::to_string_pretty(&followed)?)
                        {
                            println!("  ⚠️ Failed to update follow data: {}", e);
                        }
                        return Ok(());
                    }

                    println!(
                        "\n🔍 Checking status of {} followed PR(s)...\n",
                        followed.len().to_string().yellow().bold()
                    );

                    let client = octocrab::Octocrab::builder()
                        .personal_token(ctx.cfg.github_token.clone())
                        .build()
                        .expect("failed to build GitHub client");

                    // Phase 1: Fetch all PR details in parallel
                    let pr_futures = followed.iter().map(|pr| {
                        let client = client.clone();
                        let org = ctx.cfg.github_org.clone();
                        let repo = pr.repo.clone();
                        let pr_number = pr.pr_number;

                        async move {
                            let current = client.pulls(&org, &repo).get(pr_number).await;
                            (pr_number, repo, current)
                        }
                    });

                    let pr_results: Vec<(
                        u64,
                        String,
                        Result<octocrab::models::pulls::PullRequest, octocrab::Error>,
                    )> = join_all(pr_futures).await;

                    // Phase 2: Fetch CI statuses in parallel for successful PR fetches
                    let ci_futures: Vec<_> = pr_results
                        .iter()
                        .filter_map(|(pr_number, repo, result)| {
                            let pr = result.as_ref().ok()?;
                            let current_commit = pr.head.sha.clone();

                            let client = client.clone();
                            let org = ctx.cfg.github_org.clone();
                            let repo = repo.clone();

                            Some(async move {
                                #[derive(serde::Deserialize)]
                                struct CombinedStatus {
                                    state: String,
                                }

                                let combined_status_url = format!(
                                    "/repos/{}/{}/commits/{}/status",
                                    org, repo, current_commit
                                );

                                let current_ci: String = client
                                    .get(&combined_status_url, None::<&str>)
                                    .await
                                    .map(|s: CombinedStatus| s.state)
                                    .unwrap_or_else(|_| "unknown".to_string());

                                (*pr_number, current_commit, current_ci)
                            })
                        })
                        .collect();

                    let ci_results: Vec<(u64, String, String)> = join_all(ci_futures).await;
                    let ci_map: std::collections::HashMap<u64, (String, String)> = ci_results
                        .into_iter()
                        .map(|(pr_number, commit, ci)| (pr_number, (commit, ci)))
                        .collect();

                    let now = chrono::Utc::now().to_rfc3339();
                    let mut has_changes = false;

                    // Process results and check for changes
                    for pr in followed.iter_mut() {
                        // Find PR result
                        let pr_result = pr_results.iter().find(|(num, _, _)| *num == pr.pr_number);
                        let current = match pr_result.and_then(|(_, _, r)| r.as_ref().ok()) {
                            Some(current) => current,
                            None => continue,
                        };

                        let current_state = if current.merged.unwrap_or(false) {
                            "merged"
                        } else {
                            match current.state.as_ref() {
                                Some(octocrab::models::IssueState::Open) => "open",
                                Some(octocrab::models::IssueState::Closed) => "closed",
                                _ => "unknown",
                            }
                        };

                        let current_commit = current.head.sha.clone();

                        // Get CI status from map
                        let current_ci = ci_map
                            .get(&pr.pr_number)
                            .map(|(_, ci)| ci.clone())
                            .unwrap_or_else(|| "unknown".to_string());

                        // Check for changes
                        let state_changed = pr.last_known_state != current_state;
                        let ci_changed = pr.last_ci_status != current_ci;
                        let new_commit = pr.last_commit_sha != current_commit;

                        if state_changed || ci_changed || new_commit {
                            has_changes = true;
                            println!(
                                "  🔔 {} #{} — {}",
                                pr.repo.bold(),
                                pr.pr_number,
                                pr.pr_title
                            );

                            if state_changed {
                                println!(
                                    "      Status: {} → {}",
                                    pr.last_known_state.yellow(),
                                    current_state.green()
                                );
                            }
                            if new_commit {
                                println!(
                                    "      Commit: {} → {}",
                                    &pr.last_commit_sha[..7.min(pr.last_commit_sha.len())].yellow(),
                                    &current_commit[..7.min(current_commit.len())].green()
                                );
                            }
                            if ci_changed {
                                println!(
                                    "      CI: {} → {}",
                                    pr.last_ci_status.yellow(),
                                    current_ci.green()
                                );
                            }
                            println!();

                            // Update stored state
                            pr.last_known_state = current_state.to_string();
                            pr.last_ci_status = current_ci;
                            pr.last_commit_sha = current_commit;
                            pr.last_check = now.clone();
                        }
                    }

                    if !has_changes {
                        println!("  ✅ No changes detected in followed PRs.\n");
                    }

                    // Save updated status
                    if let Err(e) =
                        std::fs::write(&follow_file, serde_json::to_string_pretty(&followed)?)
                    {
                        println!("  ⚠️ Failed to update follow data: {}", e);
                    }
                }
            }
        }

        Commands::Chase {
            pr_number,
            pr,
            pr_numbers,
            min_age,
            since_days,
            dry_run,
            send,
            message,
            repo,
            author,
            priority,
            json,
            quiet,
        } => {
            // Warn user this is a dry run by default
            if !send {
                println!("\n🔍 DRY RUN MODE - Use --send to actually post comments\n");
            } else {
                println!("\n⚠️  Will post chase comments to GitHub!\n");
            }
            use chrono::{Duration, Utc};

            let min_age_days = min_age as i64;
            let now = Utc::now();
            let cutoff = now - Duration::days(min_age_days);

            // Handle --pr filter (global --pr > local --pr > local --pr-number)
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Build initial filtered list: either target specific PR(s) or use all ctx.reviews
            let pr_filtered: Vec<_> = if let Some(num) = target_pr {
                // Single PR targeted - use fetch_pr_by_number for accurate data
                match github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await
                {
                    Ok(prs) => prs,
                    Err(e) => {
                        println!("\n❌ Failed to fetch PR #{}: {}\n", num, e);
                        return Ok(());
                    }
                }
            } else if let Some(ref nums) = pr_numbers {
                // Multiple PRs targeted - fetch all in parallel
                let pr_nums: Vec<u64> = nums
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                let mut results = Vec::new();
                let fetch_futures = pr_nums.iter().map(|&num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        num,
                    )
                });
                let fetched = join_all(fetch_futures).await;
                for result in fetched {
                    match result {
                        Ok(prs) => results.extend(prs),
                        Err(e) => eprintln!("Warning: Failed to fetch PR: {}", e),
                    }
                }
                results
            } else {
                ctx.reviews.clone()
            };

            // Filter stale PRs (older than min_age)
            let mut stale_prs: Vec<_> = pr_filtered
                .iter()
                .filter(|r| r.created_at <= cutoff)
                .cloned()
                .collect();

            // Apply --since-days filter (only PRs created since X days ago)
            if let Some(days) = since_days {
                let since_cutoff = now - Duration::days(days as i64);
                stale_prs.retain(|r| r.created_at >= since_cutoff);
            }

            // Apply --repo filter
            if let Some(ref repo_filter) = repo {
                let pattern = repo_filter.to_lowercase();
                stale_prs.retain(|r| r.repo.to_lowercase().contains(&pattern));
            }

            // Apply --author filter
            if let Some(ref author_filter) = author {
                let pattern = author_filter.to_lowercase();
                stale_prs.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
            }

            if stale_prs.is_empty() {
                println!(
                    "\n🎉 No stale PRs older than {} day(s) to chase!\n",
                    min_age
                );
                return Ok(());
            }

            // Filter out PRs where the user has already commented
            let username = ctx.cfg.github_username.clone();
            let token = ctx.cfg.github_token.clone();
            let org = ctx.cfg.github_org.clone();

            let mut prs_to_chase = Vec::new();
            for pr in stale_prs {
                let already_commented =
                    github::has_user_commented(&token, &org, &pr.repo, pr.pr_number, &username)
                        .await;
                match already_commented {
                    Ok(true) => {
                        println!("\n⏭️  Skipping #{} - you already commented", pr.pr_number);
                    }
                    Ok(false) => {
                        prs_to_chase.push(pr);
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: Could not check comments for #{}: {}",
                            pr.pr_number, e
                        );
                        prs_to_chase.push(pr); // Include anyway on error
                    }
                }
            }

            if prs_to_chase.is_empty() {
                println!("\n🎉 No PRs to chase (all already commented or errors)!\n");
                return Ok(());
            }

            // Default chase message template
            let default_message = "👋 Hi @{author}! Just checking in on this PR — it's been waiting for review for {days} days. Could you please address any pending feedback or let us know if it's ready for another look? Thanks!";

            let message_template = message.as_deref().unwrap_or(default_message);

            // Build chase entries
            #[derive(Debug, Clone, serde::Serialize)]
            struct ChaseEntry {
                pub repo: String,
                pub pr_number: u64,
                pub pr_title: String,
                pub author: String,
                pub days_waiting: i64,
                pub message: String,
                pub pr_url: String,
                pub priority_score: u8,
            }

            let chase_entries: Vec<ChaseEntry> = prs_to_chase
                .iter()
                .map(|r| {
                    let days_waiting = (now - r.created_at).num_days();
                    let priority_score = logger::calculate_priority_score(r);
                    let msg = message_template
                        .replace("{author}", &r.pr_author)
                        .replace("{title}", &r.pr_title)
                        .replace("{days}", &days_waiting.to_string())
                        .replace("{repo}", &r.repo)
                        .replace("{pr}", &format!("#{}", r.pr_number));

                    ChaseEntry {
                        repo: r.repo.clone(),
                        pr_number: r.pr_number,
                        pr_title: r.pr_title.clone(),
                        author: r.pr_author.clone(),
                        days_waiting,
                        message: msg,
                        pr_url: r.pr_url.clone(),
                        priority_score,
                    }
                })
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&chase_entries)?);
                return Ok(());
            }

            println!(
                "\n🐢 Chasing {} stale PR(s) (older than {} days)...\n",
                chase_entries.len().to_string().yellow().bold(),
                min_age.to_string().cyan()
            );

            for entry in &chase_entries {
                let age_label = if entry.days_waiting >= 14 {
                    format!("{}d", entry.days_waiting).red().to_string()
                } else if entry.days_waiting >= 7 {
                    format!("{}d", entry.days_waiting).yellow().to_string()
                } else {
                    format!("{}d", entry.days_waiting).dimmed().to_string()
                };

                let priority_display = if priority {
                    format!("  {}", logger::priority_stars(entry.priority_score))
                } else {
                    String::new()
                };

                println!(
                    "  📬 {} {} (#{}) - {}{}",
                    entry.pr_title.bold(),
                    entry.author.cyan().to_string().as_str(),
                    entry.pr_number,
                    age_label,
                    priority_display
                );
                println!("     💬 {}\n", entry.message.dimmed());
            }

            if send {
                println!(
                    "\n📤 Sending {} chase comment(s) in parallel...\n",
                    chase_entries.len()
                );

                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();

                // Send all comments in parallel
                let send_futures = chase_entries.iter().map(|entry| {
                    let token = token.clone();
                    let org = org.clone();
                    let repo = entry.repo.clone();
                    let message = entry.message.clone();
                    let pr_number = entry.pr_number;
                    async move {
                        let client = octocrab::Octocrab::builder()
                            .personal_token(token)
                            .build()
                            .expect("failed to build GitHub client");
                        client
                            .issues(&org, &repo)
                            .create_comment(pr_number, &message)
                            .await
                    }
                });
                let results = join_all(send_futures).await;

                let mut sent = 0;
                let mut failed = 0;

                for (entry, result) in chase_entries.iter().zip(results) {
                    match result {
                        Ok(_) => {
                            if !quiet {
                                println!(
                                    "  ✅ Sent: #{} - {}",
                                    entry.pr_number,
                                    entry.pr_title.dimmed()
                                );
                            }
                            sent += 1;
                        }
                        Err(e) => {
                            if !quiet {
                                println!(
                                    "  ❌ Failed: #{} - {} ({})",
                                    entry.pr_number,
                                    entry.pr_title.dimmed(),
                                    e
                                );
                            }
                            failed += 1;
                        }
                    }
                }

                if !quiet {
                    println!(
                        "\n📊 Sent: {}, Failed: {}\n",
                        sent.to_string().green(),
                        failed.to_string().red()
                    );
                } else {
                    println!("Sent: {}, Failed: {}", sent, failed);
                }
            } else {
                // Preview mode (dry_run or default - preview already shown above)
                if dry_run {
                    println!("  (dry-run)\n");
                } else {
                    println!(
                        "  💡 Use --dry-run to preview or --send to post comments to GitHub\n"
                    );
                }
            }
        }

        Commands::ReviewTime {
            pr_number,
            pr,
            pr_numbers,
            all,
            grouped,
            priority,
            repo,
            author,
            since_days,
            json,
        } => {
            // Target specific PR: global --pr flag > local --pr > local pr_number
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo and --author and --since-days filters first (consistent with other commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            // Target specific PR via --pr or positional
            let targets: Vec<_> = if let Some(num) = target_pr {
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let mut all_prs = Vec::new();
                for prs in join_all(fetch_futures).await.into_iter().flatten() {
                    all_prs.extend(prs);
                }
                all_prs
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                logger::print_reviews(&filtered_reviews, false);
                print!(
                    "\n{} ",
                    "Select PRs to estimate review time [e.g. 1,3 or 1-3 or 'all'] (q to quit):"
                        .bold()
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match parse_selection(input.trim(), filtered_reviews.len()) {
                    Selection::Quit => return Ok(()),
                    Selection::Indices(indices) => indices
                        .into_iter()
                        .map(|i| filtered_reviews[i].clone())
                        .collect(),
                }
            };

            if targets.is_empty() {
                println!("No PRs to estimate review time for.");
                return Ok(());
            }

            // Parallel fetch file counts for all PRs
            let file_count_futures = targets.iter().map(|review| {
                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();
                let repo = review.repo.clone();
                let pr_number = review.pr_number;
                async move {
                    match github::fetch_pr_files(&token, &org, &repo, pr_number).await {
                        Ok(files) => (review.pr_number, Some(files.len() as u64)),
                        Err(_) => (review.pr_number, None),
                    }
                }
            });
            let file_count_results = join_all(file_count_futures).await;
            let file_count_map: std::collections::HashMap<u64, u64> = file_count_results
                .into_iter()
                .filter_map(|(pr_num, count)| count.map(|c| (pr_num, c)))
                .collect();

            // Fetch file counts for each PR to improve estimates
            #[derive(Debug, Clone, serde::Serialize)]
            struct ReviewTimeEstimate {
                repo: String,
                pr_number: u64,
                pr_title: String,
                author: String,
                additions: u64,
                deletions: u64,
                total_lines: u64,
                file_count: Option<u64>,
                estimated_minutes: u32,
                time_category: String,
                size_category: String,
                age_days: i64,
                priority_score: u8,
                draft: bool,
            }

            let mut estimates: Vec<ReviewTimeEstimate> = Vec::new();
            let now = chrono::Utc::now();

            for review in &targets {
                let total_lines = review.additions + review.deletions;

                // Estimate review time based on lines changed
                // Baseline: ~2 min per 50 lines, adjusted by complexity
                let base_minutes = (total_lines as f64 / 50.0 * 2.0) as u32;

                // Complexity multipliers:
                // - XS (<50 lines): 0.8x (quick review)
                // - S (50-200): 1.0x (standard)
                // - M (200-500): 1.2x (moderate complexity)
                // - L (500-1000): 1.5x (higher complexity)
                // - XL (>1000): 2.0x (large change, likely complex)
                let (complexity_mult, size_cat) = if total_lines < 50 {
                    (0.8, "XS")
                } else if total_lines < 200 {
                    (1.0, "S")
                } else if total_lines < 500 {
                    (1.2, "M")
                } else if total_lines < 1000 {
                    (1.5, "L")
                } else {
                    (2.0, "XL")
                };

                // Age adjustment: older PRs take slightly less time (context lost)
                let age_days = (now - review.created_at).num_days();
                let age_factor = if age_days > 14 { 0.9 } else { 1.0 };

                let estimated_minutes =
                    ((base_minutes as f64 * complexity_mult * age_factor).ceil() as u32).max(5);

                // Time categories
                let time_category = if estimated_minutes < 10 {
                    "⚡ lightning".to_string()
                } else if estimated_minutes < 20 {
                    "🔵 quick".to_string()
                } else if estimated_minutes < 45 {
                    "🟡 moderate".to_string()
                } else if estimated_minutes < 90 {
                    "🟠 substantial".to_string()
                } else {
                    "🔴 lengthy".to_string()
                };

                let priority_score = logger::calculate_priority_score(review);

                estimates.push(ReviewTimeEstimate {
                    repo: review.repo.clone(),
                    pr_number: review.pr_number,
                    pr_title: review.pr_title.clone(),
                    author: review.pr_author.clone(),
                    additions: review.additions,
                    deletions: review.deletions,
                    total_lines,
                    file_count: file_count_map.get(&review.pr_number).copied(),
                    estimated_minutes,
                    time_category,
                    size_category: size_cat.to_string(),
                    age_days,
                    priority_score,
                    draft: review.draft,
                });
            }

            // Sort by estimated time (longest first for planning)
            estimates.sort_by(|a, b| b.estimated_minutes.cmp(&a.estimated_minutes));

            if json {
                println!("{}", serde_json::to_string_pretty(&estimates)?);
                return Ok(());
            }

            let total_minutes: u32 = estimates.iter().map(|e| e.estimated_minutes).sum();
            let total_hours = total_minutes as f64 / 60.0;

            // Time bucket definitions: (label, emoji, min_minutes, max_minutes)
            #[derive(Clone, Copy)]
            struct TimeBucket(&'static str, &'static str, u32, Option<u32>);
            const TIME_BUCKETS: [TimeBucket; 5] = [
                TimeBucket("⚡ lightning", "⚡", 0, Some(10)),
                TimeBucket("🔵 quick", "🔵", 10, Some(20)),
                TimeBucket("🟡 moderate", "🟡", 20, Some(45)),
                TimeBucket("🟠 substantial", "🟠", 45, Some(90)),
                TimeBucket("🔴 lengthy", "🔴", 90, None),
            ];

            if grouped {
                // Grouped view: organize by time category
                let mut buckets: Vec<(TimeBucket, Vec<&ReviewTimeEstimate>)> =
                    TIME_BUCKETS.iter().cloned().map(|b| (b, vec![])).collect();

                for est in &estimates {
                    for (bucket, prs) in &mut buckets {
                        let TimeBucket(_, _, min, max) = *bucket;
                        let in_bucket = if let Some(max) = max {
                            est.estimated_minutes >= min && est.estimated_minutes < max
                        } else {
                            est.estimated_minutes >= min
                        };
                        if in_bucket {
                            prs.push(est);
                            break;
                        }
                    }
                }

                println!(
                    "\n⏱️  Review Time Estimates — {} PRs, ~{:.1}h total (grouped by time)\n{}",
                    estimates.len(),
                    total_hours,
                    "─".repeat(55)
                );

                let mut any_shown = false;
                for (bucket, prs) in &buckets {
                    if prs.is_empty() {
                        continue;
                    }
                    any_shown = true;
                    let TimeBucket(label, emoji, min, max) = *bucket;
                    let range_str = if let Some(max) = max {
                        format!("{} to {} min", min, max)
                    } else {
                        format!("{}+ min", min)
                    };

                    let bucket_minutes: u32 = prs.iter().map(|e| e.estimated_minutes).sum();
                    let bucket_hours = bucket_minutes as f64 / 60.0;

                    println!(
                        "\n{} {} {} ({} PRs, {:.1}h total)\n{}",
                        emoji,
                        label.bold(),
                        range_str.dimmed(),
                        prs.len(),
                        bucket_hours,
                        "─".repeat(40)
                    );

                    for est in prs {
                        let size_color: colored::Color = match est.size_category.as_str() {
                            "XS" | "S" => colored::Color::Green,
                            "M" => colored::Color::Yellow,
                            "L" => colored::Color::Red,
                            _ => colored::Color::Magenta,
                        };

                        let time_str = if est.estimated_minutes < 60 {
                            format!("{} min", est.estimated_minutes)
                        } else {
                            let hours = est.estimated_minutes as f64 / 60.0;
                            format!("{:.1}h", hours)
                        };

                        let stars = "★".repeat(est.priority_score as usize);

                        println!(
                            "  {} {}  #{}  {}\n     👤 {}  •  📦 {} ({} lines)  •  ⏱️ {}  {}{}",
                            est.size_category.color(size_color),
                            est.pr_title.bold(),
                            est.pr_number,
                            est.repo.dimmed(),
                            est.author.cyan(),
                            est.size_category,
                            est.total_lines,
                            time_str.green(),
                            stars.red(),
                            if est.draft {
                                " 📝DRAFT".yellow().to_string()
                            } else {
                                String::new()
                            }
                        );
                    }
                }

                if !any_shown {
                    println!("\n  No PRs to estimate.\n");
                }
                println!("\n{}", "─".repeat(55));
                println!(
                    "  📊 Total review time: {:.1} hours ({} minutes)",
                    total_hours, total_minutes
                );
                println!("  💡 Use `--grouped` to see PRs organized by time category");
                println!("  💡 Use `--json` for scripting\n");
            } else {
                // Flat view: sorted by time (longest first)
                println!(
                    "\n⏱️  Review Time Estimates — {} PRs, ~{:.1}h total\n{}",
                    estimates.len(),
                    total_hours,
                    "─".repeat(55)
                );

                for est in &estimates {
                    let size_color: colored::Color = match est.size_category.as_str() {
                        "XS" | "S" => colored::Color::Green,
                        "M" => colored::Color::Yellow,
                        "L" => colored::Color::Red,
                        _ => colored::Color::Magenta,
                    };

                    let time_str = if est.estimated_minutes < 60 {
                        format!("{} min", est.estimated_minutes)
                    } else {
                        let hours = est.estimated_minutes as f64 / 60.0;
                        format!("{:.1}h", hours)
                    };

                    let priority_display = if priority {
                        format!("  ⭐ {}/5", est.priority_score)
                    } else {
                        String::new()
                    };

                    println!(
                        "  {} {}  #{}  {}\n     👤 {}  •  📦 {} ({} lines)  •  ⏱️ {}  {}{}",
                        est.size_category.color(size_color),
                        est.pr_title.bold(),
                        est.pr_number,
                        est.repo.dimmed(),
                        est.author.cyan(),
                        est.size_category,
                        est.total_lines,
                        time_str.green(),
                        est.time_category,
                        priority_display
                    );
                    println!();
                }

                println!("{}", "─".repeat(55));
                println!(
                    "  📊 Total review time: {:.1} hours ({} minutes)",
                    total_hours, total_minutes
                );
                println!(
                    "  💡 Time estimates based on lines changed, adjusted for size complexity"
                );
                println!("  💡 Use `--grouped` to see PRs organized by time category");
                println!("  💡 Use `--priority` or `-P` to show priority scores");
                println!("  💡 Use `--json` for scripting\n");
            }
        }

        Commands::Report {
            pr_number,
            pr_numbers,
            all,
            days,
            json,
            repo,
            author,
            priority,
            since_days,
        } => {
            use chrono::{Duration, Utc};
            use std::collections::HashMap;

            let report_output_dir = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"));

            if !report_output_dir.exists() {
                println!(
                    "❌ No reviews directory found at {}. Run `prctrl list` first to save reviews.",
                    report_output_dir.display()
                );
                return Ok(());
            }

            // Target specific PR: global --pr flag > local --pr_number > pr_numbers
            let target_pr = ctx.cli.pr.or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                Vec::new()
            };

            // If pr_numbers was provided, we're done (show report for those specific PRs)
            if pr_numbers.is_some() {
                if prs_from_numbers.is_empty() {
                    println!("No PRs found for the specified numbers.");
                    return Ok(());
                }

                // Apply --repo and --author filters to the fetched PRs
                let filtered: Vec<_> = {
                    let mut result = prs_from_numbers;

                    if let Some(ref repo_filter) = repo {
                        let pattern = repo_filter.to_lowercase();
                        result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                    }

                    if let Some(ref author_filter) = author {
                        let pattern = author_filter.to_lowercase();
                        result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                    }

                    result
                };

                if json {
                    let filtered_prs_json: Vec<serde_json::Value> = filtered
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            let age_days = (chrono::Utc::now() - r.created_at).num_days() as u32;
                            serde_json::json!({
                                "repo": r.repo,
                                "number": r.pr_number,
                                "title": r.pr_title,
                                "author": r.pr_author,
                                "additions": r.additions,
                                "deletions": r.deletions,
                                "age_days": age_days,
                                "priority_score": score,
                                "draft": r.draft,
                                "url": r.pr_url
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&filtered_prs_json)?);
                } else {
                    println!("\n📊 Report for {} PR(s)\n", filtered.len());
                    logger::print_reviews(&filtered, priority);
                }
                return Ok(());
            }

            // Apply filters to pending ctx.reviews for display (--pr, --repo, --author, --all)
            let filtered_reviews: Vec<_> = if all {
                // --all flag: use all pending ctx.reviews (skip filters, but still apply --repo/--author)
                let mut result = ctx.reviews.clone();

                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            } else {
                let mut result = ctx.reviews.clone();

                // Filter by specific PR number (if provided via --pr)
                if let Some(num) = target_pr {
                    result.retain(|r| r.pr_number == num);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter (consistent with other commands - PRs created within N days)
                if let Some(days_filter) = since_days {
                    let cutoff = Utc::now() - Duration::days(days_filter as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            // Read all review files from the output directory
            let mut processed_count = 0u32;
            let mut processed_by_author: HashMap<String, u32> = HashMap::new();
            let mut processed_by_repo: HashMap<String, u32> = HashMap::new();
            let mut recent_reviews: Vec<(String, chrono::DateTime<Utc>, String)> = vec![];

            let cutoff = Utc::now() - Duration::days(days as i64);

            if let Ok(entries) = std::fs::read_dir(&report_output_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            let lines: Vec<&str> = content.lines().collect();
                            if lines.len() >= 4 {
                                let pr_title = lines.first().unwrap_or(&"").trim();
                                let date_line = lines.iter().find(|l| l.starts_with("Reviewed on"));
                                if let Some(date_str) = date_line {
                                    if let Some(date_part) = date_str.strip_prefix("Reviewed on ") {
                                        if let Ok(date) =
                                            chrono::DateTime::parse_from_rfc3339(date_part)
                                        {
                                            let date = date.with_timezone(&Utc);
                                            if date >= cutoff {
                                                processed_count += 1;
                                                recent_reviews.push((
                                                    pr_title.to_string(),
                                                    date,
                                                    path.file_name()
                                                        .unwrap_or_default()
                                                        .to_string_lossy()
                                                        .to_string(),
                                                ));

                                                for line in &lines {
                                                    if line.starts_with("- **Author**:") {
                                                        if let Some(a) =
                                                            line.strip_prefix("- **Author**:")
                                                        {
                                                            let a = a.trim();
                                                            // Apply --author filter if specified
                                                            if author.is_none()
                                                                || a.to_lowercase().contains(
                                                                    &author
                                                                        .as_ref()
                                                                        .unwrap()
                                                                        .to_lowercase(),
                                                                )
                                                            {
                                                                *processed_by_author
                                                                    .entry(a.to_string())
                                                                    .or_insert(0) += 1;
                                                            }
                                                        }
                                                    }
                                                    if line.starts_with("- **Repository**:") {
                                                        if let Some(r) =
                                                            line.strip_prefix("- **Repository**:")
                                                        {
                                                            let r = r.trim();
                                                            // Apply --repo filter if specified
                                                            if repo.is_none()
                                                                || r.to_lowercase().contains(
                                                                    &repo
                                                                        .as_ref()
                                                                        .unwrap()
                                                                        .to_lowercase(),
                                                                )
                                                            {
                                                                *processed_by_repo
                                                                    .entry(r.to_string())
                                                                    .or_insert(0) += 1;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let pending_total = filtered_reviews.len();
            let pending_old = filtered_reviews
                .iter()
                .filter(|r| {
                    let age_days = (Utc::now() - r.created_at).num_days();
                    age_days >= days as i64
                })
                .count();

            let pending_additions: u64 = filtered_reviews.iter().map(|r| r.additions).sum();
            let pending_deletions: u64 = filtered_reviews.iter().map(|r| r.deletions).sum();

            // Include filtered PR details in JSON output when targeting specific PR
            let filtered_prs_json: Vec<serde_json::Value> = filtered_reviews
                .iter()
                .map(|r| {
                    let score = logger::calculate_priority_score(r);
                    let age_days = (chrono::Utc::now() - r.created_at).num_days() as u32;
                    serde_json::json!({
                        "repo": r.repo,
                        "number": r.pr_number,
                        "title": r.pr_title,
                        "author": r.pr_author,
                        "additions": r.additions,
                        "deletions": r.deletions,
                        "age_days": age_days,
                        "priority_score": score,
                        "draft": r.draft,
                        "url": r.pr_url
                    })
                })
                .collect();

            let report = serde_json::json!({
                "period_days": days,
                "generated_at": Utc::now().to_rfc3339(),
                "target_pr": target_pr,
                "processed_reviews": processed_count,
                "pending_reviews": pending_total,
                "pending_old_count": pending_old,
                "pending_additions": pending_additions,
                "pending_deletions": pending_deletions,
                "by_author": processed_by_author,
                "by_repository": processed_by_repo,
                "filtered_prs": filtered_prs_json
            });

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "\n📊 Weekly Review Report  ({}-day period)\n{}",
                    days,
                    "─".repeat(45)
                );
                println!(
                    "  📁 Directory: {}",
                    report_output_dir.display().to_string().dimmed()
                );
                println!();
                println!("  ✅ Processed Reviews:");
                println!("     Total reviewed:     {}", processed_count);
                if !processed_by_author.is_empty() {
                    println!("     By author:");
                    let mut sorted: Vec<_> = processed_by_author.iter().collect();
                    sorted.sort_by(|a, b| b.1.cmp(a.1));
                    for (author, count) in sorted.iter().take(5) {
                        println!("       {}: {}", author.cyan(), count);
                    }
                }
                if !processed_by_repo.is_empty() {
                    println!("     By repository:");
                    let mut sorted: Vec<_> = processed_by_repo.iter().collect();
                    sorted.sort_by(|a, b| b.1.cmp(a.1));
                    for (repo, count) in sorted.iter().take(5) {
                        println!("       {}: {}", repo, count);
                    }
                }
                println!();
                println!("  ⏳ Current Pending:");
                println!("     Total pending:       {}", pending_total);
                println!("     Old ({}d+):          {}", days, pending_old);
                println!(
                    "     Lines pending:       +{} / -{}",
                    pending_additions.to_string().green(),
                    pending_deletions.to_string().red()
                );
                println!();

                // Priority breakdown if --priority flag is set
                if priority && !filtered_reviews.is_empty() {
                    println!("  ⭐ Priority Breakdown:");
                    let mut scored: Vec<_> = filtered_reviews
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            (r, score)
                        })
                        .collect();
                    scored.sort_by_key(|b| std::cmp::Reverse(b.1)); // highest priority first

                    // Group by score
                    let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> = HashMap::new();
                    for (review, score) in &scored {
                        score_groups.entry(*score).or_default().push(review);
                    }

                    for score in (1..=5).rev() {
                        if let Some(prs) = score_groups.get(&score) {
                            let stars = logger::priority_stars(score);
                            let age_days = (chrono::Utc::now() - prs[0].created_at).num_days();
                            let age_str = if age_days == 0 {
                                "today".to_string()
                            } else if age_days == 1 {
                                "1 day".to_string()
                            } else {
                                format!("{} days", age_days)
                            };
                            println!(
                                "    ⭐{}  {} PR(s)  •  oldest: {}  •  +{}/-{} lines",
                                stars,
                                prs.len(),
                                age_str,
                                prs.iter().map(|r| r.additions).sum::<u64>(),
                                prs.iter().map(|r| r.deletions).sum::<u64>()
                            );
                        }
                    }
                    println!();
                }

                if !recent_reviews.is_empty() {
                    println!("  🕐 Recent Activity (last {} days):", days);
                    recent_reviews.sort_by(|a, b| b.1.cmp(&a.1));
                    for (title, date, _) in recent_reviews.iter().take(5) {
                        let days_ago = (Utc::now() - *date).num_days();
                        let when = if days_ago == 0 {
                            "today".green()
                        } else if days_ago == 1 {
                            "yesterday".normal()
                        } else {
                            format!("{} days ago", days_ago).dimmed()
                        };
                        println!("     • {} ({})", title.bold(), when);
                    }
                    println!();
                }

                println!("  💡 Tip: Use `--json` flag for machine-readable output");
                println!("{}", "─".repeat(45));
            }
        }

        Commands::Activity {
            days,
            pr_numbers: _pr_numbers,
            all: _all,
            repo,
            author,
            pr,
            json,
            priority,
            since_days,
        } => {
            println!(
                "\n📈 Fetching your review activity (last {} days)...\n",
                days
            );

            match github::fetch_my_review_activity(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &ctx.cfg.github_repos,
                &ctx.cfg.github_username,
                days,
            )
            .await
            {
                Ok(mut activities) => {
                    // Apply --pr filter (shorthand for global --pr)
                    if let Some(target_pr) = ctx.cli.pr.or(pr) {
                        activities.retain(|a| a.pr_number == target_pr);
                    }

                    // Apply --repo filter (partial match, case-insensitive)
                    if let Some(ref repo_filter) = repo {
                        let pattern = repo_filter.to_lowercase();
                        activities.retain(|a| a.repo.to_lowercase().contains(&pattern));
                    }

                    // Apply --author filter (partial match, case-insensitive)
                    if let Some(ref author_filter) = author {
                        let pattern = author_filter.to_lowercase();
                        activities.retain(|a| a.author.to_lowercase().contains(&pattern));
                    }

                    // Apply --since-days filter (PRs reviewed within N days)
                    if let Some(since) = since_days {
                        let cutoff = chrono::Utc::now() - chrono::Duration::days(since as i64);
                        activities.retain(|a| a.reviewed_at >= cutoff);
                    }

                    // When --priority is specified, fetch PR details in parallel for priority scoring
                    // Use a HashMap to avoid index misalignment when fetches fail
                    let priority_scores: Option<std::collections::HashMap<(String, u64), u8>> =
                        if priority {
                            let fetch_futures = activities.iter().map(|activity| {
                                github::fetch_pr_by_number(
                                    &ctx.cfg.github_token,
                                    &ctx.cfg.github_org,
                                    &ctx.cfg.github_repos,
                                    activity.pr_number,
                                )
                            });
                            let results: Vec<Result<Vec<github::PendingReview>, anyhow::Error>> =
                                join_all(fetch_futures).await;
                            let mut score_map: std::collections::HashMap<(String, u64), u8> =
                                std::collections::HashMap::new();
                            for (activity, result) in activities.iter().zip(results) {
                                if let Ok(prs) = result {
                                    if let Some(pr) = prs.into_iter().next() {
                                        score_map.insert(
                                            (activity.repo.clone(), activity.pr_number),
                                            logger::calculate_priority_score(&pr),
                                        );
                                    }
                                }
                            }
                            Some(score_map)
                        } else {
                            None
                        };

                    if json {
                        println!("{}", serde_json::to_string_pretty(&activities)?);
                    } else {
                        if activities.is_empty() {
                            println!("\n  😴 No review activity found.\n");
                        } else {
                            println!(
                                "📊 Your Review Activity  (last {} days)\n{}",
                                days,
                                "─".repeat(45)
                            );
                            println!("  Total PRs reviewed:  {}", activities.len());
                            // Group by day
                            use std::collections::HashMap;
                            let mut by_day: HashMap<String, Vec<_>> = HashMap::new();
                            for activity in &activities {
                                let day = activity.reviewed_at.format("%Y-%m-%d").to_string();
                                by_day.entry(day).or_default().push(activity);
                            }

                            // Show by-day breakdown
                            let mut days_sorted: Vec<_> = by_day.keys().collect();
                            days_sorted.sort_by(|a, b| b.cmp(a)); // newest first

                            for day in days_sorted {
                                let items = by_day.get(day).unwrap();

                                // Count by state
                                let approved = items
                                    .iter()
                                    .filter(|a| a.state.contains("APPROVED"))
                                    .count();
                                let changes_req = items
                                    .iter()
                                    .filter(|a| a.state.contains("CHANGES_REQUESTED"))
                                    .count();
                                let commented =
                                    items.iter().filter(|a| a.state.contains("COMMENT")).count();

                                let day_label =
                                    if *day == chrono::Utc::now().format("%Y-%m-%d").to_string() {
                                        "today".green().bold()
                                    } else if *day
                                        == (chrono::Utc::now() - chrono::Duration::days(1))
                                            .format("%Y-%m-%d")
                                            .to_string()
                                    {
                                        "yesterday".normal().bold()
                                    } else {
                                        day.yellow()
                                    };

                                println!("\n  📅 {}  ({} PRs)", day_label, items.len());
                                if approved > 0 {
                                    print!("    ✅ {} approved", approved);
                                }
                                if changes_req > 0 {
                                    print!("    🔁 {} changes requested", changes_req);
                                }
                                if commented > 0 {
                                    print!("    💬 {} commented", commented);
                                }
                                println!();

                                for activity in items.iter().take(5) {
                                    let state_icon = if activity.state.contains("APPROVED") {
                                        "✅".to_string()
                                    } else if activity.state.contains("CHANGES_REQUESTED") {
                                        "🔁".to_string()
                                    } else {
                                        "💬".to_string()
                                    };
                                    let title = if activity.pr_title.len() > 50 {
                                        format!("{}...", &activity.pr_title[..47])
                                    } else {
                                        activity.pr_title.clone()
                                    };
                                    let priority_display = if priority {
                                        let score = priority_scores
                                            .as_ref()
                                            .and_then(|scores| {
                                                scores
                                                    .get(&(
                                                        activity.repo.clone(),
                                                        activity.pr_number,
                                                    ))
                                                    .copied()
                                            })
                                            .unwrap_or(0);
                                        format!("  {}", logger::priority_stars(score))
                                    } else {
                                        String::new()
                                    };
                                    println!(
                                        "    {}  #{}  {}{}{}",
                                        state_icon,
                                        activity.pr_number,
                                        title.dimmed(),
                                        priority_display,
                                        format_args!(" ({})", activity.repo.dimmed())
                                    );
                                }
                                if items.len() > 5 {
                                    println!("    ... and {} more", items.len() - 5);
                                }
                            }

                            // Summary by repo
                            let mut by_repo: HashMap<String, usize> = HashMap::new();
                            for activity in &activities {
                                *by_repo.entry(activity.repo.clone()).or_insert(0) += 1;
                            }
                            println!("\n  📁 By repository:");
                            let mut sorted: Vec<_> = by_repo.iter().collect();
                            sorted.sort_by(|a, b| b.1.cmp(a.1));
                            for (repo, count) in sorted.iter().take(5) {
                                println!("    {}: {}", repo, count);
                            }
                        }
                        println!("{}", "─".repeat(45));
                        println!("\n  💡 Use `--json` for machine-readable output\n");
                    }
                }
                Err(e) => {
                    println!("{}", "❌ Failed to fetch activity".red());
                    println!("   Error: {}", e);
                }
            }
        }

        Commands::Mentions {
            unread_only,
            limit,
            pr,
            since_days,
            repo,
            author,
            priority,
            json,
        } => {
            let limit = limit.unwrap_or(20);

            println!("\n🔔 Fetching your GitHub notifications...\n");

            match github::fetch_mentions(
                &ctx.cfg.github_token,
                &ctx.cfg.github_username,
                unread_only,
                limit,
            )
            .await
            {
                Ok(mentions) => {
                    // Apply --pr filter if specified
                    let filtered_mentions: Vec<_> = match pr {
                        Some(num) => mentions
                            .into_iter()
                            .filter(|m| m.pr_number == num)
                            .collect(),
                        None => mentions,
                    };

                    // Apply --since-days filter (based on updated_at)
                    let filtered_mentions: Vec<_> = match since_days {
                        Some(days) => {
                            let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                            filtered_mentions
                                .into_iter()
                                .filter(|m| m.updated_at >= cutoff)
                                .collect()
                        }
                        None => filtered_mentions,
                    };

                    // Apply --repo filter (partial match, case-insensitive)
                    let filtered_mentions: Vec<_> = match repo {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            filtered_mentions
                                .into_iter()
                                .filter(|m| m.repo.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => filtered_mentions,
                    };

                    // Apply --author filter (partial match, case-insensitive)
                    let filtered_mentions: Vec<_> = match author {
                        Some(ref pattern) => {
                            let pattern_lower = pattern.to_lowercase();
                            filtered_mentions
                                .into_iter()
                                .filter(|m| m.author.to_lowercase().contains(&pattern_lower))
                                .collect()
                        }
                        None => filtered_mentions,
                    };

                    if filtered_mentions.is_empty() {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&serde_json::json!([]))?);
                        } else {
                            if let Some(pr_num) = pr {
                                println!("  😴 No notifications found for PR #{}.", pr_num);
                            } else {
                                println!("  😴 No notifications found.");
                            }
                            if unread_only {
                                println!("  (showing all, not just unread — use `-u` to filter)");
                            }
                        }
                        return Ok(());
                    }

                    let total = filtered_mentions.len();
                    let unread_count = filtered_mentions.iter().filter(|m| m.unread).count();

                    if json {
                        println!("{}", serde_json::to_string_pretty(&filtered_mentions)?);
                    } else {
                        println!(
                            "🔔 Notifications  ({} total{} | showing top {})\n{}",
                            total,
                            if unread_count > 0 {
                                format!(", {} unread", unread_count)
                            } else {
                                String::new()
                            },
                            limit,
                            "─".repeat(50)
                        );

                        for (i, mention) in filtered_mentions.iter().enumerate() {
                            let reason_label = match mention.reason.as_str() {
                                "mention" => "🏷️ mentioned",
                                "review_requested" => "👀 review requested",
                                "assign" => "📌 assigned",
                                "author" => "✍️ you authored",
                                "team_mention" => "👥 team mentioned",
                                "cm" => "💬 comment",
                                "subscribed" => "📬 subscribed",
                                _ => "📌 unknown",
                            };

                            let unread_marker = if mention.unread { " 🔵" } else { "" };
                            let age_days = (chrono::Utc::now() - mention.updated_at).num_days();
                            let age_label = if age_days == 0 {
                                "today".green()
                            } else if age_days == 1 {
                                "1 day ago".normal()
                            } else if age_days <= 7 {
                                format!("{} days ago", age_days).yellow()
                            } else {
                                format!("{} days ago", age_days).red()
                            };

                            // Calculate priority score for notifications (1-5 stars based on age and unread status)
                            let priority_score = if age_days == 0 {
                                3 // today: neutral
                            } else if age_days <= 1 {
                                2 // 1 day old: slightly stale
                            } else if age_days <= 3 {
                                4 // 2-3 days: concerning
                            } else {
                                5 // over a week: urgent
                            };

                            let priority_display = if priority {
                                format!("  ⭐ {}/5", priority_score)
                            } else {
                                String::new()
                            };

                            let title = if mention.pr_title.len() > 55 {
                                format!("{}...", &mention.pr_title[..52])
                            } else {
                                mention.pr_title.clone()
                            };

                            println!(
                                "{}. {}  #{}  {}{}{}",
                                i + 1,
                                reason_label,
                                mention.pr_number,
                                title.bold(),
                                unread_marker,
                                priority_display
                            );
                            println!(
                                "   👤 {}  •  📁 {}  •  ⏱️ {}",
                                mention.author.cyan(),
                                mention.repo.dimmed(),
                                age_label,
                            );
                            if !mention.last_comment_preview.is_empty() {
                                let preview = if mention.last_comment_preview.len() > 80 {
                                    format!("{}...", &mention.last_comment_preview[..77])
                                } else {
                                    mention.last_comment_preview.clone()
                                };
                                println!("   💬 \"{}\"", preview.dimmed());
                            }
                            if i < total - 1 {
                                println!();
                            }
                        }

                        println!("\n{}", "─".repeat(50));
                        println!(
                            "  💡 Use `--unread-only` or `-u` to show only unread notifications"
                        );
                        println!("  💡 Use `--json` for scripting\n");
                    }
                }
                Err(e) => {
                    println!("{}", "❌ Failed to fetch notifications".red());
                    println!("   Error: {}", e);
                }
            }
        }

        Commands::Trends {
            days,
            limit,
            repo,
            author,
            json,
            priority,
            since_days,
        } => {
            use chrono::{Duration, Utc};
            use std::collections::{BTreeMap, HashMap};

            let report_output_dir = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"));
            let n = limit.unwrap_or(10);

            if !report_output_dir.exists() {
                println!(
                    "❌ No reviews directory found at {}. Run `prctrl list` first to save reviews.",
                    report_output_dir.display()
                );
                return Ok(());
            }

            // Target specific PR: global --pr flag
            let target_pr = ctx.cli.pr;

            let cutoff = Utc::now() - Duration::days(days as i64);

            #[derive(Debug, Clone, serde::Serialize)]
            struct TrendedReview {
                pr_title: String,
                pr_number: u64,
                repo: String,
                author: String,
                reviewed_at: String,
                created_at: String,
                additions: u64,
                deletions: u64,
                priority_score: u8,
            }

            // ── Collect processed ctx.reviews from review files ──
            let mut reviews_data: Vec<TrendedReview> = vec![];
            let mut total_additions: u64 = 0;
            let mut total_deletions: u64 = 0;
            let mut by_author: HashMap<String, u32> = HashMap::new();
            let mut by_repo: HashMap<String, u32> = HashMap::new();
            let mut by_day: BTreeMap<String, u32> = BTreeMap::new(); // BTree for sorted days

            if let Ok(entries) = std::fs::read_dir(&report_output_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            let lines: Vec<&str> = content.lines().collect();
                            if lines.len() >= 4 {
                                let pr_title = lines
                                    .first()
                                    .unwrap_or(&"")
                                    .trim()
                                    .trim_start_matches("# ")
                                    .to_string();
                                let date_line = lines.iter().find(|l| l.starts_with("Reviewed on"));
                                let additions_line = lines
                                    .iter()
                                    .find(|l| l.contains("+") && l.contains("additions"));
                                let deletions_line = lines
                                    .iter()
                                    .find(|l| l.contains("-") && l.contains("deletions"));
                                let author_line =
                                    lines.iter().find(|l| l.starts_with("- **Author**:"));
                                let repo_line =
                                    lines.iter().find(|l| l.starts_with("- **Repository**:"));

                                if let Some(date_str) = date_line {
                                    if let Some(date_part) = date_str.strip_prefix("Reviewed on ") {
                                        if let Ok(reviewed_at) =
                                            chrono::DateTime::parse_from_rfc3339(date_part)
                                        {
                                            let reviewed_at_tz = reviewed_at.with_timezone(&Utc);
                                            if reviewed_at_tz >= cutoff {
                                                let pr_number = path
                                                    .file_stem()
                                                    .and_then(|s| s.to_str())
                                                    .and_then(|s| s.split('_').next_back())
                                                    .and_then(|s| s.parse().ok())
                                                    .unwrap_or(0);

                                                let additions: u64 = additions_line
                                                    .and_then(|l| l.split('`').nth(1))
                                                    .and_then(|s| {
                                                        s.replace(['+', ','], "")
                                                            .trim()
                                                            .parse()
                                                            .ok()
                                                    })
                                                    .unwrap_or(0);
                                                let deletions: u64 = deletions_line
                                                    .and_then(|l| l.split('`').nth(1))
                                                    .and_then(|s| {
                                                        s.replace(['-', ','], "")
                                                            .trim()
                                                            .parse()
                                                            .ok()
                                                    })
                                                    .unwrap_or(0);
                                                let pr_author = author_line
                                                    .and_then(|l| l.strip_prefix("- **Author**:"))
                                                    .map(|s| s.trim().to_string())
                                                    .unwrap_or_default();
                                                let pr_repo = repo_line
                                                    .and_then(|l| {
                                                        l.strip_prefix("- **Repository**:")
                                                    })
                                                    .map(|s| s.trim().to_string())
                                                    .unwrap_or_default();

                                                // Apply --pr filter (target specific PR via global flag)
                                                if let Some(target) = target_pr {
                                                    if pr_number != target {
                                                        continue;
                                                    }
                                                }

                                                // Apply --repo filter (partial match, case-insensitive)
                                                if let Some(ref repo_filter) = repo {
                                                    let pattern = repo_filter.to_lowercase();
                                                    if !pr_repo.to_lowercase().contains(&pattern) {
                                                        continue;
                                                    }
                                                }

                                                // Apply --author filter (partial match, case-insensitive)
                                                if let Some(ref author_filter) = author {
                                                    let pattern = author_filter.to_lowercase();
                                                    if !pr_author.to_lowercase().contains(&pattern)
                                                    {
                                                        continue;
                                                    }
                                                }

                                                total_additions += additions;
                                                total_deletions += deletions;
                                                *by_author.entry(pr_author.clone()).or_insert(0) +=
                                                    1;
                                                *by_repo.entry(pr_repo.clone()).or_insert(0) += 1;

                                                let day_key =
                                                    reviewed_at_tz.format("%Y-%m-%d").to_string();
                                                *by_day.entry(day_key).or_insert(0) += 1;

                                                // Parse created_at from review file
                                                let created_line = lines
                                                    .iter()
                                                    .find(|l| l.starts_with("- **Created**:"));
                                                let created_at_str = created_line
                                                    .and_then(|l| l.strip_prefix("- **Created**: "))
                                                    .unwrap_or("")
                                                    .trim();
                                                let created_at_tz = if let Ok(dt) =
                                                    chrono::DateTime::parse_from_rfc3339(
                                                        created_at_str,
                                                    ) {
                                                    dt.with_timezone(&Utc)
                                                } else {
                                                    reviewed_at_tz // fallback to reviewed_at if not found
                                                };

                                                // Calculate priority score based on age and size
                                                let size = additions.saturating_add(deletions);
                                                let age_days = (reviewed_at_tz - created_at_tz)
                                                    .num_days()
                                                    .max(0)
                                                    as u32;
                                                let priority_score =
                                                    logger::calculate_priority_score_for_stats(
                                                        size, age_days,
                                                    );

                                                // Apply --since-days filter (PRs created within N days)
                                                if let Some(since) = since_days {
                                                    let since_cutoff =
                                                        Utc::now() - Duration::days(since as i64);
                                                    if created_at_tz < since_cutoff {
                                                        continue;
                                                    }
                                                }

                                                reviews_data.push(TrendedReview {
                                                    pr_title,
                                                    pr_number,
                                                    repo: pr_repo,
                                                    author: pr_author,
                                                    reviewed_at: reviewed_at_tz.to_rfc3339(),
                                                    created_at: created_at_tz.to_rfc3339(),
                                                    additions,
                                                    deletions,
                                                    priority_score,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let review_count = reviews_data.len();

            // ── Compute daily averages ──
            let active_days = by_day.len().max(1) as u64;
            let avg_per_day = review_count as f64 / active_days as f64;
            let avg_additions = if review_count > 0 {
                total_additions as f64 / review_count as f64
            } else {
                0.0
            };
            let avg_deletions = if review_count > 0 {
                total_deletions as f64 / review_count as f64
            } else {
                0.0
            };

            // ── Week-over-week comparison ──
            let this_week_start = Utc::now() - Duration::days(7);
            let prev_week_start = Utc::now() - Duration::days(14);

            let this_week_count: usize = reviews_data
                .iter()
                .filter(|r| {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&r.reviewed_at) {
                        dt.with_timezone(&Utc) >= this_week_start
                    } else {
                        false
                    }
                })
                .count();

            let prev_week_count: usize = reviews_data
                .iter()
                .filter(|r| {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&r.reviewed_at) {
                        let d = dt.with_timezone(&Utc);
                        d >= prev_week_start && d < this_week_start
                    } else {
                        false
                    }
                })
                .count();

            let wow_change = if prev_week_count > 0 {
                ((this_week_count as f64 - prev_week_count as f64) / prev_week_count as f64) * 100.0
            } else {
                0.0
            };

            // ── Top authors ──
            let mut top_authors: Vec<(String, u32)> = by_author.into_iter().collect();
            top_authors.sort_by(|a, b| b.1.cmp(&a.1));

            // ── Top repos ──
            let mut top_repos: Vec<(String, u32)> = by_repo.into_iter().collect();
            top_repos.sort_by(|a, b| b.1.cmp(&a.1));

            // ── Daily chart (last 14 days) ──
            let mut chart_days: Vec<(String, u32)> = vec![];
            for i in (0..14).rev() {
                let day = (Utc::now() - Duration::days(i))
                    .format("%Y-%m-%d")
                    .to_string();
                let count = *by_day.get(&day).unwrap_or(&0);
                chart_days.push((day, count));
            }

            if json {
                #[derive(serde::Serialize)]
                struct TrendsOutput {
                    period_days: u32,
                    total_reviews: usize,
                    reviews_by_day: BTreeMap<String, u32>,
                    avg_per_day: f64,
                    avg_additions: f64,
                    avg_deletions: f64,
                    total_additions: u64,
                    total_deletions: u64,
                    this_week_count: usize,
                    prev_week_count: usize,
                    wow_change_pct: f64,
                    top_authors: Vec<(String, u32)>,
                    top_repos: Vec<(String, u32)>,
                }
                let output = TrendsOutput {
                    period_days: days,
                    total_reviews: review_count,
                    reviews_by_day: by_day,
                    avg_per_day,
                    avg_additions,
                    avg_deletions,
                    total_additions,
                    total_deletions,
                    this_week_count,
                    prev_week_count,
                    wow_change_pct: wow_change,
                    top_authors,
                    top_repos,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "\n📈 Review Trends — last {} days\n{}",
                    days,
                    "─".repeat(45)
                );

                if review_count == 0 {
                    println!("  😴 No review data found in the last {} days.", days);
                    println!("  Process some reviews first with `prctrl delegate`.\n");
                    return Ok(());
                }

                // ── Summary stats ──
                println!("  📊 Summary");
                println!("     Total reviews:       {}", review_count);
                println!("     Daily average:       {:.1} PRs/day", avg_per_day);
                println!(
                    "     Lines reviewed:      +{} / -{}",
                    total_additions.to_string().green(),
                    total_deletions.to_string().red()
                );
                println!(
                    "     Avg PR size:         +{:.0} / -{:.0}",
                    avg_additions, avg_deletions
                );
                println!();

                // ── Week over week ──
                println!("  📅 Week-over-Week");
                let wow_icon = if wow_change > 0.0 {
                    "📈"
                } else if wow_change < 0.0 {
                    "📉"
                } else {
                    "➖"
                };
                let wow_color: colored::ColoredString = if wow_change > 0.0 {
                    wow_change.to_string().green()
                } else if wow_change < 0.0 {
                    wow_change.to_string().red()
                } else {
                    "0%".normal()
                };
                println!(
                    "     {} This week: {}   Previous: {}   Change: {}",
                    wow_icon,
                    this_week_count.to_string().cyan().bold(),
                    prev_week_count.to_string().dimmed(),
                    wow_color
                );
                println!();

                // ── Sparkline chart (last 14 days) ──
                println!("  📈 Daily Activity (last 14 days)");
                let max_count = chart_days.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1) as f64;
                for (day, count) in &chart_days {
                    let bar_len = ((*count as f64 / max_count) * 20.0).round() as usize;
                    let bar: String = "█".repeat(bar_len);
                    let empty: String = "░".repeat(20 - bar_len);
                    let is_today = *day == Utc::now().format("%Y-%m-%d").to_string();
                    let day_label = if is_today {
                        format!("{} (today)", &day[5..])
                    } else {
                        day[5..].to_string()
                    };
                    let count_label = if *count == 0 {
                        "   ".to_string()
                    } else {
                        count.to_string()
                    };
                    println!(
                        "     {}  {}{}  {}",
                        day_label.dimmed(),
                        bar.green(),
                        empty.truecolor(40, 40, 40),
                        count_label.dimmed()
                    );
                }
                println!();

                // ── Top authors ──
                if !top_authors.is_empty() {
                    println!("  👥 Top Authors (by PR count)");
                    for (author, count) in top_authors.iter().take(n) {
                        println!("     {}  {}", author.cyan(), count.to_string().dimmed());
                    }
                    println!();
                }

                // ── Top repos ──
                if !top_repos.is_empty() {
                    println!("  📁 Top Repositories");
                    for (repo, count) in top_repos.iter().take(n) {
                        let short_name = repo.split('/').next_back().unwrap_or(repo);
                        println!("     {}  {}", short_name, count.to_string().dimmed());
                    }
                    println!();
                }

                // ── Top PRs by priority (if --priority flag) ──
                if priority {
                    use crate::logger::priority_stars;
                    let mut sorted_by_priority = reviews_data.clone();
                    sorted_by_priority.sort_by(|a, b| b.priority_score.cmp(&a.priority_score));

                    println!("  ⭐ Top PRs by Priority");
                    for review in sorted_by_priority.iter().take(10) {
                        let stars = priority_stars(review.priority_score);
                        let title = if review.pr_title.len() > 45 {
                            format!("{}...", &review.pr_title[..42])
                        } else {
                            review.pr_title.clone()
                        };
                        println!(
                            "     {}  #{}  {} ({})",
                            stars,
                            review.pr_number,
                            title.dimmed(),
                            review
                                .repo
                                .split('/')
                                .next_back()
                                .unwrap_or(&review.repo)
                                .dimmed()
                        );
                    }
                    println!();
                }

                println!("  💡 Use `--days <N>` to adjust the lookback period");
                println!("  💡 Use `--json` for machine-readable output");
                println!("{}", "─".repeat(45));
                println!();
            }
        }

        Commands::ReviewVelocity {
            days,
            bottlenecks,
            priority,
            repo,
            author,
            since_days,
            json,
            pr_number,
            pr,
        } => {
            use crate::logger;
            use chrono::{Duration, Utc};
            use std::collections::{BTreeMap, HashMap};

            let report_output_dir = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"));

            if !report_output_dir.exists() {
                println!(
                    "❌ No reviews directory found at {}. Run `prctrl delegate` first.",
                    report_output_dir.display()
                );
                return Ok(());
            }

            // Target specific PR: global --pr flag > local --pr > pr_number
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            let cutoff = Utc::now() - Duration::days(days as i64);

            #[derive(Debug, Clone, serde::Serialize)]
            struct VelocityData {
                pr_title: String,
                pr_number: u64,
                repo: String,
                author: String,
                reviewed_at: String,
                created_at: String,
                hours_to_review: f64,
                additions: u64,
                deletions: u64,
                priority_score: u8,
            }

            let mut velocity_data: Vec<VelocityData> = vec![];
            let mut by_author: HashMap<String, Vec<f64>> = HashMap::new();
            let mut by_repo: HashMap<String, Vec<f64>> = HashMap::new();

            if let Ok(entries) = std::fs::read_dir(&report_output_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            let lines: Vec<&str> = content.lines().collect();
                            if lines.len() >= 4 {
                                let pr_title = lines
                                    .first()
                                    .unwrap_or(&"")
                                    .trim()
                                    .trim_start_matches("# ")
                                    .to_string();
                                let date_line = lines.iter().find(|l| l.starts_with("Reviewed on"));
                                let created_line =
                                    lines.iter().find(|l| l.starts_with("- **Created**:"));
                                let additions_line = lines
                                    .iter()
                                    .find(|l| l.contains("+") && l.contains("additions"));
                                let deletions_line = lines
                                    .iter()
                                    .find(|l| l.contains("-") && l.contains("deletions"));
                                let author_line =
                                    lines.iter().find(|l| l.starts_with("- **Author**:"));
                                let repo_line =
                                    lines.iter().find(|l| l.starts_with("- **Repository**:"));

                                if let (Some(date_str), Some(created_str)) =
                                    (date_line, created_line)
                                {
                                    if let (Some(date_part), Some(created_part)) = (
                                        date_str.strip_prefix("Reviewed on "),
                                        created_str.strip_prefix("- **Created**: "),
                                    ) {
                                        if let (Ok(reviewed_at), Ok(created_at)) = (
                                            chrono::DateTime::parse_from_rfc3339(date_part),
                                            chrono::DateTime::parse_from_rfc3339(created_part),
                                        ) {
                                            let reviewed_at_tz = reviewed_at.with_timezone(&Utc);
                                            let created_at_tz = created_at.with_timezone(&Utc);

                                            // Apply --since-days filter early (before expensive parsing)
                                            if let Some(since) = since_days {
                                                let since_cutoff =
                                                    Utc::now() - Duration::days(since as i64);
                                                if created_at_tz < since_cutoff {
                                                    continue;
                                                }
                                            }

                                            if reviewed_at_tz >= cutoff {
                                                let hours = (reviewed_at_tz - created_at_tz)
                                                    .num_hours()
                                                    as f64;

                                                let pr_number = path
                                                    .file_stem()
                                                    .and_then(|s| s.to_str())
                                                    .and_then(|s| s.split('_').next_back())
                                                    .and_then(|s| s.parse().ok())
                                                    .unwrap_or(0);

                                                // Filter by specific PR if --pr flag is set
                                                if let Some(target) = target_pr {
                                                    if pr_number != target {
                                                        continue;
                                                    }
                                                }

                                                let additions: u64 = additions_line
                                                    .and_then(|l| l.split('`').nth(1))
                                                    .and_then(|s| {
                                                        s.replace(['+', ','], "")
                                                            .trim()
                                                            .parse()
                                                            .ok()
                                                    })
                                                    .unwrap_or(0);
                                                let deletions: u64 = deletions_line
                                                    .and_then(|l| l.split('`').nth(1))
                                                    .and_then(|s| {
                                                        s.replace(['-', ','], "")
                                                            .trim()
                                                            .parse()
                                                            .ok()
                                                    })
                                                    .unwrap_or(0);
                                                let pr_author = author_line
                                                    .and_then(|l| l.strip_prefix("- **Author**:"))
                                                    .map(|s| s.trim().to_string())
                                                    .unwrap_or_default();
                                                let pr_repo = repo_line
                                                    .and_then(|l| {
                                                        l.strip_prefix("- **Repository**:")
                                                    })
                                                    .map(|s| s.trim().to_string())
                                                    .unwrap_or_default();

                                                // Apply --repo filter (partial match, case-insensitive)
                                                if let Some(ref repo_filter) = repo {
                                                    let pattern = repo_filter.to_lowercase();
                                                    if !pr_repo.to_lowercase().contains(&pattern) {
                                                        continue;
                                                    }
                                                }

                                                // Apply --author filter (partial match, case-insensitive)
                                                if let Some(ref author_filter) = author {
                                                    let pattern = author_filter.to_lowercase();
                                                    if !pr_author.to_lowercase().contains(&pattern)
                                                    {
                                                        continue;
                                                    }
                                                }

                                                by_author
                                                    .entry(pr_author.clone())
                                                    .or_default()
                                                    .push(hours);
                                                by_repo
                                                    .entry(pr_repo.clone())
                                                    .or_default()
                                                    .push(hours);

                                                // Calculate priority score for this PR
                                                let mock_review = github::PendingReview {
                                                    repo: pr_repo.clone(),
                                                    pr_number,
                                                    pr_title: pr_title.clone(),
                                                    pr_author: pr_author.clone(),
                                                    pr_url: String::new(),
                                                    created_at: created_at_tz,
                                                    additions,
                                                    deletions,
                                                    draft: false,
                                                    branch: String::new(),
                                                };
                                                let priority_score =
                                                    logger::calculate_priority_score(&mock_review);

                                                velocity_data.push(VelocityData {
                                                    pr_title,
                                                    pr_number,
                                                    repo: pr_repo,
                                                    author: pr_author,
                                                    reviewed_at: reviewed_at_tz.to_rfc3339(),
                                                    created_at: created_at_tz.to_rfc3339(),
                                                    hours_to_review: hours,
                                                    additions,
                                                    deletions,
                                                    priority_score,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if velocity_data.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "period_days": days,
                            "total_prs": 0,
                            "avg_hours_to_review": 0.0,
                            "median_hours": 0.0,
                            "fastest_review_hours": 0.0,
                            "slowest_review_hours": 0.0,
                            "by_author": {},
                            "by_repo": {},
                        }))?
                    );
                } else {
                    println!(
                        "\n⚡ Review Velocity — last {} days\n{}",
                        days,
                        "─".repeat(45)
                    );
                    println!("  😴 No review data found in the last {} days.", days);
                    println!("  Process some reviews first with `prctrl delegate`.\n");
                }
                return Ok(());
            }

            // Calculate statistics
            let mut all_hours: Vec<f64> = velocity_data.iter().map(|v| v.hours_to_review).collect();
            all_hours.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            let total_prs = velocity_data.len();
            let avg_hours = all_hours.iter().sum::<f64>() / all_hours.len() as f64;
            let median_hours = if all_hours.len().is_multiple_of(2) {
                (all_hours[all_hours.len() / 2 - 1] + all_hours[all_hours.len() / 2]) / 2.0
            } else {
                all_hours[all_hours.len() / 2]
            };
            let fastest = all_hours.first().copied().unwrap_or(0.0);
            let slowest = all_hours.last().copied().unwrap_or(0.0);

            // Author stats
            let mut author_stats: Vec<(String, f64, f64, usize)> = by_author
                .iter()
                .map(|(author, hours)| {
                    let avg = hours.iter().sum::<f64>() / hours.len() as f64;
                    let sorted = hours.clone();
                    let median = if sorted.len() % 2 == 0 {
                        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
                    } else {
                        sorted[sorted.len() / 2]
                    };
                    (author.clone(), avg, median, hours.len())
                })
                .collect();
            author_stats.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            // Repo stats
            let mut repo_stats: Vec<(String, f64, f64, usize)> = by_repo
                .iter()
                .map(|(repo, hours)| {
                    let avg = hours.iter().sum::<f64>() / hours.len() as f64;
                    let sorted = hours.clone();
                    let median = if sorted.len() % 2 == 0 {
                        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
                    } else {
                        sorted[sorted.len() / 2]
                    };
                    (repo.clone(), avg, median, hours.len())
                })
                .collect();
            repo_stats.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            if json {
                #[derive(serde::Serialize)]
                struct VelocityOutput {
                    period_days: u32,
                    total_prs: usize,
                    avg_hours_to_review: f64,
                    median_hours: f64,
                    fastest_review_hours: f64,
                    slowest_review_hours: f64,
                    by_priority: std::collections::BTreeMap<u8, (f64, usize)>,
                    by_author: BTreeMap<String, (f64, f64, usize)>,
                    by_repo: BTreeMap<String, (f64, f64, usize)>,
                }
                // Priority breakdown
                let mut by_priority_map: std::collections::BTreeMap<u8, (f64, usize)> =
                    std::collections::BTreeMap::new();
                let mut by_priority_hours: std::collections::HashMap<u8, Vec<f64>> =
                    std::collections::HashMap::new();
                for v in &velocity_data {
                    by_priority_hours
                        .entry(v.priority_score)
                        .or_default()
                        .push(v.hours_to_review);
                }
                for (score, hours_list) in &by_priority_hours {
                    let avg = hours_list.iter().sum::<f64>() / hours_list.len() as f64;
                    by_priority_map.insert(*score, (avg, hours_list.len()));
                }
                let mut by_author_map: BTreeMap<String, (f64, f64, usize)> = BTreeMap::new();
                for (author, avg, median, count) in &author_stats {
                    by_author_map.insert(author.clone(), (*avg, *median, *count));
                }
                let mut by_repo_map: BTreeMap<String, (f64, f64, usize)> = BTreeMap::new();
                for (repo, avg, median, count) in &repo_stats {
                    by_repo_map.insert(repo.clone(), (*avg, *median, *count));
                }
                let output = VelocityOutput {
                    period_days: days,
                    total_prs,
                    avg_hours_to_review: avg_hours,
                    median_hours,
                    fastest_review_hours: fastest,
                    slowest_review_hours: slowest,
                    by_priority: by_priority_map,
                    by_author: by_author_map,
                    by_repo: by_repo_map,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "\n⚡ Review Velocity — last {} days\n{}",
                    days,
                    "─".repeat(45)
                );

                // Summary stats
                println!("  📊 Summary ({} PRs reviewed)", total_prs);
                println!("     Average time to review:  {:.1} hours", avg_hours);
                println!("     Median time to review:    {:.1} hours", median_hours);
                println!("     Fastest review:           {:.1} hours", fastest);
                println!("     Slowest review:           {:.1} hours", slowest);

                // Time buckets
                let mut under_4h = 0usize;
                let mut under_24h = 0usize;
                let mut under_72h = 0usize;
                let mut over_72h = 0usize;

                for h in &all_hours {
                    if *h <= 4.0 {
                        under_4h += 1;
                    } else if *h <= 24.0 {
                        under_24h += 1;
                    } else if *h <= 72.0 {
                        under_72h += 1;
                    } else {
                        over_72h += 1;
                    }
                }

                println!("\n  ⏱️  Time Distribution");
                let total_f = total_prs as f64;
                println!(
                    "     < 4h:   {:>4} ({:>5.1}%)  {}",
                    under_4h,
                    (under_4h as f64 / total_f) * 100.0,
                    "▓"
                        .repeat((under_4h as f64 / total_f * 20.0) as usize)
                        .green()
                );
                println!(
                    "     4-24h:  {:>4} ({:>5.1}%)  {}",
                    under_24h - under_4h,
                    ((under_24h - under_4h) as f64 / total_f) * 100.0,
                    "▓"
                        .repeat(((under_24h - under_4h) as f64 / total_f * 20.0) as usize)
                        .cyan()
                );
                println!(
                    "     1-3d:   {:>4} ({:>5.1}%)  {}",
                    under_72h - under_24h,
                    ((under_72h - under_24h) as f64 / total_f) * 100.0,
                    "▓"
                        .repeat(((under_72h - under_24h) as f64 / total_f * 20.0) as usize)
                        .yellow()
                );
                println!(
                    "     > 3d:   {:>4} ({:>5.1}%)  {}",
                    over_72h,
                    (over_72h as f64 / total_f) * 100.0,
                    "▓"
                        .repeat((over_72h as f64 / total_f * 20.0) as usize)
                        .red()
                );

                if priority {
                    // Show priority breakdown for reviewed PRs
                    let mut by_priority: std::collections::HashMap<u8, Vec<f64>> =
                        std::collections::HashMap::new();
                    for v in &velocity_data {
                        by_priority
                            .entry(v.priority_score)
                            .or_default()
                            .push(v.hours_to_review);
                    }

                    println!("\n  ⭐ Priority vs Review Time");
                    for score in (1..=5).rev() {
                        if let Some(hours_list) = by_priority.get(&score) {
                            let avg = hours_list.iter().sum::<f64>() / hours_list.len() as f64;
                            let stars = logger::priority_stars(score);
                            let count = hours_list.len();
                            let bar_len = ((avg / avg_hours) * 10.0).round() as usize;
                            let bar: String = "█".repeat(bar_len.max(1));
                            let bar_colored = if score >= 4 {
                                bar.red().to_string()
                            } else if score >= 3 {
                                bar.yellow().to_string()
                            } else {
                                bar.green().to_string()
                            };
                            println!(
                                "     ⭐{}  {} PRs  {:.1}h avg  {}",
                                stars, count, avg, bar_colored
                            );
                        }
                    }
                }

                if bottlenecks {
                    println!("\n  🐢 Bottleneck Analysis — by Author");
                    println!("     (slowest average review time)");
                    for (author, avg, _median, count) in author_stats.iter().rev().take(5) {
                        let bar_len = ((avg / avg_hours) * 10.0).round() as usize;
                        let bar: String = "█".repeat(bar_len.max(1));
                        println!(
                            "     {} {}  {:.1}h avg  ({} PRs)",
                            author.cyan(),
                            bar.red(),
                            avg,
                            count
                        );
                    }

                    println!("\n  🐢 Bottleneck Analysis — by Repository");
                    println!("     (slowest average review time)");
                    for (repo, avg, _median, count) in repo_stats.iter().rev().take(5) {
                        let short_name = repo.split('/').next_back().unwrap_or(repo);
                        let bar_len = ((avg / avg_hours) * 10.0).round() as usize;
                        let bar: String = "█".repeat(bar_len.max(1));
                        println!(
                            "     {} {}  {:.1}h avg  ({} PRs)",
                            short_name.yellow(),
                            bar.red(),
                            avg,
                            count
                        );
                    }
                }

                println!("\n  💡 Use `--bottlenecks` to see which repos/authors take longest");
                println!("  💡 Use `--priority` to correlate priority with review speed");
                println!("  💡 Use `--json` for machine-readable output");
                println!("{}", "─".repeat(45));
                println!();
            }
        }

        Commands::Summary {
            json,
            repo,
            author,
            since_days,
            priority,
        } => {
            use chrono::Utc;

            // Apply --repo filter (partial match, case-insensitive)
            let filtered: Vec<_> = match repo {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    ctx.reviews
                        .iter()
                        .filter(|r| r.repo.to_lowercase().contains(&pattern_lower))
                        .cloned()
                        .collect()
                }
                None => ctx.reviews.clone(),
            };

            // Apply --author filter (partial match, case-insensitive)
            let filtered: Vec<_> = match author {
                Some(ref pattern) => {
                    let pattern_lower = pattern.to_lowercase();
                    filtered
                        .into_iter()
                        .filter(|r| r.pr_author.to_lowercase().contains(&pattern_lower))
                        .collect()
                }
                None => filtered,
            };

            // Apply --since-days filter
            let filtered: Vec<_> = match since_days {
                Some(days) => {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    filtered
                        .into_iter()
                        .filter(|r| r.created_at >= cutoff)
                        .collect()
                }
                None => filtered,
            };

            // Filter out snoozed PRs
            let snooze_file = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"))
                .join(".snoozed.json");

            let now = chrono::Utc::now();
            let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                    if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                        entries
                            .into_iter()
                            .filter_map(|e| {
                                let repo = e.get("repo")?.as_str()?.to_string();
                                let pr_number = e.get("pr_number")?.as_u64()?;
                                let until_str = e.get("snoozed_until")?.as_str()?;
                                if let Ok(until) = chrono::DateTime::parse_from_rfc3339(until_str) {
                                    if until.with_timezone(&chrono::Utc) > now {
                                        return Some((repo, pr_number));
                                    }
                                }
                                None
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            let filtered: Vec<_> = filtered
                .into_iter()
                .filter(|r| {
                    !snoozed_prs
                        .iter()
                        .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                })
                .collect();

            if filtered.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "total": 0,
                            "total_additions": 0,
                            "total_deletions": 0,
                            "oldest_age_days": 0,
                            "draft_count": 0,
                            "by_urgency": { "critical": 0, "high": 0, "medium": 0, "low": 0 },
                            "by_repo": {},
                        }))?
                    );
                } else {
                    println!("✅ No pending reviews. You're all clear!");
                }
                return Ok(());
            }

            let now = Utc::now();
            let total = filtered.len();
            let total_additions: u64 = filtered.iter().map(|r| r.additions).sum();
            let total_deletions: u64 = filtered.iter().map(|r| r.deletions).sum();
            let draft_count = filtered.iter().filter(|r| r.draft).count();

            // Find oldest PR age
            let oldest_age_days = filtered
                .iter()
                .map(|r| (now - r.created_at).num_days())
                .max()
                .unwrap_or(0);

            // Categorize by urgency (based on priority score)
            let mut critical = 0usize; // score 5
            let mut high = 0usize; // score 4
            let mut medium = 0usize; // score 3
            let mut low = 0usize; // score 1-2

            for r in &filtered {
                let score = logger::calculate_priority_score(r);
                match score {
                    5 => critical += 1,
                    4 => high += 1,
                    3 => medium += 1,
                    _ => low += 1,
                }
            }

            // By repo breakdown
            use std::collections::HashMap;
            let mut by_repo: HashMap<String, usize> = HashMap::new();
            for r in &filtered {
                *by_repo.entry(r.repo.clone()).or_insert(0) += 1;
            }

            if json {
                #[derive(serde::Serialize)]
                struct SummaryOutput {
                    total: usize,
                    total_additions: u64,
                    total_deletions: u64,
                    oldest_age_days: i64,
                    draft_count: usize,
                    by_urgency: UrgencyBreakdown,
                    by_repo: HashMap<String, usize>,
                }
                #[derive(serde::Serialize)]
                struct UrgencyBreakdown {
                    critical: usize,
                    high: usize,
                    medium: usize,
                    low: usize,
                }
                let output = SummaryOutput {
                    total,
                    total_additions,
                    total_deletions,
                    oldest_age_days,
                    draft_count,
                    by_urgency: UrgencyBreakdown {
                        critical,
                        high,
                        medium,
                        low,
                    },
                    by_repo,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                // Single-line summary for quick status
                let oldest_label = if oldest_age_days == 0 {
                    "today".green()
                } else if oldest_age_days == 1 {
                    "1d".yellow()
                } else if oldest_age_days <= 3 {
                    format!("{}d", oldest_age_days).yellow()
                } else if oldest_age_days <= 7 {
                    format!("{}d", oldest_age_days).red()
                } else {
                    format!("{}d!!", oldest_age_days).red().bold()
                };

                let urgency_parts: Vec<String> = {
                    let mut parts = Vec::new();
                    if critical > 0 {
                        parts.push(format!("🔥{}/", critical));
                    }
                    if high > 0 {
                        parts.push(format!("⚡{}/", high));
                    }
                    if medium > 0 {
                        parts.push(format!("📅{}/", medium));
                    }
                    if low > 0 {
                        parts.push(format!("💤{}/", low));
                    }
                    parts
                };
                let urgency_str = if urgency_parts.is_empty() {
                    "".to_string()
                } else {
                    let mut s = urgency_parts.join("");
                    s.pop(); // remove trailing slash
                    format!(" [{}]", s)
                };

                let draft_str = if draft_count > 0 {
                    format!(" ({} draft)", draft_count).yellow().to_string()
                } else {
                    String::new()
                };

                println!(
                    "📋 {} PRs  ⏱️ oldest:{}  +{}/-{} lines{}{}",
                    total.to_string().cyan().bold(),
                    oldest_label,
                    total_additions.to_string().green(),
                    total_deletions.to_string().red(),
                    urgency_str,
                    draft_str
                );

                // Compact repo breakdown
                if !by_repo.is_empty() {
                    let mut repo_parts: Vec<String> = by_repo
                        .iter()
                        .map(|(repo, count)| {
                            format!("{}:{}", repo.split('/').next_back().unwrap_or(repo), count)
                        })
                        .collect();
                    repo_parts.sort();
                    println!("   📁 {}", repo_parts.join(" • "));
                }

                // Detailed priority breakdown when --priority flag is set
                if priority && !filtered.is_empty() {
                    println!();
                    println!("  ⭐ Priority breakdown:");
                    let mut scored: Vec<_> = filtered
                        .iter()
                        .map(|r| {
                            let score = logger::calculate_priority_score(r);
                            (r, score)
                        })
                        .collect();
                    scored.sort_by_key(|b| std::cmp::Reverse(b.1)); // highest priority first

                    // Group by score
                    let mut score_groups: HashMap<u8, Vec<&github::PendingReview>> = HashMap::new();
                    for (review, score) in &scored {
                        score_groups.entry(*score).or_default().push(review);
                    }

                    for score in (1..=5).rev() {
                        if let Some(prs) = score_groups.get(&score) {
                            let stars = logger::priority_stars(score);
                            let age_days = (chrono::Utc::now() - prs[0].created_at).num_days();
                            let age_str = if age_days == 0 {
                                "today".to_string()
                            } else if age_days == 1 {
                                "1 day".to_string()
                            } else {
                                format!("{} days", age_days)
                            };
                            println!(
                                "    ⭐{}  {} PR(s)  •  oldest: {}  •  +{}/-{} lines",
                                stars,
                                prs.len(),
                                age_str,
                                prs.iter().map(|r| r.additions).sum::<u64>(),
                                prs.iter().map(|r| r.deletions).sum::<u64>()
                            );
                        }
                    }

                    // Highlight the single most urgent PR
                    if let Some((most_urgent, top_score)) = scored.first() {
                        let total = most_urgent.additions + most_urgent.deletions;
                        println!("\n  🚨 Most Urgent:");
                        println!(
                            "    {}  #{}  {}",
                            most_urgent.pr_title.bold(),
                            most_urgent.pr_number,
                            logger::priority_stars(*top_score).red()
                        );
                        println!(
                            "    👤 {}  •  📦 {} lines  •  {}  •  {}",
                            most_urgent.pr_author.cyan(),
                            total,
                            format_args!(
                                "{} old",
                                (chrono::Utc::now() - most_urgent.created_at).num_days()
                            ),
                            most_urgent.repo.dimmed()
                        );
                        println!("    🔗 {}", most_urgent.pr_url.blue().underline());
                    }
                }
            }
        }

        Commands::Attention {
            threshold,
            detailed,
            limit,
            priority,
            repo,
            author,
            since_days,
            json,
        } => {
            use crate::github::PendingReview;
            use chrono::Utc;

            // Apply --since-days filter first (consistent with other commands)
            let mut filtered_reviews: Vec<_> = match since_days {
                Some(days) => {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    ctx.reviews
                        .iter()
                        .filter(|r| r.created_at >= cutoff)
                        .cloned()
                        .collect()
                }
                None => ctx.reviews.clone(),
            };

            // Apply --repo and --author filters
            {
                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    filtered_reviews.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    filtered_reviews.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }
            }

            // Filter out snoozed PRs (consistent with list/delegate/search/summary commands)
            let snooze_file = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"))
                .join(".snoozed.json");

            let now = chrono::Utc::now();
            let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                    if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                        entries
                            .into_iter()
                            .filter_map(|e| {
                                let repo = e.get("repo")?.as_str()?.to_string();
                                let pr_number = e.get("pr_number")?.as_u64()?;
                                let until_str = e.get("snoozed_until")?.as_str()?;
                                if let Ok(until) = chrono::DateTime::parse_from_rfc3339(until_str) {
                                    if until.with_timezone(&chrono::Utc) > now {
                                        return Some((repo, pr_number));
                                    }
                                }
                                None
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            filtered_reviews.retain(|r| {
                !snoozed_prs
                    .iter()
                    .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
            });

            if filtered_reviews.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "total": 0,
                            "high_attention": [],
                            "message": "No pending reviews — nothing demands attention!"
                        }))?
                    );
                } else {
                    println!("✅ No pending reviews. Nothing demands your attention!");
                }
                return Ok(());
            }

            let threshold = threshold.unwrap_or(5).min(10);
            let limit = limit.unwrap_or(10);

            #[derive(Debug, Clone, serde::Serialize)]
            struct AttentionPR {
                repo: String,
                pr_number: u64,
                pr_title: String,
                pr_author: String,
                pr_url: String,
                age_days: i64,
                size: u64,
                draft: bool,
                attention_score: u8,
                factors: AttentionFactors,
            }

            #[derive(Debug, Clone, serde::Serialize)]
            struct AttentionFactors {
                age_score: u8,
                size_score: u8,
                draft_score: u8,
                staleness_bonus: u8,
            }

            fn calc_attention_score(review: &PendingReview) -> (u8, AttentionFactors) {
                let now = Utc::now();
                let age_days = (now - review.created_at).num_days() as f64;
                let size = review.additions + review.deletions;

                // Age score (0-3): 0-3 days=1, 3-7=2, 7-14=3, 14-30=4, 30+=5
                let age_score = if age_days <= 3.0 {
                    1
                } else if age_days <= 7.0 {
                    2
                } else if age_days <= 14.0 {
                    3
                } else if age_days <= 30.0 {
                    4
                } else {
                    5
                };

                // Size score (0-2): <100=1, 100-500=2, 500+=3
                let size_score = if size < 100 {
                    1
                } else if size < 500 {
                    2
                } else {
                    3
                };

                // Draft penalty (drafts are less urgent)
                let draft_score = if review.draft { 1 } else { 2 };

                // Staleness bonus: if waiting >7 days, add urgency
                let staleness_bonus = if age_days > 14.0 {
                    2
                } else if age_days > 7.0 {
                    1
                } else {
                    0
                };

                let total = (age_score + size_score + draft_score + staleness_bonus).min(10);

                (
                    total,
                    AttentionFactors {
                        age_score,
                        size_score,
                        draft_score,
                        staleness_bonus,
                    },
                )
            }

            let mut attention_list: Vec<AttentionPR> = filtered_reviews
                .iter()
                .map(|r| {
                    let (attention_score, factors) = calc_attention_score(r);
                    AttentionPR {
                        repo: r.repo.clone(),
                        pr_number: r.pr_number,
                        pr_title: r.pr_title.clone(),
                        pr_author: r.pr_author.clone(),
                        pr_url: r.pr_url.clone(),
                        age_days: (Utc::now() - r.created_at).num_days(),
                        size: r.additions + r.deletions,
                        draft: r.draft,
                        attention_score,
                        factors,
                    }
                })
                .collect();

            // Sort by attention score descending
            attention_list.sort_by(|a, b| b.attention_score.cmp(&a.attention_score));

            // Filter by threshold
            let filtered: Vec<&AttentionPR> = attention_list
                .iter()
                .filter(|p| p.attention_score >= threshold)
                .take(limit)
                .collect();

            if json {
                #[derive(serde::Serialize)]
                struct AttentionOutput {
                    threshold: u8,
                    total_matching: usize,
                    high_attention: Vec<AttentionPR>,
                }
                let output = AttentionOutput {
                    threshold,
                    total_matching: attention_list
                        .iter()
                        .filter(|p| p.attention_score >= threshold)
                        .count(),
                    high_attention: filtered.into_iter().cloned().collect(),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                if filtered.is_empty() {
                    println!(
                        "✅ No PRs above attention threshold {} — you're in good shape!",
                        threshold
                    );
                    return Ok(());
                }

                println!(
                    "\n🎯 {} PR(s) demand your attention (score >= {})\n",
                    filtered.len(),
                    threshold
                );

                for pr in filtered {
                    let age_label = if pr.age_days == 0 {
                        "today".green().to_string()
                    } else if pr.age_days == 1 {
                        "1d".yellow().to_string()
                    } else if pr.age_days <= 3 {
                        format!("{}d", pr.age_days).yellow().to_string()
                    } else if pr.age_days <= 7 {
                        format!("{}d", pr.age_days).red().to_string()
                    } else {
                        format!("{}d!!", pr.age_days).red().bold().to_string()
                    };

                    let draft_label = if pr.draft {
                        " [DRAFT]".yellow().to_string()
                    } else {
                        String::new()
                    };
                    let stars = "🔥".repeat((pr.attention_score / 2).min(5) as usize);
                    let priority_display = if priority {
                        let p_score =
                            logger::calculate_priority_score(&crate::github::PendingReview {
                                repo: pr.repo.clone(),
                                pr_number: pr.pr_number,
                                pr_title: pr.pr_title.clone(),
                                pr_author: pr.pr_author.clone(),
                                pr_url: pr.pr_url.clone(),
                                created_at: chrono::Utc::now()
                                    - chrono::Duration::days(pr.age_days),
                                additions: pr.size,
                                deletions: 0,
                                draft: pr.draft,
                                branch: String::new(),
                            });
                        format!("  {}", logger::priority_stars(p_score))
                    } else {
                        String::new()
                    };

                    println!(
                        "  {}  {} {} ({}){}{}",
                        stars,
                        pr.pr_title.bold(),
                        format!("#{}", pr.pr_number).dimmed(),
                        pr.repo.dimmed(),
                        draft_label,
                        priority_display
                    );
                    println!(
                        "      👤 {}  •  {} lines  •  opened {}",
                        pr.pr_author.cyan(),
                        pr.size,
                        age_label
                    );

                    if detailed {
                        println!(
                            "      📊 breakdown: age={} size={} draft={} stale_bonus={}",
                            pr.factors.age_score,
                            pr.factors.size_score,
                            pr.factors.draft_score,
                            pr.factors.staleness_bonus
                        );
                    }
                    println!("      🔗 {}\n", pr.pr_url.blue().underline());
                }
            }
        }

        Commands::Focus {
            dry_run,
            all,
            limit,
            open,
            json,
            priority,
            repo,
            author,
            since_days,
            pr_number,
            pr_numbers,
            pr,
        } => {
            use chrono::Utc;

            // Target specific PR(s): global --pr > local --pr > local --pr-number > --pr-numbers
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else {
                Vec::new()
            };

            // If pr_numbers was provided, use those PRs directly
            if pr_numbers.is_some() {
                if prs_from_numbers.is_empty() {
                    println!("No PRs found for the specified numbers.");
                    return Ok(());
                }
                // Determine how many PRs to show
                let show_count = if all { limit.unwrap_or(10) } else { 1 };
                let focused_prs: Vec<_> = prs_from_numbers.into_iter().take(show_count).collect();
                // Apply sorting by priority
                let mut sorted = focused_prs.clone();
                sorted.sort_by(|a, b| {
                    let score_a = logger::calculate_priority_score(a);
                    let score_b = logger::calculate_priority_score(b);
                    let age_a = (Utc::now() - a.created_at).num_days();
                    let age_b = (Utc::now() - b.created_at).num_days();
                    (score_b, age_a).cmp(&(score_a, age_b))
                });
                let focused_prs = if all {
                    sorted
                } else {
                    sorted.into_iter().take(1).collect()
                };
                // Output (simplified - just show the PRs)
                for pr in &focused_prs {
                    let score = logger::calculate_priority_score(pr);
                    println!("#{} {} [⭐{}/5]", pr.pr_number, pr.pr_title, score);
                    println!(
                        "   {}  •  {}  •  +{}/-{} lines",
                        pr.repo, pr.pr_author, pr.additions, pr.deletions
                    );
                }
                return Ok(());
            }

            // Determine how many PRs to show
            let show_count = if all { limit.unwrap_or(10) } else { 1 };

            // Apply --repo and --author and --since-days filters (consistent with other commands)
            let filtered: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // If target_pr is set, fetch that specific PR
            let base_reviews: Vec<github::PendingReview> = if let Some(num) = target_pr {
                // Single PR via --pr or --pr-number - fetch and use that
                match github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await
                {
                    Ok(prs) => prs,
                    Err(e) => {
                        println!("❌ Failed to fetch PR #{}: {}", num, e);
                        return Ok(());
                    }
                }
            } else {
                filtered
            };

            // Filter out snoozed PRs (unless --pr is specified, consistent with list/delegate/search/filter/top commands)
            let filtered: Vec<_> = if target_pr.is_none() {
                let snooze_file = ctx
                    .output_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("./reviews"))
                    .join(".snoozed.json");

                let now = chrono::Utc::now();
                let snoozed_prs: Vec<(String, u64)> = if snooze_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&snooze_file) {
                        if let Ok(entries) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                        {
                            entries
                                .into_iter()
                                .filter_map(|e| {
                                    let repo = e.get("repo")?.as_str()?.to_string();
                                    let pr_number = e.get("pr_number")?.as_u64()?;
                                    let until_str = e.get("snoozed_until")?.as_str()?;
                                    if let Ok(until) =
                                        chrono::DateTime::parse_from_rfc3339(until_str)
                                    {
                                        if until.with_timezone(&chrono::Utc) > now {
                                            return Some((repo, pr_number));
                                        }
                                    }
                                    None
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                let _snoozed_count = snoozed_prs.len();
                base_reviews
                    .into_iter()
                    .filter(|r| {
                        !snoozed_prs
                            .iter()
                            .any(|(repo, num)| *num == r.pr_number && repo == &r.repo)
                    })
                    .inspect(|_| ())
                    .collect()
            } else {
                base_reviews
            };

            if filtered.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "focused": null,
                            "total_pending": ctx.reviews.len(),
                            "message": "No matching reviews found."
                        }))?
                    );
                } else {
                    println!("🎯 No matching PRs found.");
                    if let Some(pr_num) = target_pr {
                        println!("   PR #{} not found in pending reviews.", pr_num);
                    } else {
                        println!("   No PR matches your filters (--repo, --author).");
                    }
                    if !ctx.reviews.is_empty() {
                        println!("   You have {} total pending PRs.", ctx.reviews.len());
                    }
                }
                return Ok(());
            }

            // Sort by priority score (highest first), then by age (oldest first) for ties
            let mut sorted = filtered.clone();
            sorted.sort_by(|a, b| {
                let score_a = logger::calculate_priority_score(a);
                let score_b = logger::calculate_priority_score(b);
                let age_a = (Utc::now() - a.created_at).num_days();
                let age_b = (Utc::now() - b.created_at).num_days();
                (score_b, age_a).cmp(&(score_a, age_b))
            });

            // Take the top N PRs
            let focused_prs: Vec<_> = sorted.into_iter().take(show_count).collect();
            let focused_count = focused_prs.len();

            for (idx, pr) in focused_prs.into_iter().enumerate() {
                let score = logger::calculate_priority_score(&pr);
                let age_days = (Utc::now() - pr.created_at).num_days();
                let total_lines = pr.additions + pr.deletions;
                let is_single = focused_count == 1;

                if json {
                    #[derive(serde::Serialize)]
                    struct FocusOutput<'a> {
                        repo: &'a str,
                        pr_number: u64,
                        pr_title: &'a str,
                        pr_author: &'a str,
                        pr_url: &'a str,
                        score: u8,
                        age_days: i64,
                        additions: u64,
                        deletions: u64,
                        draft: bool,
                    }
                    let output = FocusOutput {
                        repo: &pr.repo,
                        pr_number: pr.pr_number,
                        pr_title: &pr.pr_title,
                        pr_author: &pr.pr_author,
                        pr_url: &pr.pr_url,
                        score,
                        age_days,
                        additions: pr.additions,
                        deletions: pr.deletions,
                        draft: pr.draft,
                    };
                    // For --all, print each JSON object on its own line
                    // For single, print pretty JSON
                    if all {
                        println!("{}", serde_json::to_string(&output)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    }
                } else {
                    let score_stars = logger::priority_stars(score);
                    let age_label = if age_days == 0 {
                        "today".green().to_string()
                    } else if age_days == 1 {
                        "1 day ago".yellow().to_string()
                    } else if age_days <= 3 {
                        format!("{} days ago", age_days).yellow().to_string()
                    } else if age_days <= 7 {
                        format!("{} days ago", age_days).red().to_string()
                    } else {
                        format!("{} days ago!!", age_days).red().bold().to_string()
                    };

                    let draft_label = if pr.draft {
                        " [DRAFT]".yellow().to_string()
                    } else {
                        String::new()
                    };
                    let header = if is_single {
                        "🎯 YOUR FOCUS PR".to_string()
                    } else {
                        format!("🎯 FOCUS PR #{}/{}", idx + 1, focused_count)
                    };

                    println!();
                    println!("{}", header);
                    println!("{}", "─".repeat(50));
                    println!();
                    println!("  #{}  {}{}", pr.pr_number, pr.pr_title.bold(), draft_label);
                    println!();
                    println!(
                        "  📁 {}  👤 {}  ⏱️ {}  📊 {}/{}",
                        pr.repo.split('/').next_back().unwrap_or(&pr.repo).dimmed(),
                        pr.pr_author.cyan(),
                        age_label,
                        pr.additions.to_string().green(),
                        pr.deletions.to_string().red()
                    );
                    println!(
                        "  📏 Total: {} lines  {}",
                        total_lines,
                        score_stars.red().bold()
                    );
                    if priority {
                        println!(
                            "  ⭐ Priority: {}/5  {}",
                            score,
                            logger::priority_stars(score)
                        );
                    }
                    println!("  🔗 {}", pr.pr_url.blue().underline());
                    println!();

                    // Only show footer for the last PR
                    if idx == focused_count - 1 {
                        println!("{}", "─".repeat(50));
                        println!(
                            "  Total pending: {} PRs ({} matching filters)",
                            ctx.reviews.len(),
                            filtered.len()
                        );
                        if filtered.len() > show_count {
                            println!("  Run `prctrl focus --all` to see more, or `prctrl top` for full list");
                        }
                        println!();
                    }

                    if open && is_single {
                        open::that(&pr.pr_url)?;
                        println!("🖥️  Opening PR in browser...");
                    }

                    if dry_run {
                        println!();
                        println!("(dry-run - no action taken)");
                    }
                }
            }
        }

        Commands::Conflicts {
            only_conflicts,
            all,
            pr_numbers,
            pr_number,
            pr,
            repo,
            author,
            since_days,
            priority,
            json,
        } => {
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            let targets: Vec<_> = if let Some(num) = target_pr {
                // When --pr is specified, bypass filters and fetch directly
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if all {
                if ctx.reviews.is_empty() {
                    println!("No pending reviews found.");
                    return Ok(());
                }
                // Apply --repo, --author, and --since-days filters to --all results
                let mut filtered = ctx.reviews.clone();
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    filtered.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    filtered.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    filtered.retain(|r| r.created_at >= cutoff);
                }
                filtered
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Parallel fetch all specified PRs
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                if ctx.reviews.is_empty() {
                    println!("No pending reviews found.");
                    return Ok(());
                }
                // Apply --repo, --author, and --since-days filters early (reduce API calls)
                let mut filtered = ctx.reviews.clone();
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    filtered.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    filtered.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    filtered.retain(|r| r.created_at >= cutoff);
                }
                filtered
            };

            if targets.is_empty() {
                println!("No PRs to check conflict status for.");
                return Ok(());
            }

            println!(
                "\n🔍 Checking merge conflict status for {} PR(s)...\n",
                targets.len()
            );
            io::stdout().flush()?;

            match github::fetch_merge_conflict_status(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &targets,
            )
            .await
            {
                Ok(statuses) => {
                    let conflict_count = statuses.iter().filter(|s| s.has_conflicts).count();
                    let clean_count = statuses.len() - conflict_count;

                    if json {
                        #[derive(serde::Serialize)]
                        struct ConflictOutput<'a> {
                            repo: &'a str,
                            pr_number: u64,
                            pr_title: &'a str,
                            has_conflicts: bool,
                            mergeable: Option<bool>,
                            rebaseable: Option<bool>,
                        }
                        let output: Vec<ConflictOutput> = statuses
                            .iter()
                            .map(|s| ConflictOutput {
                                repo: &s.repo,
                                pr_number: s.pr_number,
                                pr_title: &s.pr_title,
                                has_conflicts: s.has_conflicts,
                                mergeable: s.mergeable,
                                rebaseable: s.rebaseable,
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("\n⚠️  Merge Conflict Report\n{}", "─".repeat(50));
                        println!(
                            "  ❌ PRs with conflicts: {}",
                            conflict_count.to_string().red().bold()
                        );
                        println!(
                            "  ✅ Clean PRs:           {}",
                            clean_count.to_string().green().bold()
                        );
                        println!("{}", "─".repeat(50));

                        // Build a lookup from (repo, pr_number) -> PriorityReview for priority scores
                        let priority_lookup: std::collections::HashMap<
                            (String, u64),
                            &github::PendingReview,
                        > = targets
                            .iter()
                            .map(|r| ((r.repo.clone(), r.pr_number), r))
                            .collect();

                        // Sort: conflicts first, then by repo
                        let mut sorted = statuses.clone();
                        sorted.sort_by(|a, b| {
                            let a_conflict = if a.has_conflicts { 0 } else { 1 };
                            let b_conflict = if b.has_conflicts { 0 } else { 1 };
                            if a_conflict != b_conflict {
                                a_conflict.cmp(&b_conflict)
                            } else {
                                a.repo.cmp(&b.repo)
                            }
                        });

                        let mut shown_any = false;
                        for status in sorted {
                            if only_conflicts && !status.has_conflicts {
                                continue;
                            }
                            shown_any = true;

                            // Get priority score if available
                            let priority_label = if priority {
                                if let Some(review) =
                                    priority_lookup.get(&(status.repo.clone(), status.pr_number))
                                {
                                    let score = logger::calculate_priority_score(review);
                                    format!("  ⭐ {}/5", score)
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };

                            if status.has_conflicts {
                                let rebase_label = if status.rebaseable == Some(true) {
                                    " [rebaseable]".yellow()
                                } else {
                                    "".normal()
                                };
                                println!(
                                    "\n  ❌ #{} {} ({}){}{}",
                                    status.pr_number,
                                    status.pr_title.bold().red(),
                                    status.repo.dimmed(),
                                    rebase_label,
                                    priority_label
                                );
                                println!("      ⚠️  Cannot merge - has merge conflicts");
                                println!(
                                    "      🔗 {}",
                                    format!("{}/pull/{}", status.repo, status.pr_number)
                                        .blue()
                                        .underline()
                                );
                            } else {
                                println!(
                                    "\n  ✅ #{} {} ({}){}",
                                    status.pr_number,
                                    status.pr_title.bold().green(),
                                    status.repo.dimmed(),
                                    priority_label
                                );
                            }
                        }

                        if !shown_any {
                            if only_conflicts {
                                println!("\n  🎉 No PRs with conflicts found!\n");
                            } else {
                                println!("\n  No PRs to display.\n");
                            }
                        }

                        println!("{}", "─".repeat(50));
                        if !only_conflicts {
                            println!("\n💡 Use `--only-conflicts` or `-c` to show only PRs with conflicts");
                        }
                        println!();
                    }
                }
                Err(e) => {
                    println!("{}", "❌ Failed to check conflicts".red());
                    println!("   Error: {}", e);
                }
            }
        }

        Commands::Ci {
            failed_only,
            passing_only,
            all,
            pr_numbers,
            pr_number,
            pr,
            repo,
            author,
            since_days,
            priority,
            json,
        } => {
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo, --author, and --since-days filters (consistent with other batch commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            let targets: Vec<_> = if let Some(num) = target_pr {
                // When --pr is specified, bypass filters and fetch directly
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if all {
                // --all flag: use all filtered pending ctx.reviews
                if filtered_reviews.is_empty() {
                    println!("No pending reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Parallel fetch all specified PRs
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                // Interactive selection uses filtered_reviews
                filtered_reviews
            };

            if targets.is_empty() {
                println!("No PRs to check CI status for.");
                return Ok(());
            }

            println!("\n🔧 Checking CI status for {} PR(s)...\n", targets.len());
            io::stdout().flush()?;

            match github::fetch_ci_status(&ctx.cfg.github_token, &ctx.cfg.github_org, &targets)
                .await
            {
                Ok(statuses) => {
                    // Pair CI statuses with original pending ctx.reviews for priority calculation
                    let statuses_with_reviews: Vec<(&github::CiStatus, &github::PendingReview)> =
                        statuses
                            .iter()
                            .filter_map(|s| {
                                targets
                                    .iter()
                                    .find(|r| r.pr_number == s.pr_number && r.repo == s.repo)
                                    .map(|r| (s, r))
                            })
                            .collect();

                    // Apply filters
                    let filtered: Vec<_> = statuses_with_reviews
                        .iter()
                        .filter(|(s, _)| {
                            if failed_only {
                                // Failing: overall failure OR any check failed
                                s.overall_status == "failure"
                                    || s.checks
                                        .iter()
                                        .any(|c| c.conclusion.as_deref() == Some("failure"))
                            } else if passing_only {
                                // Passing: overall success AND all checks passed (or no checks configured)
                                s.overall_status == "success"
                                    && (s.checks.is_empty()
                                        || s.checks
                                            .iter()
                                            .all(|c| c.conclusion.as_deref() == Some("success")))
                            } else {
                                true
                            }
                        })
                        .map(|(s, r)| (*s, *r))
                        .collect();

                    if filtered.is_empty() {
                        if failed_only {
                            println!("🎉 No PRs with failing CI checks!\n");
                        } else if passing_only {
                            println!("❌ No PRs with fully passing CI.\n");
                        } else {
                            println!("No CI status data available.\n");
                        }
                        return Ok(());
                    }

                    let failure_count = filtered
                        .iter()
                        .filter(|(s, _)| {
                            s.overall_status == "failure"
                                || s.checks
                                    .iter()
                                    .any(|c| c.conclusion.as_deref() == Some("failure"))
                        })
                        .count();
                    let success_count = filtered.len() - failure_count;

                    if json {
                        // For JSON output, include priority score if enabled
                        if priority {
                            #[derive(serde::Serialize)]
                            struct CiStatusWithPriority<'a> {
                                #[serde(flatten)]
                                status: &'a github::CiStatus,
                                priority_score: u8,
                                priority_stars: String,
                            }
                            let output: Vec<CiStatusWithPriority> = filtered
                                .iter()
                                .map(|(s, r)| {
                                    let score = logger::calculate_priority_score(r);
                                    CiStatusWithPriority {
                                        status: s,
                                        priority_score: score,
                                        priority_stars: logger::priority_stars(score),
                                    }
                                })
                                .collect();
                            println!("{}", serde_json::to_string_pretty(&output)?);
                        } else {
                            let statuses_only: Vec<_> = filtered.iter().map(|(s, _)| s).collect();
                            println!("{}", serde_json::to_string_pretty(&statuses_only)?);
                        }
                    } else {
                        println!("\n🔧 CI Status Report\n{}", "─".repeat(50));
                        println!("  ❌ Failing:  {}", failure_count.to_string().red().bold());
                        println!(
                            "  ✅ Passing:  {}",
                            success_count.to_string().green().bold()
                        );
                        println!("{}", "─".repeat(50));

                        // Sort: failures first, then by repo
                        let mut sorted = filtered.clone();
                        sorted.sort_by(|(a, _), (b, _)| {
                            let a_fail = if a.overall_status == "failure"
                                || a.checks
                                    .iter()
                                    .any(|c| c.conclusion.as_deref() == Some("failure"))
                            {
                                0
                            } else {
                                1
                            };
                            let b_fail = if b.overall_status == "failure"
                                || b.checks
                                    .iter()
                                    .any(|c| c.conclusion.as_deref() == Some("failure"))
                            {
                                0
                            } else {
                                1
                            };
                            if a_fail != b_fail {
                                a_fail.cmp(&b_fail)
                            } else {
                                a.repo.cmp(&b.repo)
                            }
                        });

                        for (status, review) in &sorted {
                            let has_failure = status.overall_status == "failure"
                                || status
                                    .checks
                                    .iter()
                                    .any(|c| c.conclusion.as_deref() == Some("failure"));
                            let has_success = status
                                .checks
                                .iter()
                                .all(|c| c.conclusion.as_deref() == Some("success"));
                            let has_in_progress =
                                status.checks.iter().any(|c| c.status == "in_progress");

                            let status_icon = if has_failure {
                                "❌".red()
                            } else if has_success {
                                "✅".green()
                            } else if has_in_progress {
                                "⏳".yellow()
                            } else {
                                "⚪".normal()
                            };

                            let status_label = if has_failure {
                                "FAILING".red().to_string()
                            } else if has_success {
                                "PASSING".green().to_string()
                            } else if has_in_progress {
                                "IN PROGRESS".yellow().to_string()
                            } else {
                                "UNKNOWN".dimmed().to_string()
                            };

                            let priority_display = if priority {
                                let score = logger::calculate_priority_score(review);
                                format!("  ⭐ {}/5", score)
                            } else {
                                String::new()
                            };

                            println!(
                                "\n{} #{}  {}{}  ({})\n    Status: {}",
                                status_icon,
                                status.pr_number,
                                status.pr_title.bold(),
                                priority_display,
                                status.repo.dimmed(),
                                status_label
                            );

                            if status.checks.is_empty() {
                                println!("    (no checks configured)");
                            } else {
                                for check in &status.checks {
                                    let check_status_icon = match check.conclusion.as_deref() {
                                        Some("success") => "✅".to_string(),
                                        Some("failure") => "❌".to_string(),
                                        Some("cancelled") | Some("skipped") => "⚠️ ".to_string(),
                                        Some("timed_out") => "⏱️ ".to_string(),
                                        Some("neutral") => "➖".to_string(),
                                        Some("action_required") => "🔔".to_string(),
                                        None if check.status == "in_progress" => {
                                            "🔄 ".yellow().to_string()
                                        }
                                        None if check.status == "queued"
                                            || check.status == "waiting" =>
                                        {
                                            "⏳ ".yellow().to_string()
                                        }
                                        _ => "⚪ ".to_string(),
                                    };
                                    let check_conclusion =
                                        check.conclusion.as_deref().unwrap_or(&check.status);
                                    println!(
                                        "      {}{}  ({})",
                                        check_status_icon,
                                        check.name.dimmed(),
                                        check_conclusion.dimmed()
                                    );
                                }
                            }
                        }

                        println!("{}", "─".repeat(50));
                        println!("\n💡 Use `ci --failed-only` or `-f` to show only failing PRs");
                        println!("💡 Use `ci --passing-only` or `-p` to show only passing PRs\n");
                    }
                }
                Err(e) => {
                    println!("{}", "❌ Failed to check CI status".red());
                    println!("   Error: {}", e);
                }
            }
        }

        Commands::Health { json, suggest } => {
            println!("\n🏥 Fetching GitHub API health status...\n");
            io::stdout().flush()?;

            match github::fetch_health_status(&ctx.cfg.github_token).await {
                Ok(health) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&health)?);
                    } else {
                        println!("🏥 GitHub API Health Status\n{}", "─".repeat(50));

                        // Authentication status
                        if health.authenticated {
                            println!(
                                "  ✅ Authenticated as: {}",
                                health
                                    .username
                                    .as_ref()
                                    .unwrap_or(&"unknown".to_string())
                                    .cyan()
                            );
                        } else {
                            println!("  ❌ Not authenticated - check your GITHUB_TOKEN");
                        }
                        println!();

                        // Rate limits
                        let mut critical_limits = Vec::new();
                        let mut low_limits = Vec::new();

                        for limit in &health.rate_limits {
                            let usage_pct = if limit.limit > 0 {
                                (limit.remaining as f64 / limit.limit as f64) * 100.0
                            } else {
                                100.0
                            };

                            let status_icon = if limit.remaining == 0 {
                                "🔴".red()
                            } else if usage_pct < 10.0 {
                                "🟡".yellow()
                            } else if usage_pct < 50.0 {
                                "🟠".yellow()
                            } else {
                                "🟢".green()
                            };

                            let reset_formatted = if limit.reset_in_seconds > 0 {
                                let hours = limit.reset_in_seconds / 3600;
                                let mins = (limit.reset_in_seconds % 3600) / 60;
                                if hours > 0 {
                                    format!("resets in {}h {}m", hours, mins)
                                } else {
                                    format!("resets in {}m", mins)
                                }
                            } else {
                                "already reset".to_string()
                            };

                            // Visual bar (shows USED quota, not remaining)
                            let bar_width = 20;
                            let used_pct = 100.0 - usage_pct; // Invert to show used quota
                            let bar: String = if limit.limit > 0 {
                                let filled =
                                    ((used_pct / 100.0) * bar_width as f64).round() as usize;
                                let empty = bar_width - filled;
                                // Color the bar based on used percentage (high usage = red)
                                let bar_color: colored::Color = if used_pct > 80.0 {
                                    colored::Color::Red
                                } else if used_pct > 50.0 {
                                    colored::Color::Yellow
                                } else {
                                    colored::Color::Green
                                };
                                format!(
                                    "{}{}",
                                    "█".repeat(filled).color(bar_color),
                                    "░".repeat(empty).truecolor(60, 60, 60)
                                )
                            } else {
                                "░".repeat(bar_width)
                            };

                            let usage_str = format!("{}/{}", limit.remaining, limit.limit);
                            let reset_str = reset_formatted;

                            println!(
                                "  {} {:12} {} {} ({})",
                                status_icon,
                                limit.resource,
                                bar,
                                usage_str.dimmed(),
                                reset_str.dimmed()
                            );

                            // Categorize for suggestions
                            if limit.remaining == 0 {
                                critical_limits.push(limit.resource.clone());
                            } else if usage_pct < 10.0 {
                                low_limits.push((
                                    limit.resource.clone(),
                                    limit.remaining,
                                    limit.limit,
                                ));
                            }
                        }

                        if health.rate_limit_warning {
                            println!();
                            println!(
                                "  ⚠️  {}",
                                "Rate limit warning: API quota is running low!"
                                    .yellow()
                                    .bold()
                            );
                        }

                        println!("{}", "─".repeat(50));
                        let server_time_str = health
                            .server_time
                            .format("%Y-%m-%d %H:%M:%S UTC")
                            .to_string();
                        println!("  🕐 Server time: {}", server_time_str.dimmed());

                        // Suggestions based on rate limit status
                        if suggest {
                            println!();
                            println!("💡 Recommendations\n{}", "─".repeat(50));

                            // Get core limit remaining for calculations
                            let core_remaining = health
                                .rate_limits
                                .iter()
                                .find(|l| l.resource == "core")
                                .map(|l| l.remaining)
                                .unwrap_or(0);

                            // API cost estimates table
                            println!();
                            println!("  📊 Command API Costs (approximate GitHub API calls):");
                            println!();
                            println!("  {:25} {:>6}   Notes", "Command", "Calls");
                            println!("  {}", "─".repeat(60));

                            struct CmdCost<'a> {
                                name: &'a str,
                                calls: u32,
                                notes: &'a str,
                            }
                            let cmd_costs = vec![
                                CmdCost {
                                    name: "list",
                                    calls: 1,
                                    notes: "per repo (list PRs)",
                                },
                                CmdCost {
                                    name: "list --all",
                                    calls: 3,
                                    notes: "all repos + details",
                                },
                                CmdCost {
                                    name: "stats",
                                    calls: 2,
                                    notes: "list + per-PR details",
                                },
                                CmdCost {
                                    name: "team-summary",
                                    calls: 2,
                                    notes: "list + per-PR details",
                                },
                                CmdCost {
                                    name: "summary",
                                    calls: 1,
                                    notes: "lightweight aggregate",
                                },
                                CmdCost {
                                    name: "search",
                                    calls: 2,
                                    notes: "list + per-PR details",
                                },
                                CmdCost {
                                    name: "delegate",
                                    calls: 8,
                                    notes: "+PR details +CLAUDE API",
                                },
                                CmdCost {
                                    name: "delegate --dry-run",
                                    calls: 3,
                                    notes: "preview without CLAUDE",
                                },
                                CmdCost {
                                    name: "focus",
                                    calls: 4,
                                    notes: "list + details +score",
                                },
                                CmdCost {
                                    name: "top",
                                    calls: 3,
                                    notes: "list + details +score",
                                },
                                CmdCost {
                                    name: "ci",
                                    calls: 6,
                                    notes: "+ status + check runs",
                                },
                                CmdCost {
                                    name: "conflicts",
                                    calls: 4,
                                    notes: "list + merge status",
                                },
                                CmdCost {
                                    name: "info",
                                    calls: 2,
                                    notes: "single PR full details",
                                },
                                CmdCost {
                                    name: "files",
                                    calls: 4,
                                    notes: "list + per-PR file diff",
                                },
                                CmdCost {
                                    name: "labels",
                                    calls: 4,
                                    notes: "list + per-PR labels",
                                },
                                CmdCost {
                                    name: "timeline",
                                    calls: 3,
                                    notes: "list + timeline events",
                                },
                                CmdCost {
                                    name: "review",
                                    calls: 3,
                                    notes: "full diff with patches",
                                },
                                CmdCost {
                                    name: "mentions",
                                    calls: 4,
                                    notes: "notifications + PR details",
                                },
                                CmdCost {
                                    name: "activity",
                                    calls: 8,
                                    notes: "all repos + timelines",
                                },
                                CmdCost {
                                    name: "digest",
                                    calls: 6,
                                    notes: "aggregate + trends",
                                },
                            ];

                            for cmd in &cmd_costs {
                                let affordable = if core_remaining >= 10 {
                                    "🟢".green()
                                } else if core_remaining >= cmd.calls {
                                    "🟡".yellow()
                                } else {
                                    "🔴".red()
                                };
                                println!(
                                    "  {} {:25} {:>5}   {}",
                                    affordable,
                                    cmd.name,
                                    format!("~{}", cmd.calls),
                                    cmd.notes.dimmed()
                                );
                            }

                            println!();
                            println!("  💡 Tips:");
                            println!("    • Commands with --dry-run cost ~3x less (skip external API calls)");
                            println!(
                                "    • Batch commands (--all) multiply API costs by number of PRs"
                            );
                            println!("    • Use --json to reduce output parsing overhead");
                            println!();

                            if critical_limits.is_empty() && low_limits.is_empty() {
                                println!("  ✅ All rate limits look healthy. You're good to go!");
                                println!();
                                println!("  Commands to run freely:");
                                println!("    • prctrl list --all  (fetch all PRs)");
                                println!("    • prctrl team-summary");
                                println!("    • prctrl delegate --dry-run");
                            } else if !critical_limits.is_empty() {
                                let critical = critical_limits.join(", ");
                                println!("  🔴 CRITICAL: {} limit(s) exhausted", critical);
                                println!();
                                println!("  Immediate actions:");
                                println!("    • Stop any batch operations immediately");
                                println!("    • Wait for reset (or use --json to check timing)");
                                println!();
                                println!("  When limits reset, consider:");
                                println!(
                                    "    • Use --json for scripting to reduce output overhead"
                                );
                                println!("    • Avoid commands that fetch full diffs or files");
                                println!("    • Run critical commands first before they reset");

                                // Find when core limit resets
                                for limit in &health.rate_limits {
                                    if limit.resource == "core" && limit.reset_in_seconds > 0 {
                                        let hours = limit.reset_in_seconds / 3600;
                                        let mins = (limit.reset_in_seconds % 3600) / 60;
                                        println!();
                                        println!("  ⏱️  Core limit resets in {}h {}m", hours, mins);
                                        break;
                                    }
                                }
                            } else {
                                let low = low_limits
                                    .iter()
                                    .map(|(r, _, _)| r.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                println!("  🟡 LOW: {} limit(s) below 10%", low);
                                println!();
                                println!(
                                    "  Current budget: ~{} core API calls remaining",
                                    core_remaining
                                );
                                println!();
                                println!("  Safe commands to run now:");
                                println!("    • prctrl summary");
                                println!("    • prctrl list (no --all)");
                                println!("    • prctrl stats");
                                println!();
                                println!("  Commands to avoid until reset:");
                                println!("    • delegate --all (high API cost)");
                                println!("    • activity (multiple timeline fetches)");
                                println!("    • review (full diff with patches)");
                                println!();
                                println!("  Lower-cost alternatives:");
                                println!("    • prctrl search (title filter)");
                                println!("    • prctrl filter (client-side)");
                                println!("    • prctrl browse (URLs only)");

                                // Show safe remaining calls
                                if let Some((_, remaining, limit)) =
                                    low_limits.iter().find(|(r, _, _)| *r == "core")
                                {
                                    println!();
                                    println!(
                                        "  📊 Core API: ~{} calls remaining before throttle",
                                        remaining
                                    );
                                    if *limit > 0 {
                                        let safe_batch = (*remaining as f64 * 0.3).round() as usize;
                                        println!(
                                            "  📊 Suggested safe batch size: {} requests",
                                            safe_batch
                                        );
                                    }
                                }
                            }

                            println!();
                            println!("  Run without --suggest for basic status only");
                        } else {
                            println!();
                            println!("  💡 Use `--suggest` for actionable recommendations");
                        }
                        println!("{}", "─".repeat(50));
                    }
                }
                Err(e) => {
                    println!("{}", "❌ Failed to fetch health status".red());
                    println!("   Error: {}", e);
                }
            }
        }

        Commands::Export {
            format,
            output,
            columns,
            pr_number,
            pr_numbers,
            pr,
            all,
            json,
            repo,
            author,
            since_days,
            priority,
        } => {
            use chrono::Utc;

            let target_pr = ctx.cli.pr.or(pr).or(pr_number);
            let export_format = format.as_deref().unwrap_or("csv").to_lowercase();

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                let all_prs: Vec<_> = join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect();
                all_prs
            } else {
                Vec::new()
            };

            let reviews_to_export = if let Some(num) = target_pr {
                // Single PR via --pr or positional - bypass other filters
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if pr_numbers.is_some() {
                // Batch mode via --pr-numbers - use fetched PRs
                prs_from_numbers
            } else if all {
                // --all flag: fetch fresh data for all pending ctx.reviews
                github::fetch_pending_reviews(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    &ctx.cfg.github_username,
                    &ctx.cfg.github_teams,
                    ctx.cli.include_mine,
                    ctx.cli.include_drafts,
                    &ctx.cli.exclude_prefix,
                    &ctx.cfg.crew_members,
                    ctx.cfg.max_pr_age_days,
                )
                .await?
            } else {
                ctx.reviews.clone()
            };

            // Apply --repo filter (partial match, case-insensitive)
            let reviews_to_export: Vec<_> = if let Some(ref repo_filter) = repo {
                let pattern = repo_filter.to_lowercase();
                reviews_to_export
                    .into_iter()
                    .filter(|r| r.repo.to_lowercase().contains(&pattern))
                    .collect()
            } else {
                reviews_to_export
            };

            // Apply --author filter (partial match, case-insensitive)
            let reviews_to_export: Vec<_> = if let Some(ref author_filter) = author {
                let pattern = author_filter.to_lowercase();
                reviews_to_export
                    .into_iter()
                    .filter(|r| r.pr_author.to_lowercase().contains(&pattern))
                    .collect()
            } else {
                reviews_to_export
            };

            // Apply --since-days filter
            let reviews_to_export: Vec<_> = if let Some(days) = since_days {
                let cutoff = Utc::now() - chrono::Duration::days(days as i64);
                reviews_to_export
                    .into_iter()
                    .filter(|r| r.created_at >= cutoff)
                    .collect()
            } else {
                reviews_to_export
            };

            if reviews_to_export.is_empty() {
                println!("No reviews to export.");
                return Ok(());
            }

            // Parse columns (default: all) - only used for CSV/Markdown
            let default_cols = vec![
                "repo", "number", "title", "author", "size", "age", "draft", "url",
            ];
            let cols: Vec<&str> = if let Some(ref c) = columns {
                c.split(',').map(|s| s.trim()).collect()
            } else {
                default_cols.clone()
            };

            let now = Utc::now();
            let mut output_content = String::new();

            // JSON output - uses all fields, respects --columns for filtering
            if json {
                #[derive(serde::Serialize)]
                struct ExportRecord<'a> {
                    repo: &'a str,
                    number: u64,
                    title: &'a str,
                    author: &'a str,
                    size: String,
                    age_days: i64,
                    draft: bool,
                    url: &'a str,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    priority_score: Option<u8>,
                }

                let records: Vec<ExportRecord> = reviews_to_export
                    .iter()
                    .map(|r| {
                        let age = r.created_at.signed_duration_since(now);
                        let age_days = age.num_days().abs();
                        ExportRecord {
                            repo: &r.repo,
                            number: r.pr_number,
                            title: &r.pr_title,
                            author: &r.pr_author,
                            size: format!("+{}/-{}", r.additions, r.deletions),
                            age_days,
                            draft: r.draft,
                            url: &r.pr_url,
                            priority_score: if priority {
                                Some(logger::calculate_priority_score(r))
                            } else {
                                None
                            },
                        }
                    })
                    .collect();

                output_content = serde_json::to_string_pretty(&records).unwrap_or_default();
                output_content.push('\n'); // Ensure trailing newline for stdout
            } else if export_format == "markdown" || export_format == "md" {
                // Markdown table format
                // Header
                output_content.push_str("| ");
                for col in &cols {
                    output_content.push_str(match *col {
                        "repo" => "Repo",
                        "number" => "#",
                        "title" => "Title",
                        "author" => "Author",
                        "size" => "Size",
                        "age" => "Age",
                        "draft" => "Draft",
                        "url" => "URL",
                        "priority" => "Priority",
                        _ => col,
                    });
                    output_content.push_str(" | ");
                }
                // Add priority column header if --priority is set
                if priority {
                    output_content.push_str(" | ");
                }
                output_content.push_str("\n| ");
                for _ in &cols {
                    output_content.push_str("---|");
                }
                if priority {
                    output_content.push_str("---|");
                }
                output_content.push('\n');

                // Rows
                for r in &reviews_to_export {
                    output_content.push_str("| ");
                    for col in &cols {
                        match *col {
                            "repo" => output_content.push_str(&format!("`{}` | ", r.repo)),
                            "number" => output_content.push_str(&format!("#{} | ", r.pr_number)),
                            "title" => output_content.push_str(&format!("{} | ", r.pr_title)),
                            "author" => output_content.push_str(&format!("{} | ", r.pr_author)),
                            "size" => output_content
                                .push_str(&format!("+{}/-{} | ", r.additions, r.deletions)),
                            "age" => {
                                let age = r.created_at.signed_duration_since(now);
                                let age_days = age.num_days().abs();
                                output_content.push_str(&format!("{}d | ", age_days));
                            }
                            "draft" => output_content
                                .push_str(&format!("{} | ", if r.draft { "yes" } else { "no" })),
                            "url" => output_content.push_str(&format!("[link]({}) | ", r.pr_url)),
                            "priority" => {
                                let score = logger::calculate_priority_score(r);
                                output_content.push_str(&format!("{}/5 | ", score));
                            }
                            _ => output_content.push_str(&format!("{} | ", col)),
                        }
                    }
                    output_content.push('\n');
                }
            } else {
                // CSV format (default)
                // Build header including priority if needed
                let mut header_cols = cols.to_vec();
                if priority && !header_cols.contains(&"priority") {
                    header_cols.push("priority");
                }

                // Header
                output_content.push_str(&header_cols.join(","));
                output_content.push('\n');

                // Rows
                for r in &reviews_to_export {
                    for (i, col) in cols.iter().enumerate() {
                        if i > 0 {
                            output_content.push(',');
                        }
                        match *col {
                            "repo" => output_content.push_str(&r.repo),
                            "number" => output_content.push_str(&r.pr_number.to_string()),
                            "title" => {
                                // Escape quotes in title
                                let escaped = r.pr_title.replace('"', "\"\"");
                                output_content.push_str(&format!("\"{}\"", escaped));
                            }
                            "author" => output_content.push_str(&r.pr_author),
                            "size" => output_content
                                .push_str(&format!("+{}/-{}", r.additions, r.deletions)),
                            "age" => {
                                let age = r.created_at.signed_duration_since(now);
                                let age_days = age.num_days().abs();
                                output_content.push_str(&format!("{}d", age_days));
                            }
                            "draft" => output_content.push_str(if r.draft { "yes" } else { "no" }),
                            "url" => output_content.push_str(&r.pr_url),
                            _ => output_content.push_str(col),
                        }
                    }
                    // Add priority score if --priority is set
                    if priority {
                        output_content.push(',');
                        let score = logger::calculate_priority_score(r);
                        output_content.push_str(&score.to_string());
                    }
                    output_content.push('\n');
                }
            }

            // Write output
            if let Some(ref path) = output {
                std::fs::write(path, &output_content)?;
                if json {
                    println!(
                        "✅ Exported {} reviews to {} (JSON)",
                        reviews_to_export.len(),
                        path.display()
                    );
                } else {
                    println!(
                        "✅ Exported {} reviews to {}",
                        reviews_to_export.len(),
                        path.display()
                    );
                }
            } else {
                print!("{}", output_content);
            }
        }

        Commands::History {
            repo,
            author,
            state,
            days,
            limit,
            json,
        } => {
            use std::collections::HashMap;

            let history_output_dir = ctx
                .output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./reviews"));

            if !history_output_dir.exists() {
                println!(
                    "❌ No reviews directory found at {}. Run `prctrl list` first to save reviews.",
                    history_output_dir.display()
                );
                return Ok(());
            }

            #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
            struct HistoryEntry {
                pub repo: String,
                pub pr_number: u64,
                pub pr_title: String,
                pub author: String,
                pub reviewed_at: String,
                pub state: String,
                pub lines_added: u64,
                pub lines_deleted: u64,
            }

            let mut all_entries: Vec<HistoryEntry> = Vec::new();
            let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);

            if let Ok(entries) = std::fs::read_dir(&history_output_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            let lines: Vec<&str> = content.lines().collect();
                            if lines.len() >= 4 {
                                let pr_title = lines.first().unwrap_or(&"").trim().to_string();
                                let date_line = lines.iter().find(|l| l.starts_with("Reviewed on"));
                                let reviewed_at = date_line
                                    .and_then(|l| l.strip_prefix("Reviewed on "))
                                    .unwrap_or("")
                                    .trim();

                                if let Ok(date) = chrono::DateTime::parse_from_rfc3339(reviewed_at)
                                {
                                    if date.with_timezone(&chrono::Utc) < cutoff {
                                        continue;
                                    }

                                    let repo = lines
                                        .get(1)
                                        .unwrap_or(&"")
                                        .replace("Repository: ", "")
                                        .trim()
                                        .to_string();
                                    let author = lines
                                        .get(2)
                                        .unwrap_or(&"")
                                        .replace("Author: ", "")
                                        .trim()
                                        .to_string();
                                    let state = lines
                                        .get(3)
                                        .unwrap_or(&"")
                                        .replace("Review state: ", "")
                                        .trim()
                                        .to_string();

                                    // Try to extract lines added/deleted
                                    let lines_added: u64 = lines
                                        .iter()
                                        .find(|l| l.contains("+") && l.contains("additions"))
                                        .and_then(|l| l.split('+').nth(1))
                                        .and_then(|s| s.split('/').next())
                                        .and_then(|s| s.parse().ok())
                                        .unwrap_or(0);
                                    let lines_deleted: u64 = lines
                                        .iter()
                                        .find(|l| l.contains("-") && l.contains("deletions"))
                                        .and_then(|l| l.split('-').nth(1))
                                        .and_then(|s| s.split('/').next())
                                        .and_then(|s| s.trim().parse().ok())
                                        .unwrap_or(0);

                                    all_entries.push(HistoryEntry {
                                        repo,
                                        pr_number: 0, // Will be extracted from content if needed
                                        pr_title,
                                        author,
                                        reviewed_at: reviewed_at.to_string(),
                                        state,
                                        lines_added,
                                        lines_deleted,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Filter entries
            if let Some(ref repo_filter) = repo {
                all_entries.retain(|e| e.repo.to_lowercase().contains(&repo_filter.to_lowercase()));
            }
            if let Some(ref author_filter) = author {
                all_entries.retain(|e| {
                    e.author
                        .to_lowercase()
                        .contains(&author_filter.to_lowercase())
                });
            }
            if let Some(ref state_filter) = state {
                all_entries.retain(|e| {
                    e.state
                        .to_uppercase()
                        .contains(&state_filter.to_uppercase())
                });
            }

            // Sort by most recent
            all_entries.sort_by(|a, b| b.reviewed_at.cmp(&a.reviewed_at));

            let total = all_entries.len();
            let display_limit = limit.unwrap_or(50);
            all_entries.truncate(display_limit);

            if json {
                println!("{}", serde_json::to_string_pretty(&all_entries)?);
            } else {
                println!(
                    "\n📜 Review History (last {} days)\n{}",
                    days,
                    "─".repeat(50)
                );
                println!("  Total matching entries: {}", total);

                if all_entries.is_empty() {
                    println!("\n  😴 No review history found.\n");
                    return Ok(());
                }

                // Group by state
                let mut by_state: HashMap<String, Vec<_>> = HashMap::new();
                for entry in &all_entries {
                    by_state.entry(entry.state.clone()).or_default().push(entry);
                }

                for (state_name, entries) in &by_state {
                    let icon = if state_name.contains("APPROVED") {
                        "✅"
                    } else if state_name.contains("CHANGES") {
                        "🔁"
                    } else {
                        "💬"
                    };
                    println!("\n  {} {} ({} PRs)", icon, state_name, entries.len());

                    for entry in entries.iter().take(10) {
                        if let Ok(date) = chrono::DateTime::parse_from_rfc3339(&entry.reviewed_at) {
                            let age = chrono::Utc::now() - date.with_timezone(&chrono::Utc);
                            let age_str = if age.num_days() > 0 {
                                format!("{}d ago", age.num_days())
                            } else if age.num_hours() > 0 {
                                format!("{}h ago", age.num_hours())
                            } else {
                                "just now".to_string()
                            };
                            println!(
                                "    #{}  {}  {}  ({})",
                                entry.pr_number,
                                entry.pr_title.chars().take(40).collect::<String>(),
                                entry.repo.dimmed(),
                                age_str
                            );
                        }
                    }
                    if entries.len() > 10 {
                        println!("    ... and {} more", entries.len() - 10);
                    }
                }

                println!("\n{}", "─".repeat(50));
                println!(
                    "  💡 Use `--json` for scripting | `--repo`, `--author`, `--state` to filter\n"
                );
            }
        }

        Commands::Ready {
            repo,
            author,
            priority,
            since_days,
            json,
            pr_number,
            pr,
            pr_numbers,
            all,
        } => {
            use std::collections::HashMap;

            // Priority: global --pr flag > local --pr > positional PR_NUMBER
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Handle batch PR numbers - fetch all specified PRs in parallel
            let prs_from_numbers: Vec<github::PendingReview> = if let Some(ref nums) = pr_numbers {
                let mut results = Vec::new();
                for part in nums.split(',') {
                    if let Ok(num) = part.trim().parse::<u64>() {
                        results.push(num);
                    }
                }
                if results.is_empty() {
                    println!("❌ No valid PR numbers provided.");
                    return Ok(());
                }
                // Fetch all specified PRs in parallel
                let fetch_futures = results.iter().map(|num| {
                    github::fetch_pr_by_number(
                        &ctx.cfg.github_token,
                        &ctx.cfg.github_org,
                        &ctx.cfg.github_repos,
                        *num,
                    )
                });
                join_all(fetch_futures)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok())
                    .flatten()
                    .collect()
            } else {
                Vec::new()
            };

            // Determine base PR list based on targeting mode
            let base_reviews: Vec<github::PendingReview> = if let Some(num) = target_pr {
                // Single PR via --pr or positional
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if pr_numbers.is_some() {
                // Batch mode via --pr-numbers
                prs_from_numbers
            } else if all {
                // --all flag: use all pending ctx.reviews
                ctx.reviews.clone()
            } else {
                // Interactive mode: use filtered ctx.reviews from main fetch
                ctx.reviews.clone()
            };

            // Apply --repo, --author, and --since-days filters to ctx.reviews
            let filtered_reviews: Vec<_> = {
                let mut result = base_reviews;

                // Apply --since-days filter (only show PRs created since N days ago)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            // Group ctx.reviews by repo for efficient API calls
            let mut by_repo: HashMap<String, Vec<&github::PendingReview>> = HashMap::new();
            for r in &filtered_reviews {
                by_repo.entry(r.repo.clone()).or_default().push(r);
            }

            #[derive(serde::Serialize)]
            struct ReadyPr {
                repo: String,
                pr_number: u64,
                pr_title: String,
                pr_author: String,
                pr_url: String,
                additions: u64,
                deletions: u64,
                age_days: i64,
                approved: bool,
                ci_status: String,
                has_conflicts: bool,
                draft: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                priority_score: Option<u8>,
            }

            // Build a flat list of (repo, pr_number) for parallel fetching
            let fetch_tasks: Vec<(String, u64)> = by_repo
                .iter()
                .flat_map(|(repo_name, repo_reviews)| {
                    repo_reviews
                        .iter()
                        .map(|r| (repo_name.clone(), r.pr_number))
                })
                .collect();

            // Build review lookup for matching results to ctx.reviews
            let mut review_lookup: HashMap<(String, u64), &github::PendingReview> = HashMap::new();
            for r in &filtered_reviews {
                review_lookup.insert((r.repo.clone(), r.pr_number), r);
            }

            // Phase 1: Fetch all PR details in parallel
            let github_username = ctx.cfg.github_username.clone();
            let futures = fetch_tasks.iter().map(move |(repo_name, pr_number)| {
                let client = octocrab::Octocrab::builder()
                    .personal_token(ctx.cfg.github_token.clone())
                    .build()
                    .expect("failed to build GitHub client");
                let org = ctx.cfg.github_org.clone();
                let repo_name = repo_name.clone();
                let pr_number = *pr_number;
                let github_username = github_username.clone();

                async move {
                    let pr = client.pulls(&org, &repo_name).get(pr_number).await?;

                    // Check approvals
                    let approved = pr
                        .requested_reviewers
                        .as_deref()
                        .map(|reviewers| reviewers.iter().any(|r| r.login == github_username))
                        .unwrap_or(false);

                    // Check CI status via combined status
                    #[derive(serde::Deserialize)]
                    struct CombinedStatus {
                        state: String,
                    }
                    let ci_state: String = client
                        .get(
                            format!(
                                "/repos/{}/{}/commits/{}/status",
                                org, repo_name, pr.head.sha
                            ),
                            None::<&str>,
                        )
                        .await
                        .map(|s: CombinedStatus| s.state)
                        .unwrap_or_else(|_| "unknown".to_string());

                    // Check for merge conflicts
                    let has_conflicts = pr.mergeable == Some(false);
                    let mergeable = pr.mergeable;

                    Ok::<(bool, String, bool, Option<bool>), anyhow::Error>((
                        approved,
                        ci_state,
                        has_conflicts,
                        mergeable,
                    ))
                }
            });

            #[allow(clippy::type_complexity)]
            let detail_results: Vec<
                Result<(bool, String, bool, Option<bool>), anyhow::Error>,
            > = join_all(futures).await;

            let mut ready_prs = Vec::new();

            for ((repo_name, pr_number), result) in
                fetch_tasks.iter().zip(detail_results)
            {
                let review = match review_lookup.get(&(repo_name.clone(), *pr_number)) {
                    Some(r) => r,
                    None => continue,
                };

                let (approved, ci_status, has_conflicts, mergeable) = match result {
                    Ok((a, c, h, m)) => (a, c, h, m),
                    Err(_) => (false, "unknown".to_string(), false, None),
                };

                let _is_ready = !review.draft
                    && (ci_status == "success" || ci_status == "pending")
                    && !has_conflicts
                    && mergeable != Some(false);

                let age_days = (chrono::Utc::now() - review.created_at).num_days();

                ready_prs.push(ReadyPr {
                    repo: review.repo.clone(),
                    pr_number: review.pr_number,
                    pr_title: review.pr_title.clone(),
                    pr_author: review.pr_author.clone(),
                    pr_url: review.pr_url.clone(),
                    additions: review.additions,
                    deletions: review.deletions,
                    age_days,
                    approved,
                    ci_status,
                    has_conflicts,
                    draft: review.draft,
                    priority_score: if priority {
                        Some(logger::calculate_priority_score(review))
                    } else {
                        None
                    },
                });
            }

            // Sort by readiness: ready first, then by age
            ready_prs.sort_by(|a, b| {
                let a_ready = !a.draft
                    && (a.ci_status == "success" || a.ci_status == "pending")
                    && !a.has_conflicts;
                let b_ready = !b.draft
                    && (b.ci_status == "success" || b.ci_status == "pending")
                    && !b.has_conflicts;
                match (a_ready, b_ready) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.age_days.cmp(&b.age_days),
                }
            });

            if json {
                println!("{}", serde_json::to_string_pretty(&ready_prs)?);
            } else {
                let ready_count = ready_prs
                    .iter()
                    .filter(|p| {
                        !p.draft
                            && (p.ci_status == "success" || p.ci_status == "pending")
                            && !p.has_conflicts
                    })
                    .count();

                println!(
                    "\n🚀 Merge Readiness — {} PRs total, {} ready to merge\n{}",
                    ready_prs.len(),
                    ready_count,
                    "─".repeat(50)
                );

                for pr in &ready_prs {
                    let is_ready = !pr.draft
                        && (pr.ci_status == "success" || pr.ci_status == "pending")
                        && !pr.has_conflicts;

                    let status_icon = if is_ready {
                        "✅".green()
                    } else if pr.draft {
                        "📝".yellow()
                    } else if pr.has_conflicts {
                        "⚠️  conflicts".red()
                    } else {
                        "⏳".normal()
                    };

                    let ci_icon: ColoredString = match pr.ci_status.as_str() {
                        "success" => "✅ CI".green(),
                        "failure" | "error" => "❌ CI".red(),
                        "pending" => "⏳ CI".yellow(),
                        _ => format!("? CI ({})", pr.ci_status).dimmed(),
                    };

                    let _total = pr.additions + pr.deletions;
                    let age_str: ColoredString = if pr.age_days == 0 {
                        "today".green()
                    } else if pr.age_days == 1 {
                        "1 day".normal()
                    } else if pr.age_days <= 7 {
                        format!("{} days", pr.age_days).yellow()
                    } else {
                        format!("{} days", pr.age_days).red()
                    };

                    let priority_display = if priority {
                        if let Some(score) = pr.priority_score {
                            format!("  ⭐ {}/5", logger::priority_stars(score))
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    println!(
                        "  {}  #{}  {}{}",
                        status_icon,
                        pr.pr_number,
                        pr.pr_title.bold(),
                        priority_display
                    );
                    println!(
                        "      👤 {}  •  📦 +{}/-{}  •  ⏱️ {}  •  {}",
                        pr.pr_author.cyan(),
                        pr.additions,
                        pr.deletions,
                        age_str,
                        ci_icon
                    );
                    println!(
                        "      📁 {}  🔗 {}",
                        pr.repo.dimmed(),
                        pr.pr_url.blue().underline()
                    );
                    println!();
                }

                println!("{}", "─".repeat(50));
                println!("  💡 Ready = not draft + CI passing + no conflicts");
                if priority {
                    println!("  💡 Priority based on age and size");
                }
                println!("  💡 Use `--json` for scripting\n");
            }
        }

        Commands::Blocked {
            repo,
            author,
            since_days,
            ci_only,
            conflicts_only,
            priority,
            limit,
            json,
        } => {
            let limit = limit.unwrap_or(20);

            #[derive(Clone, serde::Serialize)]
            struct BlockedPr {
                repo: String,
                pr_number: u64,
                pr_title: String,
                pr_author: String,
                pr_url: String,
                additions: u64,
                deletions: u64,
                age_days: i64,
                draft: bool,
                ci_status: String,
                has_conflicts: bool,
                mergeable: bool,
                blockers: Vec<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                priority_score: Option<u8>,
            }

            // Filter ctx.reviews by repo, author, and --since-days if specified
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --since-days filter first (consistent with other commands)
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                result
            };

            if filtered_reviews.is_empty() {
                if json {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!([]))?);
                } else {
                    println!("\n🚧 Blocked PRs — 0 total\n{}", "─".repeat(50));
                    println!("  🎉 No pending reviews found.");
                }
                return Ok(());
            }

            // Phase 1: Parallel fetch all PR details
            let pr_detail_futures = filtered_reviews.iter().map(|review| {
                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();
                let repo_name = review.repo.clone();
                let pr_number = review.pr_number;
                async move {
                    let client = octocrab::Octocrab::builder()
                        .personal_token(token)
                        .build()
                        .expect("failed to build GitHub client");
                    client.pulls(org, repo_name).get(pr_number).await
                }
            });
            let pr_results: Vec<Result<_, _>> = join_all(pr_detail_futures).await;

            // Phase 2: Parallel fetch CI status for each PR (only for successful PR fetches)
            #[derive(serde::Deserialize)]
            struct CombinedStatus {
                state: String,
            }
            let ci_futures = pr_results.iter().enumerate().filter_map(|(idx, r)| {
                let pr = r.as_ref().ok()?;
                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();
                let repo_name = pr.base.repo.clone().map(|r| r.name).unwrap_or_default();
                let sha = pr.head.sha.clone();
                Some(async move {
                    let client = octocrab::Octocrab::builder()
                        .personal_token(token)
                        .build()
                        .expect("failed to build GitHub client");
                    let state: String = client
                        .get(
                            format!("/repos/{}/{}/commits/{}/status", org, repo_name, sha),
                            None::<&str>,
                        )
                        .await
                        .map(|s: CombinedStatus| s.state)
                        .unwrap_or_else(|_| "unknown".to_string());
                    anyhow::Ok((idx, state))
                })
            });
            let ci_results: Vec<Result<(usize, String), _>> = join_all(ci_futures).await;

            // Build a map of idx -> ci_status for fast lookup
            let mut ci_map: std::collections::HashMap<usize, String> =
                std::collections::HashMap::new();
            for ci_result in ci_results.into_iter().flatten() {
                ci_map.insert(ci_result.0, ci_result.1);
            }

            // Build blocked_prs from parallel results
            let mut blocked_prs: Vec<BlockedPr> = Vec::new();

            for (idx, (review, pr_result)) in filtered_reviews
                .iter()
                .zip(pr_results)
                .enumerate()
            {
                let ci_status = ci_map
                    .get(&idx)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                let (has_conflicts, mergeable, blockers) = match pr_result {
                    Ok(pr) => {
                        let mut block_list = Vec::new();

                        if ci_status == "failure" || ci_status == "error" {
                            block_list.push("CI failing".to_string());
                        }

                        let conflicts = pr.mergeable == Some(false);
                        if conflicts {
                            block_list.push("Merge conflict".to_string());
                        }

                        let can_merge = pr.mergeable == Some(true);
                        if !can_merge && !conflicts {
                            block_list.push("Not mergeable".to_string());
                        }

                        if pr.draft.unwrap_or(false) {
                            block_list.push("Draft PR".to_string());
                        }

                        (conflicts, can_merge, block_list)
                    }
                    Err(_) => (false, false, vec!["Unable to fetch PR details".to_string()]),
                };

                let is_blocked = !blockers.is_empty();

                // Apply filters
                if ci_only && !blockers.iter().any(|b| b.contains("CI")) {
                    continue;
                }
                if conflicts_only && !blockers.iter().any(|b| b.contains("conflict")) {
                    continue;
                }

                if is_blocked {
                    let age_days = (chrono::Utc::now() - review.created_at).num_days();
                    blocked_prs.push(BlockedPr {
                        repo: review.repo.clone(),
                        pr_number: review.pr_number,
                        pr_title: review.pr_title.clone(),
                        pr_author: review.pr_author.clone(),
                        pr_url: review.pr_url.clone(),
                        additions: review.additions,
                        deletions: review.deletions,
                        age_days,
                        draft: review.draft,
                        ci_status,
                        has_conflicts,
                        mergeable,
                        blockers,
                        priority_score: if priority {
                            Some(logger::calculate_priority_score(review))
                        } else {
                            None
                        },
                    });
                }
            }

            // Sort by blockers count (most blocked first), then by priority/age
            blocked_prs.sort_by(|a, b| {
                let blk_cmp = b.blockers.len().cmp(&a.blockers.len());
                if blk_cmp == std::cmp::Ordering::Equal {
                    // When priority is enabled, sort by priority score; otherwise by age
                    match (a.priority_score, b.priority_score) {
                        (Some(score_a), Some(score_b)) => score_b.cmp(&score_a),
                        _ => b.age_days.cmp(&a.age_days),
                    }
                } else {
                    blk_cmp
                }
            });

            let shown_prs: Vec<_> = blocked_prs.iter().take(limit).cloned().collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&shown_prs)?);
            } else {
                println!(
                    "\n🚧 Blocked PRs — {} total\n{}",
                    blocked_prs.len(),
                    "─".repeat(50)
                );

                if shown_prs.is_empty() {
                    println!("  🎉 No blocked PRs found! All clear.");
                } else {
                    for pr in &shown_prs {
                        let total = pr.additions + pr.deletions;
                        let blocker_tags: String = pr
                            .blockers
                            .iter()
                            .map(|b: &String| {
                                if b.contains("CI") {
                                    "🔴 CI".red().to_string()
                                } else if b.contains("conflict") {
                                    "⚠️ Conflict".yellow().to_string()
                                } else if b.contains("Draft") {
                                    "📝 Draft".yellow().to_string()
                                } else {
                                    format!("❌ {}", b)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("  ");

                        println!(
                            "  🚫 #{} {}  ({})",
                            pr.pr_number,
                            pr.pr_title.bold(),
                            pr.repo.dimmed()
                        );
                        if priority {
                            if let Some(score) = pr.priority_score {
                                println!("     ⭐ {}/5  {}", score, logger::priority_stars(score));
                            }
                        }
                        println!(
                            "     👤 {}  •  📦 {} lines  •  ⏱️ {} days",
                            pr.pr_author.cyan(),
                            total,
                            pr.age_days
                        );
                        println!("     {}", blocker_tags);
                        println!("     🔗 {}", pr.pr_url.blue().underline());
                        println!();
                    }

                    if blocked_prs.len() > limit {
                        println!(
                            "  ...and {} more. Use `--limit 30` to see additional.",
                            blocked_prs.len() - limit
                        );
                    }
                }

                println!("{}", "─".repeat(50));
                println!("  💡 Use `--ci-only` to show only CI failures");
                println!("  💡 Use `--conflicts-only` to show only merge conflicts");
                println!("  💡 Use `--priority` to show priority scores");
                println!("  💡 Use `--author` to filter by author");
                println!("  💡 Use `--json` for scripting\n");
            }
        }

        Commands::Ping {
            emoji,
            pr_numbers,
            pr_number,
            pr,
            dry_run,
            all,
            send,
            since_days,
            repo,
            author,
            json,
            priority,
        } => {
            // Priority: global --pr flag > local --pr > local pr_number > pr_numbers > interactive
            let target_pr = ctx.cli.pr.or(pr).or(pr_number);

            // Apply --repo, --author, and --since-days filters (consistent with other commands)
            let filtered_reviews: Vec<_> = {
                let mut result = ctx.reviews.clone();

                // Apply --repo filter (partial match, case-insensitive)
                if let Some(ref repo_filter) = repo {
                    let pattern = repo_filter.to_lowercase();
                    result.retain(|r| r.repo.to_lowercase().contains(&pattern));
                }

                // Apply --author filter (partial match, case-insensitive)
                if let Some(ref author_filter) = author {
                    let pattern = author_filter.to_lowercase();
                    result.retain(|r| r.pr_author.to_lowercase().contains(&pattern));
                }

                // Apply --since-days filter
                if let Some(days) = since_days {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    result.retain(|r| r.created_at >= cutoff);
                }

                result
            };

            let targets: Vec<_> = if let Some(num) = target_pr {
                // When --pr is specified, fetch directly (bypasses filters)
                github::fetch_pr_by_number(
                    &ctx.cfg.github_token,
                    &ctx.cfg.github_org,
                    &ctx.cfg.github_repos,
                    num,
                )
                .await?
            } else if all {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                filtered_reviews
            } else if let Some(ref nums) = pr_numbers {
                let nums: Vec<u64> = nums
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                let nums_for_display = nums
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut matched = Vec::new();
                for num in nums {
                    if let Some(review) = filtered_reviews.iter().find(|r| r.pr_number == num) {
                        matched.push(review.clone());
                    }
                }
                if matched.is_empty() {
                    println!("No matching PRs found for: {}", nums_for_display);
                    return Ok(());
                }
                matched
            } else {
                if filtered_reviews.is_empty() {
                    println!("No matching reviews found.");
                    return Ok(());
                }
                // Skip interactive selection in dry-run mode
                if dry_run {
                    filtered_reviews
                } else {
                    logger::print_reviews(&filtered_reviews, false);
                    print!(
                        "\n{} ",
                        "Select PR(s) to ping [e.g. 1 or 1,3 or 1-3] (q to quit):".bold()
                    );
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    match parse_selection(input.trim(), filtered_reviews.len()) {
                        Selection::Quit => return Ok(()),
                        Selection::Indices(indices) => indices
                            .into_iter()
                            .map(|i| filtered_reviews[i].clone())
                            .collect(),
                    }
                }
            };

            // Build output (shared between JSON and text modes)
            #[derive(serde::Serialize)]
            struct PingOutput<'a> {
                repo: &'a str,
                pr_number: u64,
                pr_title: &'a str,
                pr_author: &'a str,
                pr_url: &'a str,
                age_days: i64,
                emoji: &'a str,
            }

            let outputs: Vec<PingOutput> = targets
                .iter()
                .map(|r| PingOutput {
                    repo: &r.repo,
                    pr_number: r.pr_number,
                    pr_title: &r.pr_title,
                    pr_author: &r.pr_author,
                    pr_url: &r.pr_url,
                    age_days: (chrono::Utc::now() - r.created_at).num_days(),
                    emoji: &emoji,
                })
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&outputs)?);
                return Ok(());
            }

            println!("\n👀 Ping Command");
            println!("{}", "─".repeat(50));
            println!("  Emoji: {}", emoji);
            println!();

            if send {
                // Parallel send: display preview then send all in parallel
                for review in &targets {
                    let age_days = (chrono::Utc::now() - review.created_at).num_days();
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score))
                    } else {
                        String::new()
                    };
                    println!(
                        "  📤 Sending #{} — {} by @{} ({} days old){}",
                        review.pr_number,
                        review.pr_title,
                        review.pr_author.cyan(),
                        if age_days == 0 {
                            "today".to_string()
                        } else {
                            format!("{} days", age_days)
                        },
                        priority_label
                    );
                }

                println!(
                    "\n⏳ Sending {} emoji reaction(s) in parallel...\n",
                    targets.len()
                );

                let token = ctx.cfg.github_token.clone();
                let org = ctx.cfg.github_org.clone();
                let emoji = emoji.clone();

                // Send all reactions in parallel
                let send_futures =
                    targets.iter().map(|review| {
                        let token = token.clone();
                        let org = org.clone();
                        let repo = review.repo.clone();
                        let pr_number = review.pr_number;
                        let emoji = emoji.clone();
                        async move {
                            github::add_pr_reaction(&token, &org, &repo, pr_number, &emoji).await
                        }
                    });
                let results = join_all(send_futures).await;

                let mut sent = 0;
                let mut failed = 0;

                for (review, result) in targets.iter().zip(results) {
                    match result {
                        Ok(_) => {
                            println!("  ✅ #{} — {}", review.pr_number, review.pr_title.dimmed());
                            sent += 1;
                        }
                        Err(e) => {
                            println!(
                                "  ❌ #{} — {} ({})",
                                review.pr_number,
                                review.pr_title.dimmed(),
                                e
                            );
                            failed += 1;
                        }
                    }
                }

                println!(
                    "\n📊 Sent: {}, Failed: {}\n",
                    sent.to_string().green(),
                    failed.to_string().red()
                );
            } else {
                // Preview mode (default when no --send)
                for review in &targets {
                    let age_days = (chrono::Utc::now() - review.created_at).num_days();
                    let priority_label = if priority {
                        let score = logger::calculate_priority_score(review);
                        format!(" {}", logger::priority_stars(score))
                    } else {
                        String::new()
                    };
                    if dry_run {
                        println!(
                            "  🔍 Dry-run: would ping #{} — {} by @{} ({} days old){}",
                            review.pr_number,
                            review.pr_title,
                            review.pr_author.cyan(),
                            if age_days == 0 {
                                "today".to_string()
                            } else {
                                format!("{} days", age_days)
                            },
                            priority_label
                        );
                    } else {
                        println!(
                            "  🔍 Will ping #{} — {} by @{} ({} days old){}",
                            review.pr_number,
                            review.pr_title,
                            review.pr_author.cyan(),
                            if age_days == 0 {
                                "today".to_string()
                            } else {
                                format!("{} days", age_days)
                            },
                            priority_label
                        );
                    }
                }

                println!();
                println!("{}", "─".repeat(50));
                if dry_run {
                    println!("  (dry-run — no emoji reactions sent)\n");
                } else {
                    println!("  💡 Use `--send` to actually send the emoji reactions");
                    println!("  💡 Available emojis: eyes (default), rocket, heart, +1, hooray");
                    println!("  💡 Use `-e rocket` or `-e heart` to change emoji\n");
                }
            }
        }

        Commands::Compare {
            pr1,
            pr2,
            detailed,
            priority,
            json,
        } => {
            // Parse PR identifiers (format: "repo#123" or just "123")
            fn parse_pr_id(s: &str, repos: &[String]) -> Option<(String, u64)> {
                if s.contains('#') {
                    let parts: Vec<&str> = s.split('#').collect();
                    if parts.len() == 2 {
                        let repo = parts[0].to_string();
                        let num = parts[1].parse::<u64>().ok()?;
                        return Some((repo, num));
                    }
                } else if let Ok(num) = s.parse::<u64>() {
                    // Use first repo if not specified
                    if let Some(repo) = repos.first() {
                        return Some((repo.clone(), num));
                    }
                }
                None
            }

            let (repo1, num1) = parse_pr_id(&pr1, &ctx.cfg.github_repos).ok_or_else(|| {
                anyhow::anyhow!("Invalid PR format: {}. Use 'repo#123' or '123'", pr1)
            })?;
            let (repo2, num2) = parse_pr_id(&pr2, &ctx.cfg.github_repos).ok_or_else(|| {
                anyhow::anyhow!("Invalid PR format: {}. Use 'repo#123' or '123'", pr2)
            })?;

            // Fetch both PRs
            let client = octocrab::Octocrab::builder()
                .personal_token(ctx.cfg.github_token.clone())
                .build()
                .expect("failed to build GitHub client");

            #[derive(Debug, Clone, serde::Serialize)]
            struct ComparedPr {
                repo: String,
                pr_number: u64,
                pr_title: String,
                author: String,
                age_days: i64,
                additions: u64,
                deletions: u64,
                total_lines: u64,
                draft: bool,
                files_count: usize,
                languages: std::collections::HashMap<String, u64>,
                priority_score: u8,
            }

            async fn fetch_pr_details(
                client: &octocrab::Octocrab,
                org: &str,
                repo: &str,
                pr_number: u64,
            ) -> anyhow::Result<ComparedPr> {
                let pr = client.pulls(org, repo).get(pr_number).await?;
                let age_days = (chrono::Utc::now()
                    - pr.created_at.unwrap_or_else(chrono::Utc::now))
                .num_days();
                let additions = pr.additions.unwrap_or(0);
                let deletions = pr.deletions.unwrap_or(0);
                let total_lines = additions + deletions;

                // Calculate priority score (1-5 stars)
                let priority_score = {
                    let age_score = if age_days <= 1 {
                        1
                    } else if age_days <= 3 {
                        2
                    } else if age_days <= 7 {
                        3
                    } else if age_days <= 14 {
                        4
                    } else {
                        5
                    };
                    let size_score = if total_lines <= 50 {
                        1
                    } else if total_lines <= 200 {
                        2
                    } else if total_lines <= 500 {
                        3
                    } else if total_lines <= 1000 {
                        4
                    } else {
                        5
                    };
                    ((age_score + size_score) / 2).clamp(1, 5) as u8
                };

                // Fetch files to get language breakdown
                let files = client
                    .pulls(org, repo)
                    .list_files(pr_number)
                    .await?
                    .into_iter()
                    .collect::<Vec<_>>();
                let files_count = files.len();
                let mut languages: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for f in &files {
                    let lang = f
                        .filename
                        .split('.')
                        .next_back()
                        .map(|ext| {
                            match ext {
                                "ts" | "tsx" => "TypeScript",
                                "js" | "jsx" | "mjs" => "JavaScript",
                                "py" => "Python",
                                "go" => "Go",
                                "java" => "Java",
                                "rs" => "Rust",
                                "rb" => "Ruby",
                                "cs" => "C#",
                                "cpp" | "cc" | "cxx" => "C++",
                                "c" | "h" => "C",
                                "swift" => "Swift",
                                "kt" | "kts" => "Kotlin",
                                "scala" => "Scala",
                                "php" => "PHP",
                                "ex" | "exs" => "Elixir",
                                "erl" => "Erlang",
                                "hs" => "Haskell",
                                "lua" => "Lua",
                                "sql" => "SQL",
                                "sh" | "bash" | "zsh" => "Shell",
                                "yml" | "yaml" => "YAML",
                                "json" => "JSON",
                                "md" => "Markdown",
                                "html" | "htm" => "HTML",
                                "css" | "scss" | "sass" => "CSS",
                                _ => "Other",
                            }
                            .to_string()
                        })
                        .unwrap_or_else(|| "Other".to_string());
                    *languages.entry(lang).or_insert(0) += 1;
                }

                Ok(ComparedPr {
                    repo: repo.to_string(),
                    pr_number,
                    pr_title: pr.title.unwrap_or_default(),
                    author: pr
                        .user
                        .as_ref()
                        .map(|u| u.login.clone())
                        .unwrap_or_default(),
                    age_days,
                    additions,
                    deletions,
                    total_lines,
                    draft: pr.draft.unwrap_or(false),
                    files_count,
                    languages,
                    priority_score,
                })
            }

            // Fetch both PRs in parallel using join_all
            let (pr_details_1, pr_details_2) = {
                let fetch_1 = fetch_pr_details(&client, &ctx.cfg.github_org, &repo1, num1);
                let fetch_2 = fetch_pr_details(&client, &ctx.cfg.github_org, &repo2, num2);
                let (result_1, result_2) = tokio::join!(fetch_1, fetch_2);
                (
                    result_1.map_err(|e| {
                        anyhow::anyhow!("Failed to fetch PR {}#{}: {}", repo1, num1, e)
                    })?,
                    result_2.map_err(|e| {
                        anyhow::anyhow!("Failed to fetch PR {}#{}: {}", repo2, num2, e)
                    })?,
                )
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "pr1": pr_details_1,
                        "pr2": pr_details_2,
                    }))?
                );
            } else {
                println!("\n⚖️  PR Comparison\n{}", "═".repeat(60));

                // Helper to print PR comparison row
                macro_rules! print_row {
                    ($label:expr, $val1:expr, $val2:expr) => {
                        let max_len = 40;
                        let val1_str = $val1.chars().take(max_len).collect::<String>();
                        let val2_str = $val2.chars().take(max_len).collect::<String>();
                        println!("  {:12}  {:<40}  {:<40}", $label, val1_str, val2_str);
                    };
                }

                // PR headers
                println!(
                    "\n  {:12}  {:<40}  {:<40}",
                    "",
                    format!(
                        "#{} {}",
                        num1,
                        &pr_details_1.pr_title[..pr_details_1.pr_title.len().min(35)]
                    ),
                    format!(
                        "#{} {}",
                        num2,
                        &pr_details_2.pr_title[..pr_details_2.pr_title.len().min(35)]
                    )
                );
                println!(
                    "  {:12}  {:<40}  {:<40}",
                    "",
                    pr_details_1.repo.bold(),
                    pr_details_2.repo.bold()
                );
                println!("{}", "─".repeat(60));

                print_row!(
                    "Author",
                    pr_details_1.author.cyan().to_string(),
                    pr_details_2.author.cyan().to_string()
                );
                print_row!(
                    "Age",
                    format!("{} days", pr_details_1.age_days),
                    format!("{} days", pr_details_2.age_days)
                );
                print_row!(
                    "Size",
                    format!("+{}/-{}", pr_details_1.additions, pr_details_1.deletions),
                    format!("+{}/-{}", pr_details_2.additions, pr_details_2.deletions)
                );
                print_row!(
                    "Files",
                    pr_details_1.files_count.to_string(),
                    pr_details_2.files_count.to_string()
                );
                print_row!(
                    "Draft",
                    if pr_details_1.draft { "Yes" } else { "No" }.to_string(),
                    if pr_details_2.draft { "Yes" } else { "No" }.to_string()
                );
                print_row!(
                    "Priority",
                    if priority {
                        format!(
                            "{}/5 {}",
                            pr_details_1.priority_score,
                            logger::priority_stars(pr_details_1.priority_score)
                        )
                    } else {
                        format!("{}/5", pr_details_1.priority_score)
                    },
                    if priority {
                        format!(
                            "{}/5 {}",
                            pr_details_2.priority_score,
                            logger::priority_stars(pr_details_2.priority_score)
                        )
                    } else {
                        format!("{}/5", pr_details_2.priority_score)
                    }
                );

                println!("{}", "─".repeat(60));

                // Winner indicators
                let winner = |label: &str, w1: bool, w2: bool| -> String {
                    match (w1, w2) {
                        (true, false) => format!("← {} wins", label),
                        (false, true) => format!("{} wins →", label),
                        _ => format!("{} tie", label),
                    }
                };

                let age_winner = pr_details_1.age_days < pr_details_2.age_days;
                let size_winner = pr_details_1.total_lines < pr_details_2.total_lines;
                let priority_winner = pr_details_1.priority_score > pr_details_2.priority_score;

                println!("  📊 Summary:");
                println!(
                    "    • Age: {} (newer)",
                    winner(
                        "age",
                        age_winner,
                        pr_details_2.age_days < pr_details_1.age_days
                    )
                );
                println!(
                    "    • Size: {} (smaller)",
                    winner(
                        "size",
                        size_winner,
                        pr_details_2.total_lines < pr_details_1.total_lines
                    )
                );
                println!(
                    "    • Priority: {} (higher score)",
                    winner(
                        "priority",
                        priority_winner,
                        pr_details_2.priority_score > pr_details_1.priority_score
                    )
                );

                if detailed {
                    println!("\n  💻 Languages:");
                    print!("    PR #{}: ", num1);
                    let mut langs: Vec<_> = pr_details_1.languages.iter().collect();
                    langs.sort_by(|a, b| b.1.cmp(a.1));
                    for (lang, count) in langs.iter().take(5) {
                        print!("{} ({}), ", lang, count);
                    }
                    println!();
                    print!("    PR #{}: ", num2);
                    let mut langs: Vec<_> = pr_details_2.languages.iter().collect();
                    langs.sort_by(|a, b| b.1.cmp(a.1));
                    for (lang, count) in langs.iter().take(5) {
                        print!("{} ({}), ", lang, count);
                    }
                    println!();
                }

                // URLs
                let url1 = format!("https://github.com/{}/pull/{}", ctx.cfg.github_org, repo1);
                let url2 = format!("https://github.com/{}/pull/{}", ctx.cfg.github_org, repo2);
                println!("\n  🔗 Links:");
                println!("    PR #{}: {}", num1, url1.blue().underline());
                println!("    PR #{}: {}", num2, url2.blue().underline());
                println!();
            }
        }

        Commands::Config { action } => match action {
            crate::cli::ConfigAction::Init { force } => {
                run_config_init(force)?;
            }
            crate::cli::ConfigAction::Show => {
                run_config_show()?;
            }
            crate::cli::ConfigAction::Update {
                token,
                username,
                org,
                repos,
                teams,
                ..
            } => {
                run_config_update(
                    token.as_deref(),
                    username.as_deref(),
                    org.as_deref(),
                    repos.as_deref(),
                    teams.as_deref(),
                )?;
            }
        },
        Commands::Stack {
            json,
            tree,
            repo,
            author,
            limit,
        } => {
            match stack::detect_stacks(
                &ctx.cfg.github_token,
                &ctx.cfg.github_org,
                &ctx.cfg.github_repos,
                author.as_deref(),
                repo.as_deref(),
                ctx.cli.include_drafts,
                limit,
            )
            .await
            {
                Ok(stacks) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&stacks)?);
                    } else if tree {
                        print!("{}", stack::render_stacks_tree(&stacks));
                    } else {
                        print!("{}", stack::render_stacks(&stacks));
                    }
                }
                Err(e) => {
                    eprintln!("Error detecting stacks: {}", e);
                }
            }
        }
    }

    // Open terminal tab last, after all files are written
    if ctx.cli.open_terminal {
        if let Some(ref dir) = ctx.output_dir {
            std::fs::create_dir_all(dir)?;
            terminal::open_terminal_at(dir)?;
            println!(
                "🖥️  Opened terminal at {}",
                dir.display().to_string().cyan()
            );
        }
    }

    Ok(())
}
