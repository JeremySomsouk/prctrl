//! Helper functions extracted from main.rs.

use crate::github;
use crate::logger;

use colored::*;
use std::collections::BTreeSet;

use std::io::{self, Write};
use std::path::PathBuf;

pub fn display_timeline(
    review: &github::PendingReview,
    timeline: &[github::TimelineEvent],
    json: bool,
    priority: bool,
    total_prs: usize,
    current_index: usize,
) -> Result<(), anyhow::Error> {
    let timeline_output = serde_json::json!({
        "pr_number": review.pr_number,
        "pr_title": review.pr_title,
        "repo": review.repo,
        "url": review.pr_url,
        "priority_score": if priority { Some(logger::calculate_priority_score(review)) } else { None },
        "events": timeline,
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&timeline_output)?);
    } else {
        // Add PR number prefix when showing multiple PRs
        let prefix = if total_prs > 1 {
            format!("[{} of {}] ", current_index + 1, total_prs)
        } else {
            String::new()
        };
        let priority_display = if priority {
            let score = logger::calculate_priority_score(review);
            format!("  {}", logger::priority_stars(score))
        } else {
            String::new()
        };
        println!("\n{}", "═".repeat(60));
        println!(
            "{}📜 PR #{} — {} Timeline{}",
            prefix,
            review.pr_number,
            review.pr_title.bold(),
            priority_display
        );
        println!("{}", "═".repeat(60));
        println!();

        if timeline.is_empty() {
            println!("  No timeline events found.");
        } else {
            // Group events by type for summary
            let mut review_count = 0;
            let mut comment_count = 0;
            let mut label_events = 0;
            let mut other_events = 0;

            for event in timeline {
                match event.event.as_str() {
                    "PullRequestReview" => review_count += 1,
                    "Comment" | "IssueComment" => comment_count += 1,
                    "labeled" | "unlabeled" => label_events += 1,
                    _ => other_events += 1,
                }
            }

            let other_label = if other_events > 0 {
                format!(", {} other events", other_events.to_string().magenta())
            } else {
                String::new()
            };
            println!(
                "  📊 Summary: {} reviews, {} comments, {} label changes{}",
                review_count.to_string().green(),
                comment_count.to_string().cyan(),
                label_events.to_string().yellow(),
                other_label
            );
            println!();
            println!("{}", "─".repeat(60));

            // Show chronological timeline
            for event in timeline {
                let time_str = event.created_at.format("%Y-%m-%d %H:%M").to_string();
                let actor_str = event.actor.as_deref().unwrap_or("unknown");

                let (icon, desc) = match event.event.as_str() {
                    "PullRequestReview" => {
                        let state = event
                            .data
                            .get("review_state")
                            .and_then(|s| s.as_str())
                            .unwrap_or("COMMENTED");
                        let state_icon: &str = match state {
                            "APPROVED" => "✅",
                            "CHANGES_REQUESTED" => "🔁",
                            _ => "💬",
                        };
                        let preview = event
                            .data
                            .get("body_preview")
                            .and_then(|b| b.as_str())
                            .map(|s| format!(": \"{}\"", s.chars().take(60).collect::<String>()))
                            .unwrap_or_default();
                        (
                            state_icon.to_string(),
                            format!("{} by @{} review{}", state, actor_str.cyan(), preview),
                        )
                    }
                    "Comment" | "IssueComment" => {
                        let preview = event
                            .data
                            .get("body_preview")
                            .and_then(|b| b.as_str())
                            .map(|s| format!(": \"{}\"", s.chars().take(80).collect::<String>()))
                            .unwrap_or_default();
                        (
                            "💬".to_string(),
                            format!("Comment by @{}{}", actor_str.cyan(), preview),
                        )
                    }
                    "labeled" => {
                        let label = event
                            .data
                            .get("label")
                            .and_then(|l| l.as_str())
                            .unwrap_or("unknown");
                        (
                            "🏷️".to_string(),
                            format!("Labeled with *{}* by @{}", label, actor_str.cyan()),
                        )
                    }
                    "unlabeled" => {
                        let label = event
                            .data
                            .get("label")
                            .and_then(|l| l.as_str())
                            .unwrap_or("unknown");
                        (
                            "🏷️".to_string(),
                            format!("Unlabeled *{}* by @{}", label, actor_str.cyan()),
                        )
                    }
                    "assigned" => {
                        let assignee = event
                            .data
                            .get("assignee")
                            .and_then(|a| a.as_str())
                            .unwrap_or("unknown");
                        (
                            "👤".to_string(),
                            format!("Assigned to @{}", assignee.cyan()),
                        )
                    }
                    "unassigned" => {
                        let assignee = event
                            .data
                            .get("assignee")
                            .and_then(|a| a.as_str())
                            .unwrap_or("unknown");
                        ("👤".to_string(), format!("Unassigned @{}", assignee.cyan()))
                    }
                    "merged" => (
                        "🔀".to_string(),
                        format!("PR merged by @{}", actor_str.cyan()),
                    ),
                    "closed" => {
                        let merged = event
                            .data
                            .get("merged")
                            .and_then(|m| m.as_bool())
                            .unwrap_or(false);
                        if merged {
                            (
                                "✅".to_string(),
                                format!("PR closed and merged by @{}", actor_str.cyan()),
                            )
                        } else {
                            (
                                "❌".to_string(),
                                format!("PR closed without merging by @{}", actor_str.cyan()),
                            )
                        }
                    }
                    "reopened" => (
                        "🔄".to_string(),
                        format!("PR reopened by @{}", actor_str.cyan()),
                    ),
                    "referenced" => (
                        "🔗".to_string(),
                        format!("Referenced from commit by @{}", actor_str.cyan()),
                    ),
                    "head_ref_force_pushed" => (
                        "⚡".to_string(),
                        format!("Head branch force-pushed by @{}", actor_str.cyan()),
                    ),
                    "head_ref_deleted" => (
                        "🗑️".to_string(),
                        format!("Head branch deleted by @{}", actor_str.cyan()),
                    ),
                    "ready_for_review" => (
                        "📣".to_string(),
                        format!("PR marked as ready for review by @{}", actor_str.cyan()),
                    ),
                    "converted_to_draft" => (
                        "📝".to_string(),
                        format!("PR converted to draft by @{}", actor_str.cyan()),
                    ),
                    "locked" => (
                        "🔒".to_string(),
                        format!("PR locked by @{}", actor_str.cyan()),
                    ),
                    "unlocked" => (
                        "🔓".to_string(),
                        format!("PR unlocked by @{}", actor_str.cyan()),
                    ),
                    "pinned" => (
                        "📌".to_string(),
                        format!("PR pinned by @{}", actor_str.cyan()),
                    ),
                    "unpinned" => (
                        "📍".to_string(),
                        format!("PR unpinned by @{}", actor_str.cyan()),
                    ),
                    "subscribed" | "unsubscribed" => (
                        "🔔".to_string(),
                        format!("@{} {}", actor_str.cyan(), event.event.replace("_", " ")),
                    ),
                    "mentioned" | "team_mentioned" => (
                        "@".to_string(),
                        format!(
                            "{} mentioned by @{}",
                            event.event.replace("_", " "),
                            actor_str.cyan()
                        ),
                    ),
                    _ => (
                        "📌".to_string(),
                        format!("{} by @{}", event.event.replace("_", " "), actor_str.cyan()),
                    ),
                };

                println!();
                println!("  {}  {}  {}", time_str.dimmed(), icon, desc);
            }
        }

        println!();
        println!("{}", "─".repeat(60));
        println!("  🔗 {}", review.pr_url.blue().underline());
        println!("{}", "═".repeat(60));
        println!();
    }

    Ok(())
}

pub fn format_duration(d: chrono::Duration) -> String {
    let total_hours = d.num_hours();
    if total_hours < 24 {
        format!("{}h", total_hours)
    } else {
        let days = d.num_days();
        if days < 7 {
            format!("{}d", days)
        } else {
            let weeks = days / 7;
            format!("{}w", weeks)
        }
    }
}

pub fn colorize_label(name: &str, color: &str) -> colored::ColoredString {
    // Parse hex color (GitHub label colors are 6-char hex without #)
    if color.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&color[0..2], 16),
            u8::from_str_radix(&color[2..4], 16),
            u8::from_str_radix(&color[4..6], 16),
        ) {
            // Use the hex color for the text
            let label_with_bg = format!(" {} ", name);
            return label_with_bg.color(colored::Color::TrueColor { r, g, b });
        }
    }
    // Fallback: return name with cyan color
    name.cyan()
}

pub enum Selection {
    Quit,
    Indices(Vec<usize>),
}

pub fn parse_selection(input: &str, total: usize) -> Selection {
    let input = input.trim().to_lowercase();

    if input == "q" || input == "quit" {
        return Selection::Quit;
    }

    if input == "all" {
        return Selection::Indices((0..total).collect());
    }

    let mut indices = BTreeSet::new();
    let mut ignored: Vec<String> = Vec::new();

    for part in input.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            match (start.trim().parse::<usize>(), end.trim().parse::<usize>()) {
                (Ok(s), Ok(e)) if s >= 1 && e >= s && e <= total => {
                    for i in s..=e {
                        indices.insert(i - 1);
                    }
                }
                _ => ignored.push(part.to_string()),
            }
        } else if let Ok(n) = part.parse::<usize>() {
            if n >= 1 && n <= total {
                indices.insert(n - 1);
            } else {
                ignored.push(part.to_string());
            }
        } else {
            ignored.push(part.to_string());
        }
    }

    // Give the user explicit feedback instead of silently dropping bad tokens.
    if !ignored.is_empty() {
        eprintln!(
            "⚠️  Ignored invalid selection(s): {} (valid range: 1-{})",
            ignored.join(", "),
            total
        );
    }

    Selection::Indices(indices.into_iter().collect())
}

// =============================================================================
// Config Management Functions
// =============================================================================

pub fn get_config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("prctrl")
        .join("config.toml")
}

pub fn run_config_init(force: bool) -> anyhow::Result<()> {
    let config_path = get_config_path();

    if config_path.exists() && !force {
        println!("\n⚠️  Config already exists at {:?}", config_path);
        println!("   Use --force to overwrite");
        return Ok(());
    }

    println!("\n🚀 PRCtrl Configuration Setup\n");
    println!("This will create a config file at:");
    println!("  {:?}\n", config_path);
    println!("Let's get some info:\n");

    let mut github_token = String::new();
    let mut github_username = String::new();
    let mut github_org = String::new();
    let mut github_repos = String::new();
    let mut github_teams = String::new();

    print!("📦 GitHub Personal Access Token (ghp_xxx): ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut github_token)?;
    github_token = github_token.trim().to_string();

    print!("👤 GitHub Username: ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut github_username)?;
    github_username = github_username.trim().to_string();

    print!("🏢 GitHub Organization: ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut github_org)?;
    github_org = github_org.trim().to_string();

    print!("📚 Repos to monitor (comma-separated): ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut github_repos)?;
    github_repos = github_repos.trim().to_string();

    print!("👥 GitHub Teams (optional, comma-separated): ");
    io::stdout().flush()?;
    io::stdin().read_line(&mut github_teams)?;
    github_teams = github_teams.trim().to_string();

    let config_content = format!(
        r#"# PRCtrl Configuration
# Generated by `prctrl config init`

[github]
token = "{github_token}"
username = "{github_username}"
org = "{github_org}"
repos = [{repos}]
teams = [{teams}]

[notifications]
enabled = true
interval = 300

[defaults]
include_drafts = false
exclude_prefix = ["chore(deps)"]
"#,
        repos = github_repos
            .split(',')
            .map(|s| format!("\"{}\"", s.trim()))
            .collect::<Vec<_>>()
            .join(", "),
        teams = if github_teams.is_empty() {
            "[]".to_string()
        } else {
            github_teams
                .split(',')
                .map(|s| format!("\"{}\"", s.trim().to_lowercase()))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_path, config_content)?;

    println!("\n✅ Config saved to {:?}", config_path);
    println!("\nNext steps:");
    println!("  1. Run `prctrl list` to test your config");
    println!("  2. Or set environment variables - see README");
    println!();

    Ok(())
}

pub fn run_config_show() -> anyhow::Result<()> {
    let config_path = get_config_path();

    if !config_path.exists() {
        println!("\n⚠️  No config file found at {:?}", config_path);
        println!("   Run `prctrl config init` to create one\n");
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)?;
    let masked = mask_token_in_config(&content);
    println!("\n📄 Current Configuration\n");
    println!("{}", masked);

    Ok(())
}

/// Mask sensitive values (tokens, API keys) in config output.
/// Shows first 4 and last 4 characters with **** in between.
pub fn mask_token_in_config(content: &str) -> String {
    let sensitive_keys = ["token", "api_key", "anthropic_api_key"];
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    for line in lines.iter_mut() {
        if let Some((key, value)) = line.split_once('=') {
            let key_trimmed = key.trim();
            if sensitive_keys.contains(&key_trimmed) {
                // Extract the quoted value
                let val = value.trim().trim_matches('"').trim();
                if val.len() > 8 {
                    let masked_val = format!("{}****{}", &val[..4], &val[val.len() - 4..]);
                    *line = line.replace(val, &masked_val);
                }
            }
        }
    }
    lines.join("\n")
}

pub fn run_config_update(
    token: Option<&str>,
    username: Option<&str>,
    org: Option<&str>,
    repos: Option<&str>,
    teams: Option<&str>,
) -> anyhow::Result<()> {
    let config_path = get_config_path();

    if !config_path.exists() {
        println!("\n⚠️  No config file found.");
        println!("   Run `prctrl config init` first.\n");
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)?;
    let mut config: toml::Table = toml::from_str(&content).unwrap_or_else(|_| {
        println!("⚠️  Could not parse existing config. Creating new structure.");
        toml::Table::new()
    });

    if let Some(token) = token {
        config
            .entry("github".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(github) = config.get_mut("github").and_then(|v| v.as_table_mut()) {
            github.insert("token".to_string(), toml::Value::String(token.to_string()));
        }
    }

    if let Some(username) = username {
        config
            .entry("github".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(github) = config.get_mut("github").and_then(|v| v.as_table_mut()) {
            github.insert(
                "username".to_string(),
                toml::Value::String(username.to_string()),
            );
        }
    }

    if let Some(org) = org {
        config
            .entry("github".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(github) = config.get_mut("github").and_then(|v| v.as_table_mut()) {
            github.insert("org".to_string(), toml::Value::String(org.to_string()));
        }
    }

    if let Some(repos) = repos {
        let repos_array: toml::Value = repos
            .split(',')
            .map(|s| toml::Value::String(s.trim().to_string()))
            .collect::<Vec<_>>()
            .into();
        config
            .entry("github".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(github) = config.get_mut("github").and_then(|v| v.as_table_mut()) {
            github.insert("repos".to_string(), repos_array);
        }
    }

    if let Some(teams) = teams {
        let teams_array: toml::Value = teams
            .split(',')
            .map(|s| toml::Value::String(s.trim().to_lowercase()))
            .collect::<Vec<_>>()
            .into();
        config
            .entry("github".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(github) = config.get_mut("github").and_then(|v| v.as_table_mut()) {
            github.insert("teams".to_string(), teams_array);
        }
    }

    std::fs::write(&config_path, toml::to_string_pretty(&config)?)?;

    println!("\n✅ Config updated at {:?}\n", config_path);

    Ok(())
}

/// Reads snoozed PRs from .snoozed.json and returns Vec<(repo, pr_number)> for PRs still in snooze period.
pub fn read_snoozed_prs(snooze_file: &PathBuf) -> Vec<(String, u64)> {
    if !snooze_file.exists() {
        return Vec::new();
    }
    let content = match std::fs::read_to_string(snooze_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let entries = match serde_json::from_str::<Vec<serde_json::Value>>(&content) {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "Warning: Failed to parse snooze file {:?}: {}",
                snooze_file, err
            );
            return Vec::new();
        }
    };
    let now = chrono::Utc::now();
    entries
        .into_iter()
        .filter_map(|e| {
            let repo = e.get("repo")?.as_str()?.to_string();
            let pr_number = e.get("pr_number")?.as_u64()?;
            let until_str = e.get("snoozed_until")?.as_str()?;
            let until = chrono::DateTime::parse_from_rfc3339(until_str).ok()?;
            if until.with_timezone(&chrono::Utc) > now {
                Some((repo, pr_number))
            } else {
                None
            }
        })
        .collect()
}
