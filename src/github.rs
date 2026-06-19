use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use octocrab::Octocrab;
use serde::Serialize;

/// Create a shared Octocrab client.
///
/// All functions should use this instead of building their own client.
/// A single client reuses the underlying HTTP connection pool (keep-alive,
/// TLS session) across all API calls, avoiding repeated handshakes.
pub fn new_client(token: &str) -> Octocrab {
    Octocrab::builder()
        .personal_token(token.to_string())
        .build()
        .expect("failed to build GitHub client")
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingReview {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub pr_author: String,
    pub pr_url: String,
    pub created_at: DateTime<Utc>,
    pub additions: u64,
    pub deletions: u64,
    pub draft: bool,
    pub branch: String,
}

pub async fn fetch_pr_by_number(
    token: &str,
    org: &str,
    repos: &[String],
    pr_number: u64,
) -> Result<Vec<PendingReview>> {
    let client = new_client(token);

    // Parallel fetch from all repos using join_all
    let futures = repos.iter().map(|repo| {
        let client = &client;
        let repo_name = repo.clone();
        async move { (repo_name, client.pulls(org, repo).get(pr_number).await) }
    });

    let results_vec: Vec<(String, Result<_, _>)> = join_all(futures).await;

    let mut results = vec![];
    for (repo_name, result) in results_vec {
        match result {
            Ok(pr) => {
                let author = pr
                    .user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .unwrap_or_default();
                results.push(PendingReview {
                    repo: repo_name,
                    pr_number: pr.number,
                    pr_title: pr.title.clone().unwrap_or_default(),
                    pr_author: author,
                    pr_url: pr
                        .html_url
                        .as_ref()
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    created_at: pr.created_at.unwrap_or_default(),
                    additions: pr.additions.unwrap_or(0),
                    deletions: pr.deletions.unwrap_or(0),
                    draft: pr.draft.unwrap_or(false),
                    branch: pr.head.label.clone().unwrap_or_default(),
                });
            }
            Err(_) => continue,
        }
    }

    Ok(results)
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_pending_reviews(
    token: &str,
    org: &str,
    repos: &[String],
    username: &str,
    teams: &[String],
    include_mine: bool,
    include_drafts: bool,
    exclude_prefixes: &[String],
    crew_members: &[String],
    max_age_days: Option<u32>,
) -> Result<Vec<PendingReview>> {
    // Collect candidate PRs across all repos with pagination
    #[derive(Clone)]
    struct CandidatePr {
        repo: String,
        number: u64,
        title: String,
        author: String,
        url: String,
        created_at: DateTime<Utc>,
        draft: bool,
        branch: String,
        additions: Option<u64>,
        deletions: Option<u64>,
    }

    let mut candidates: Vec<CandidatePr> = Vec::new();

    // Parallel fetch PR lists from all repos with full pagination
    let repo_futures = repos.iter().map(|repo| {
        let client = new_client(token);
        let repo = repo.clone();
        async move {
            let mut all_prs = Vec::new();
            let first_page = client
                .pulls(org, &repo)
                .list()
                .state(octocrab::params::State::Open)
                .per_page(100)
                .send()
                .await?;

            all_prs.extend(first_page.items);

            let mut next_page = first_page.next;
            while next_page.is_some() {
                match client.get_page(&next_page).await {
                    Ok(Some(page)) => {
                        next_page = page.next.clone();
                        all_prs.extend(page.items);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }

            Ok::<(String, Vec<_>), octocrab::Error>((repo, all_prs))
        }
    });

    let repo_results: Vec<(String, Vec<_>)> = join_all(repo_futures)
        .await
        .into_iter()
        .filter_map(|result| match result {
            Ok((repo, items)) => Some((repo, items)),
            Err(e) => {
                eprintln!("Warning: Failed to fetch PRs from a repo: {}", e);
                None
            }
        })
        .collect();

    for (repo, prs) in repo_results {
        for pr in prs {
            if !include_drafts && pr.draft.unwrap_or(false) {
                continue;
            }

            let title = pr.title.clone().unwrap_or_default();
            if exclude_prefixes
                .iter()
                .any(|p| !p.is_empty() && title.starts_with(p))
            {
                continue;
            }

            let author = pr.user.as_ref().map(|u| u.login.as_str()).unwrap_or("");

            // Filter by max age if configured
            if let Some(max_age) = max_age_days {
                let pr_created = match pr.created_at {
                    Some(created) => created,
                    None => {
                        // If PR has no creation date, conservatively filter it out
                        // when age filtering is enabled
                        continue;
                    }
                };
                let duration = chrono::Utc::now() - pr_created;
                if duration.num_days() > max_age as i64 {
                    continue;
                }
            }

            // --crew mode: only show PRs authored by crew members
            if !crew_members.is_empty() {
                if !crew_members.iter().any(|m| m == author) {
                    continue;
                }
            } else {
                if !include_mine && author == username {
                    continue;
                }

                let user_requested = pr
                    .requested_reviewers
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .any(|r| r.login == username);

                let team_requested = if teams.is_empty() {
                    false
                } else {
                    pr.requested_teams
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .any(|t| teams.contains(&t.slug.to_lowercase()))
                };

                if !user_requested && !team_requested {
                    continue;
                }
            }

            candidates.push(CandidatePr {
                repo: repo.clone(),
                number: pr.number,
                title,
                author: author.to_string(),
                url: pr
                    .html_url
                    .as_ref()
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
                created_at: pr.created_at.unwrap_or_default(),
                draft: pr.draft.unwrap_or(false),
                branch: pr.head.label.clone().unwrap_or_default(),
                additions: pr.additions,
                deletions: pr.deletions,
            });
        }
    }

    // Build pending reviews — fetch details only for PRs missing additions/deletions
    let (needs_detail, _has_data): (Vec<_>, Vec<_>) = candidates
        .iter()
        .partition::<Vec<_>, _>(|c| c.additions.is_none() || c.deletions.is_none());

    let detail_futures = needs_detail.iter().map(|c| {
        let client = new_client(token);
        let repo = c.repo.clone();
        let number = c.number;
        async move { client.pulls(org, &repo).get(number).await }
    });

    let details: Vec<Result<_, _>> = join_all(detail_futures).await;

    let detail_map: std::collections::HashMap<(String, u64), (u64, u64)> = needs_detail
        .into_iter()
        .zip(details)
        .filter_map(|(c, result)| match result {
            Ok(detail) => Some((
                (c.repo.clone(), c.number),
                (detail.additions.unwrap_or(0), detail.deletions.unwrap_or(0)),
            )),
            Err(e) => {
                eprintln!(
                    "Warning: Failed to fetch details for PR #{} in {}: {}",
                    c.number, c.repo, e
                );
                None
            }
        })
        .collect();

    let mut pending: Vec<PendingReview> = Vec::new();
    for candidate in candidates {
        let (additions, deletions) = detail_map
            .get(&(candidate.repo.clone(), candidate.number))
            .copied()
            .unwrap_or_else(|| {
                (
                    candidate.additions.unwrap_or(0),
                    candidate.deletions.unwrap_or(0),
                )
            });
        pending.push(PendingReview {
            repo: candidate.repo,
            pr_number: candidate.number,
            pr_title: candidate.title,
            pr_author: candidate.author,
            pr_url: candidate.url,
            created_at: candidate.created_at,
            additions,
            deletions,
            draft: candidate.draft,
            branch: candidate.branch,
        });
    }

    // Sort: oldest first (most urgent)
    pending.sort_by_key(|r| r.created_at);
    Ok(pending)
}

pub async fn fetch_my_open_prs(
    token: &str,
    org: &str,
    repos: &[String],
    username: &str,
    include_drafts: bool,
    exclude_prefixes: &[String],
    max_age_days: Option<u32>,
) -> Result<Vec<PendingReview>> {
    // Collect candidate PRs across all repos with full pagination
    #[derive(Clone)]
    struct CandidatePr {
        repo: String,
        number: u64,
        title: String,
        author: String,
        url: String,
        created_at: DateTime<Utc>,
        draft: bool,
        branch: String,
        additions: Option<u64>,
        deletions: Option<u64>,
    }

    let mut candidates: Vec<CandidatePr> = Vec::new();

    // Parallel fetch PR lists from all repos with pagination
    let repo_futures = repos.iter().map(|repo| {
        let client = new_client(token);
        let repo = repo.clone();
        async move {
            let mut all_prs = Vec::new();
            let first_page = client
                .pulls(org, &repo)
                .list()
                .state(octocrab::params::State::Open)
                .per_page(100)
                .send()
                .await?;

            all_prs.extend(first_page.items);

            let mut next_page = first_page.next;
            while next_page.is_some() {
                match client.get_page(&next_page).await {
                    Ok(Some(page)) => {
                        next_page = page.next.clone();
                        all_prs.extend(page.items);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }

            Ok::<(String, Vec<_>), octocrab::Error>((repo, all_prs))
        }
    });

    let repo_results: Vec<(String, Vec<_>)> = join_all(repo_futures)
        .await
        .into_iter()
        .filter_map(|result| match result {
            Ok((repo, items)) => Some((repo, items)),
            Err(e) => {
                eprintln!("Warning: Failed to fetch PRs from a repo: {}", e);
                None
            }
        })
        .collect();

    for (repo, prs) in repo_results {
        for pr in prs {
            if !include_drafts && pr.draft.unwrap_or(false) {
                continue;
            }

            let title = pr.title.clone().unwrap_or_default();
            if exclude_prefixes
                .iter()
                .any(|p| !p.is_empty() && title.starts_with(p))
            {
                continue;
            }

            let author = pr.user.as_ref().map(|u| u.login.as_str()).unwrap_or("");

            // Filter by max age if configured
            if let Some(max_age) = max_age_days {
                let pr_created = match pr.created_at {
                    Some(created) => created,
                    None => {
                        // If PR has no creation date, conservatively filter it out
                        // when age filtering is enabled
                        continue;
                    }
                };
                let duration = chrono::Utc::now() - pr_created;
                if duration.num_days() > max_age as i64 {
                    continue;
                }
            }

            if author != username {
                continue;
            }

            candidates.push(CandidatePr {
                repo: repo.clone(),
                number: pr.number,
                title,
                author: author.to_string(),
                url: pr
                    .html_url
                    .as_ref()
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
                created_at: pr.created_at.unwrap_or_default(),
                draft: pr.draft.unwrap_or(false),
                branch: pr.head.label.clone().unwrap_or_default(),
                additions: pr.additions,
                deletions: pr.deletions,
            });
        }
    }

    // Build results — fetch details only for PRs missing additions/deletions
    let (needs_detail, _has_data): (Vec<_>, Vec<_>) = candidates
        .iter()
        .partition::<Vec<_>, _>(|c| c.additions.is_none() || c.deletions.is_none());

    let detail_futures = needs_detail.iter().map(|c| {
        let client = new_client(token);
        let repo = c.repo.clone();
        let number = c.number;
        async move { client.pulls(org, &repo).get(number).await }
    });

    let details: Vec<Result<_, _>> = join_all(detail_futures).await;

    let detail_map: std::collections::HashMap<(String, u64), (u64, u64)> = needs_detail
        .into_iter()
        .zip(details)
        .filter_map(|(c, result)| match result {
            Ok(detail) => Some((
                (c.repo.clone(), c.number),
                (detail.additions.unwrap_or(0), detail.deletions.unwrap_or(0)),
            )),
            Err(e) => {
                eprintln!(
                    "Warning: Failed to fetch details for PR #{} in {}: {}",
                    c.number, c.repo, e
                );
                None
            }
        })
        .collect();

    let mut my_prs: Vec<PendingReview> = Vec::new();
    for candidate in candidates {
        let (additions, deletions) = detail_map
            .get(&(candidate.repo.clone(), candidate.number))
            .copied()
            .unwrap_or_else(|| {
                (
                    candidate.additions.unwrap_or(0),
                    candidate.deletions.unwrap_or(0),
                )
            });
        my_prs.push(PendingReview {
            repo: candidate.repo,
            pr_number: candidate.number,
            pr_title: candidate.title,
            pr_author: candidate.author,
            pr_url: candidate.url,
            created_at: candidate.created_at,
            additions,
            deletions,
            draft: candidate.draft,
            branch: candidate.branch,
        });
    }

    // Sort: oldest first (most urgent)
    my_prs.sort_by_key(|r| r.created_at);
    Ok(my_prs)
}

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequestFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

pub async fn fetch_pr_files(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PullRequestFile>> {
    let client = new_client(token);

    let files: Vec<PullRequestFile> = client
        .pulls(org, repo)
        .list_files(pr_number)
        .await?
        .into_iter()
        .map(|f| {
            let status_str = match f.status {
                octocrab::models::repos::DiffEntryStatus::Added => "added",
                octocrab::models::repos::DiffEntryStatus::Removed => "removed",
                octocrab::models::repos::DiffEntryStatus::Modified => "modified",
                octocrab::models::repos::DiffEntryStatus::Renamed => "renamed",
                octocrab::models::repos::DiffEntryStatus::Copied => "copied",
                octocrab::models::repos::DiffEntryStatus::Changed => "changed",
                octocrab::models::repos::DiffEntryStatus::Unchanged => "unchanged",
                _ => "unknown",
            };
            PullRequestFile {
                filename: f.filename,
                status: status_str.to_string(),
                additions: f.additions,
                deletions: f.deletions,
            }
        })
        .collect();

    Ok(files)
}

#[derive(Debug, Clone, Serialize)]
pub struct PullRequestLabel {
    pub id: String,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

pub async fn fetch_pr_labels(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PullRequestLabel>> {
    let client = new_client(token);

    let labels: Vec<PullRequestLabel> = client
        .pulls(org, repo)
        .get(pr_number)
        .await?
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| PullRequestLabel {
            id: l.id.to_string(),
            name: l.name,
            color: l.color,
            description: l.description,
        })
        .collect();

    Ok(labels)
}

/// Represents a single file's diff with metadata.
#[derive(Debug, Clone)]
pub struct PullRequestDiff {
    pub filename: String,
    pub status: String, // "added", "removed", "modified", "renamed"
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    pub language: Option<String>,
}

/// Represents merge conflict status for a PR
#[derive(Debug, Clone, Serialize)]
pub struct MergeConflictStatus {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub has_conflicts: bool,
    pub mergeable: Option<bool>,
    pub rebaseable: Option<bool>,
}

/// Represents CI check run status for a commit
#[derive(Debug, Clone, Serialize)]
pub struct CiStatus {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub head_sha: String,
    pub overall_status: String, // "success", "failure", "pending", "error", "cancelled"
    pub checks: Vec<CiCheck>,
}

/// Individual CI check within a commit
#[derive(Debug, Clone, Serialize)]
pub struct CiCheck {
    pub name: String,
    pub status: String, // "completed", "in_progress", "queued", "pending", "waiting", "requested"
    pub conclusion: Option<String>, // "success", "failure", "neutral", "cancelled", "skipped", "timed_out", "action_required"
    pub app_name: String,           // "GitHub Actions", "CircleCI", "Jenkins", etc.
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

pub async fn fetch_pr_diff(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PullRequestDiff>> {
    let client = new_client(token);

    let files: Vec<PullRequestDiff> = client
        .pulls(org, repo)
        .list_files(pr_number)
        .await?
        .into_iter()
        .map(|f| {
            let status_str = match f.status {
                octocrab::models::repos::DiffEntryStatus::Added => "added",
                octocrab::models::repos::DiffEntryStatus::Removed => "removed",
                octocrab::models::repos::DiffEntryStatus::Modified => "modified",
                octocrab::models::repos::DiffEntryStatus::Renamed => "renamed",
                octocrab::models::repos::DiffEntryStatus::Copied => "copied",
                octocrab::models::repos::DiffEntryStatus::Changed => "changed",
                octocrab::models::repos::DiffEntryStatus::Unchanged => "unchanged",
                _ => "unknown",
            };
            // Extract language from filename extension
            let language = f.filename.split('.').next_back().map(|ext| {
                match ext {
                    "ts" | "tsx" => "typescript",
                    "js" | "jsx" | "mjs" | "cjs" => "javascript",
                    "py" => "python",
                    "go" => "go",
                    "java" => "java",
                    "rb" => "ruby",
                    "cs" => "csharp",
                    "cpp" | "cc" | "cxx" => "cpp",
                    "c" | "h" => "c",
                    "swift" => "swift",
                    "kt" | "kts" => "kotlin",
                    "scala" => "scala",
                    "rs" => "rust",
                    "php" => "php",
                    "ex" | "exs" => "elixir",
                    "erl" => "erlang",
                    "hs" => "haskell",
                    "ml" | "mli" => "ocaml",
                    "fs" | "fsx" => "fsharp",
                    "lua" => "lua",
                    "r" => "r",
                    "sql" => "sql",
                    "sh" | "bash" | "zsh" => "bash",
                    "ps1" => "powershell",
                    "yml" | "yaml" => "yaml",
                    "toml" => "toml",
                    "json" => "json",
                    "xml" => "xml",
                    "html" | "htm" => "html",
                    "css" | "scss" | "sass" | "less" => "css",
                    "md" | "markdown" => "markdown",
                    "dockerfile" => "dockerfile",
                    "tf" => "hcl",
                    "proto" => "protobuf",
                    _ => ext,
                }
                .to_string()
            });
            PullRequestDiff {
                filename: f.filename,
                status: status_str.to_string(),
                additions: f.additions,
                deletions: f.deletions,
                patch: f.patch,
                language,
            }
        })
        .collect();

    Ok(files)
}

/// Represents a review event from GitHub history
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewActivity {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub author: String,
    pub reviewed_at: chrono::DateTime<chrono::Utc>,
    pub state: String, // "APPROVED", "CHANGES_REQUESTED", "COMMENTED"
}

pub async fn fetch_my_review_activity(
    token: &str,
    org: &str,
    repos: &[String],
    username: &str,
    days: u32,
) -> Result<Vec<ReviewActivity>> {
    let since = chrono::Utc::now() - chrono::Duration::days(days as i64);

    // Phase 1: Fetch all PRs from all repos in parallel
    let pr_futures = repos.iter().map(|repo| {
        let client = new_client(token);
        let org = org.to_string();
        let repo = repo.clone();
        // since already bound

        async move {
            let prs = client
                .pulls(&org, &repo)
                .list()
                .state(octocrab::params::State::All)
                .per_page(100)
                .send()
                .await
                .unwrap_or_default();

            // Filter PRs updated within our window
            let candidates: Vec<_> = prs
                .items
                .into_iter()
                .filter(|pr| {
                    let updated_at = pr
                        .updated_at
                        .unwrap_or_else(|| pr.created_at.unwrap_or_else(chrono::Utc::now));
                    updated_at >= since
                })
                .map(|pr| {
                    (
                        repo.clone(),
                        pr.number,
                        pr.title.clone().unwrap_or_default(),
                        pr.user
                            .as_ref()
                            .map(|u| u.login.clone())
                            .unwrap_or_default(),
                    )
                })
                .collect();

            (org, repo, candidates)
        }
    });

    #[allow(clippy::type_complexity)]
    let repo_results: Vec<(String, String, Vec<(String, u64, String, String)>)> =
        join_all(pr_futures).await;

    // Phase 2: Fetch timelines for all candidate PRs in parallel
    let timeline_futures: Vec<_> = repo_results
        .iter()
        .flat_map(|(org, repo, prs)| {
            prs.iter()
                .map(|(_, pr_number, _, _)| {
                    let client = new_client(token);
                    let org = org.clone();
                    let repo = repo.clone();
                    let pr_number = *pr_number;

                    async move {
                        let timeline: Vec<serde_json::Value> = client
                            .get(
                                format!("/repos/{}/{}/issues/{}/timeline", org, repo, pr_number),
                                None::<&str>,
                            )
                            .await
                            .unwrap_or_default();

                        (pr_number, repo, timeline)
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let timeline_results: Vec<(u64, String, Vec<serde_json::Value>)> =
        join_all(timeline_futures).await;

    // Phase 3: Parse timeline results and extract reviews
    let mut all_reviews = Vec::new();
    for (pr_number, repo, timeline) in timeline_results {
        // Find the PR info from repo_results
        let pr_info = repo_results
            .iter()
            .find(|(_, r, _)| r == &repo)
            .and_then(|(_, _, prs)| prs.iter().find(|(_, num, _, _)| *num == pr_number));

        let (pr_title, pr_author) = match pr_info {
            Some((_, _, title, author)) => (title.clone(), author.clone()),
            None => continue,
        };

        for event in timeline {
            // Look for "PullRequestReview" events by our username
            if event.get("event").and_then(|e| e.as_str()) == Some("pull_request_review") {
                if let Some(user_obj) = event.get("user") {
                    if user_obj.get("login").and_then(|l| l.as_str()) == Some(username) {
                        let reviewed_at = event
                            .get("submitted_at")
                            .and_then(|t| t.as_str())
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&chrono::Utc))
                            .unwrap_or_else(chrono::Utc::now);

                        let state = event
                            .get("state")
                            .and_then(|s| s.as_str())
                            .unwrap_or("COMMENTED")
                            .to_string();

                        all_reviews.push(ReviewActivity {
                            repo,
                            pr_number,
                            pr_title,
                            author: pr_author.clone(),
                            reviewed_at,
                            state,
                        });
                        break; // one review per user per PR
                    }
                }
            }
        }
    }

    // Sort by most recent first
    all_reviews.sort_by_key(|b| std::cmp::Reverse(b.reviewed_at));
    Ok(all_reviews)
}

/// Fetch merge conflict status for a list of pending reviews
pub async fn fetch_merge_conflict_status(
    token: &str,
    org: &str,
    reviews: &[PendingReview],
) -> Result<Vec<MergeConflictStatus>> {
    // Build a flat list of (repo, pr_number) to fetch in parallel
    let fetch_tasks: Vec<(String, u64)> = reviews
        .iter()
        .map(|r| (r.repo.clone(), r.pr_number))
        .collect();

    // Fetch all PR details in parallel using join_all
    let client = new_client(token);

    let futures = fetch_tasks.iter().map(|(repo, pr_number)| {
        let client = client.clone();
        let repo = repo.clone();
        let pr_number = *pr_number;
        async move { client.pulls(org, &repo).get(pr_number).await }
    });

    let results_vec: Vec<Result<_, _>> = join_all(futures).await;

    let mut results = Vec::new();
    for ((repo, pr_number), result) in fetch_tasks.into_iter().zip(results_vec) {
        match result {
            Ok(pr) => {
                let has_conflicts = pr.mergeable == Some(false);
                results.push(MergeConflictStatus {
                    repo,
                    pr_number,
                    pr_title: pr.title.unwrap_or_default(),
                    has_conflicts,
                    mergeable: pr.mergeable,
                    rebaseable: pr.rebaseable,
                });
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to fetch PR #{} in {}: {}",
                    pr_number, repo, e
                );
            }
        }
    }

    Ok(results)
}

/// Fetch CI (GitHub Actions / checks) status for a list of pending reviews
pub async fn fetch_ci_status(
    token: &str,
    org: &str,
    reviews: &[PendingReview],
) -> Result<Vec<CiStatus>> {
    let client = new_client(token);

    // Fetch PR info + combined status + check runs for each PR in parallel
    let futures = reviews.iter().map(|review| {
        let client = client.clone();
        let org = org.to_string();
        let repo = review.repo.clone();
        let pr_number = review.pr_number;

        async move {
            // Step 1: Get PR head SHA
            let pr = client.pulls(&org, &repo).get(pr_number).await?;
            let head_sha = pr.head.sha.clone();
            let pr_title = pr.title.clone().unwrap_or_default();

            // Step 2 & 3: Fetch combined status and check runs in parallel
            let status_url = format!("/repos/{}/{}/commits/{}/status", org, repo, head_sha);
            let check_runs_url = format!("/repos/{}/{}/commits/{}/check-runs", org, repo, head_sha);

            #[derive(serde::Deserialize)]
            struct CombinedStatus {
                state: String,
            }

            #[derive(serde::Deserialize)]
            struct CheckRunsResponse {
                #[allow(dead_code)]
                total_count: u32,
                check_runs: Vec<CheckRunDto>,
            }

            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct CheckRunDto {
                name: String,
                status: String,
                conclusion: Option<String>,
                app_name: String,
                started_at: Option<String>,
                completed_at: Option<String>,
            }

            // Fetch status and check runs concurrently
            let (overall_status, check_runs_result) = tokio::join!(
                client.get::<CombinedStatus, _, _>(&status_url, None::<&str>),
                client.get::<CheckRunsResponse, _, _>(&check_runs_url, None::<&str>),
            );

            let overall_status = overall_status
                .map(|s: CombinedStatus| s.state)
                .unwrap_or_else(|_| "unknown".to_string());

            let checks: Vec<CiCheck> = check_runs_result
                .map(|response: CheckRunsResponse| {
                    response
                        .check_runs
                        .into_iter()
                        .map(|cr| CiCheck {
                            name: cr.name,
                            status: cr.status,
                            conclusion: cr.conclusion,
                            app_name: cr.app_name,
                            started_at: cr.started_at,
                            completed_at: cr.completed_at,
                        })
                        .collect()
                })
                .unwrap_or_default();

            Ok::<CiStatus, anyhow::Error>(CiStatus {
                repo: repo.clone(),
                pr_number,
                pr_title,
                head_sha,
                overall_status,
                checks,
            })
        }
    });

    let results: Vec<Result<CiStatus>> = join_all(futures).await;

    // Collect successes, log failures
    let mut ci_statuses = Vec::new();
    for (review, result) in reviews.iter().zip(results) {
        match result {
            Ok(status) => ci_statuses.push(status),
            Err(e) => {
                eprintln!(
                    "Warning: Failed to fetch CI status for #{} in {}: {}",
                    review.pr_number, review.repo, e
                );
            }
        }
    }

    Ok(ci_statuses)
}

/// Represents a GitHub notification/mention for the current user.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Mention {
    /// Repository name (e.g. "org/repo")
    pub repo: String,
    /// PR or issue number
    pub pr_number: u64,
    /// PR or issue title
    pub pr_title: String,
    /// Author of the PR/issue
    pub author: String,
    /// URL to the PR/issue
    pub pr_url: String,
    /// Whether this notification is unread
    pub unread: bool,
    /// Why the user is receiving this notification (mention, review_requested, etc.)
    pub reason: String,
    /// When the notification was last updated
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Preview of the last comment that triggered the mention (if available)
    pub last_comment_preview: String,
}

/// Fetch GitHub notifications where the user was mentioned or directly involved.
pub async fn fetch_mentions(
    token: &str,
    _username: &str,
    unread_only: bool,
    limit: usize,
) -> Result<Vec<Mention>> {
    let client = new_client(token);

    // Use GitHub's notifications API
    #[derive(serde::Deserialize)]
    struct Notification {
        #[allow(dead_code)]
        id: String,
        unread: bool,
        reason: String,
        updated_at: String,
        #[serde(rename = "subject")]
        subject: NotificationSubject,
        repository: NotificationRepo,
    }

    #[derive(serde::Deserialize)]
    struct NotificationSubject {
        title: String,
        #[serde(rename = "type")]
        notification_type: String,
        url: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct NotificationRepo {
        full_name: String,
    }

    let all_notifications: Vec<Notification> = client
        .get("notifications", Some(&[("per_page", "100")]))
        .await
        .unwrap_or_default();

    let mut mentions = Vec::new();

    // First pass: collect all notifications that match our filters (before fetching PR details)
    for notification in all_notifications {
        // Filter: only PR/issue notifications (not repository, discussions, etc.)
        if notification.subject.notification_type != "PullRequest"
            && notification.subject.notification_type != "Issue"
        {
            continue;
        }

        // Filter: only if it involves this user (mention, review_requested, assign, author, team_mention)
        let relevant_reasons = [
            "mention",
            "review_requested",
            "assign",
            "author",
            "team_mention",
            "cm",
        ];
        if !relevant_reasons.contains(&notification.reason.as_str()) {
            continue;
        }

        // Filter: unread only
        if unread_only && !notification.unread {
            continue;
        }

        // Parse the PR/issue number and repo from the subject URL
        // URL format: https://api.github.com/repos/{org}/{repo}/pulls/{number}
        let (org_repo, pr_number): (String, u64) = if let Some(ref url) = notification.subject.url {
            let parts: Vec<&str> = url.split('/').collect();
            if parts.len() >= 2 {
                // parts: ["https:", "", "api.github.com", "repos", org, repo, "pulls", number]
                let org = parts.get(4).unwrap_or(&"");
                let repo = parts.get(5).unwrap_or(&"");
                let num = parts.get(7).and_then(|s| s.parse().ok()).unwrap_or(0);
                (format!("{}/{}", org, repo), num)
            } else {
                (notification.repository.full_name.clone(), 0)
            }
        } else {
            (notification.repository.full_name.clone(), 0)
        };

        // Convert API URL to web URL
        let pr_url = notification
            .subject
            .url
            .as_ref()
            .map(|url| {
                url.replace("https://api.github.com/repos/", "https://github.com/")
                    .replace("/pulls/", "/pull/")
            })
            .unwrap_or_else(|| {
                format!(
                    "https://github.com/{}/pull/{}",
                    notification.repository.full_name, pr_number
                )
            });

        // Parse updated_at
        let updated_at = chrono::DateTime::parse_from_rfc3339(&notification.updated_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        // Parse repo parts for the API call
        let repo_parts: Vec<&str> = org_repo.split('/').collect();
        let _org = repo_parts.first().unwrap_or(&"");
        let _repo = repo_parts.get(1).unwrap_or(&"");

        mentions.push(Mention {
            repo: org_repo,
            pr_number,
            pr_title: notification.subject.title,
            author: String::new(), // Will be filled in parallel fetch
            pr_url,
            unread: notification.unread,
            reason: notification.reason,
            updated_at,
            last_comment_preview: String::new(),
        });
    }

    // Limit mentions before fetching details (avoid unnecessary API calls)
    // We fetch a bit extra in case some fail or don't return PR data
    let max_to_fetch = std::cmp::min(mentions.len(), limit * 2);
    mentions.truncate(max_to_fetch);

    // Parallel fetch PR details to get author information
    let detail_futures = mentions.iter().filter(|m| m.pr_number > 0).map(|m| {
        let client = new_client(token);
        let repo = m.repo.clone();
        let pr_number = m.pr_number;

        async move {
            // Parse repo parts
            let parts: Vec<&str> = repo.split('/').collect();
            let org = parts.first().unwrap_or(&"");
            let repo_name = parts.get(1).unwrap_or(&"");
            // For PullRequest notifications, fetch the PR to get author
            client
                .pulls(org.to_string(), repo_name.to_string())
                .get(pr_number)
                .await
        }
    });

    let detail_results: Vec<Result<_, _>> = join_all(detail_futures).await;

    // Build a map of (repo, pr_number) -> author
    let mut author_map: std::collections::HashMap<(String, u64), String> =
        std::collections::HashMap::new();
    for (mention, result) in mentions.iter().zip(detail_results) {
        if mention.pr_number > 0 {
            if let Ok(detail) = result {
                let author = detail
                    .user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .unwrap_or_default();
                author_map.insert((mention.repo.clone(), mention.pr_number), author);
            }
        }
    }

    // Update mentions with author information and truncate to limit
    for mention in &mut mentions {
        if let Some(author) = author_map.get(&(mention.repo.clone(), mention.pr_number)) {
            mention.author = author.clone();
        } else if mention.pr_number > 0 {
            mention.author = "unknown".to_string();
        }
    }

    mentions.truncate(limit);

    // Sort by most recently updated
    mentions.sort_by_key(|b| std::cmp::Reverse(b.updated_at));

    Ok(mentions)
}

/// Represents GitHub API rate limit information.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RateLimitInfo {
    /// Resource type (e.g., "core", "search", "graphql")
    pub resource: String,
    /// Current limit
    pub limit: u32,
    /// Remaining requests in current window
    pub remaining: u32,
    /// Reset timestamp
    pub reset: chrono::DateTime<chrono::Utc>,
    /// Seconds until reset
    pub reset_in_seconds: i64,
}

/// Represents overall GitHub API health status.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthStatus {
    /// API rate limits
    pub rate_limits: Vec<RateLimitInfo>,
    /// GitHub API server current time
    pub server_time: chrono::DateTime<chrono::Utc>,
    /// Whether the authenticated user has a valid token
    pub authenticated: bool,
    /// GitHub username (if authenticated)
    pub username: Option<String>,
    /// Whether the rate limit is being hit (remaining < 10% of limit)
    pub rate_limit_warning: bool,
}

/// Fetch GitHub API rate limits and health status.
pub async fn fetch_health_status(token: &str) -> Result<HealthStatus> {
    let client = new_client(token);

    #[derive(serde::Deserialize)]
    struct RateLimitResponse {
        resources: RateResources,
        rate: RateLimitEntry,
    }

    #[derive(serde::Deserialize)]
    struct RateResources {
        #[allow(dead_code)]
        #[serde(rename = "core")]
        core: RateLimitEntry,
        #[serde(rename = "search")]
        search: RateLimitEntry,
        #[serde(rename = "graphql")]
        graphql: Option<RateLimitEntry>,
    }

    #[derive(serde::Deserialize)]
    struct RateLimitEntry {
        limit: u32,
        remaining: u32,
        reset: u64,
        #[allow(dead_code)]
        #[serde(rename = "used")]
        used: u32,
        #[allow(dead_code)]
        #[serde(rename = "resource")]
        resource: Option<String>,
    }

    let rate_limits: Vec<RateLimitInfo> = client
        .get("rate_limit", None::<&str>)
        .await
        .map(|response: RateLimitResponse| {
            let now = chrono::Utc::now();
            let mut limits = vec![];

            // Core API
            let reset_core =
                chrono::DateTime::from_timestamp(response.rate.reset as i64, 0).unwrap_or(now);
            limits.push(RateLimitInfo {
                resource: "core".to_string(),
                limit: response.rate.limit,
                remaining: response.rate.remaining,
                reset: reset_core,
                reset_in_seconds: (reset_core - now).num_seconds(),
            });

            // Search API
            let reset_search =
                chrono::DateTime::from_timestamp(response.resources.search.reset as i64, 0)
                    .unwrap_or(now);
            limits.push(RateLimitInfo {
                resource: "search".to_string(),
                limit: response.resources.search.limit,
                remaining: response.resources.search.remaining,
                reset: reset_search,
                reset_in_seconds: (reset_search - now).num_seconds(),
            });

            // GraphQL (optional)
            if let Some(graphql) = response.resources.graphql {
                let reset_graphql =
                    chrono::DateTime::from_timestamp(graphql.reset as i64, 0).unwrap_or(now);
                limits.push(RateLimitInfo {
                    resource: "graphql".to_string(),
                    limit: graphql.limit,
                    remaining: graphql.remaining,
                    reset: reset_graphql,
                    reset_in_seconds: (reset_graphql - now).num_seconds(),
                });
            }

            limits
        })
        .unwrap_or_default();

    // Try to get authenticated user info
    #[derive(serde::Deserialize)]
    struct UserResponse {
        login: String,
    }

    let (authenticated, username) =
        match client.get::<UserResponse, _, _>("user", None::<&str>).await {
            Ok(user) => (true, Some(user.login)),
            Err(_) => (false, None),
        };

    let server_time = chrono::Utc::now();

    // Check if any rate limit is critically low (< 10%)
    let rate_limit_warning = rate_limits
        .iter()
        .any(|r| r.limit > 0 && (r.remaining as f64 / r.limit as f64) < 0.1);

    Ok(HealthStatus {
        rate_limits,
        server_time,
        authenticated,
        username,
        rate_limit_warning,
    })
}

/// Represents a single event in a PR's timeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineEvent {
    /// Event type: "PullRequestReview", "Comment", "LabelEvent", "AssignEvent", "MilestoneEvent", "ClosedEvent", "ReopenedEvent", "MergedEvent", etc.
    pub event: String,
    /// When this event occurred
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Actor (user) who triggered this event
    pub actor: Option<String>,
    /// Event-specific data (e.g., review state, comment body preview)
    pub data: serde_json::Value,
}

/// Fetch the timeline of events for a specific PR.
pub async fn fetch_pr_timeline(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<TimelineEvent>> {
    let client = new_client(token);

    let timeline_url = format!("/repos/{}/{}/issues/{}/timeline", org, repo, pr_number);

    let events: Vec<serde_json::Value> = client
        .get(&timeline_url, Some(&[("per_page", "100")]))
        .await
        .unwrap_or_default();

    let mut timeline = Vec::new();

    for event in events {
        let event_type = event
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown")
            .to_string();

        let created_at = event
            .get("created_at")
            .and_then(|t| t.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);

        let actor = event
            .get("actor")
            .or_else(|| event.get("user"))
            .and_then(|a| a.get("login"))
            .and_then(|l| l.as_str())
            .map(String::from);

        // Extract event-specific data
        let mut data = serde_json::json!({});

        match event_type.as_str() {
            "PullRequestReview" => {
                if let Some(state) = event.get("state").and_then(|s| s.as_str()) {
                    data["review_state"] = serde_json::json!(state);
                }
                if let Some(body) = event.get("body").and_then(|b| b.as_str()) {
                    let preview: String = body.chars().take(200).collect();
                    data["body_preview"] = serde_json::json!(preview);
                }
            }
            "Comment" | "IssueComment" => {
                if let Some(body) = event.get("body").and_then(|b| b.as_str()) {
                    let preview: String = body.chars().take(200).collect();
                    data["body_preview"] = serde_json::json!(preview);
                }
            }
            "labeled" => {
                if let Some(label) = event.get("label").and_then(|l| l.get("name")) {
                    data["label"] = label.clone();
                }
            }
            "unlabeled" => {
                if let Some(label) = event.get("label").and_then(|l| l.get("name")) {
                    data["label"] = label.clone();
                }
            }
            "assigned" | "unassigned" => {
                if let Some(assignee) = event.get("assignee").and_then(|a| a.get("login")) {
                    data["assignee"] = assignee.clone();
                }
            }
            "merged" => {
                if let Some(merge_commit_sha) = event.get("merge_commit_sha") {
                    data["merge_commit_sha"] = merge_commit_sha.clone();
                }
            }
            "closed" => {
                if let Some(merged) = event.get("merged") {
                    data["merged"] = merged.clone();
                }
            }
            _ => {
                // For other event types, include relevant fields
                if let Some(label) = event.get("label").and_then(|l| l.get("name")) {
                    data["label"] = label.clone();
                }
            }
        }

        timeline.push(TimelineEvent {
            event: event_type,
            created_at,
            actor,
            data,
        });
    }

    // Timeline API returns events newest-first, but we want chronological order
    timeline.reverse();

    Ok(timeline)
}

/// Send an emoji reaction to a PR/issue to get author's attention without leaving a comment.
///
/// GitHub supports these reaction emojis:
/// - `+1` (thumbs up)
/// - `-1` (thumbs down)
/// - `laugh` (😂)
/// - `confused` (😕)
/// - `heart` (❤️)
/// - `hooray` (🎉)
/// - `rocket` (🚀)
/// - `eyes` (👀)
pub async fn add_pr_reaction(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
    reaction: &str,
) -> Result<()> {
    let client = new_client(token);

    // Map friendly names to GitHub content values
    let content = match reaction {
        "+1" | "thumbsup" | "thumbs_up" => "+1",
        "-1" | "thumbsdown" | "thumbs_down" => "-1",
        "laugh" | "laughed" | "lol" => "laugh",
        "confused" | "unsure" => "confused",
        "heart" | "love" => "heart",
        "hooray" | "party" | "celebrate" => "hooray",
        "rocket" | "rockets" => "rocket",
        "eyes" | "looking" | "👀" => "eyes",
        _ => reaction, // Use as-is if it doesn't match
    };

    let url = format!("/repos/{}/{}/issues/{}/reactions", org, repo, pr_number);

    #[derive(serde::Serialize)]
    struct ReactionPayload {
        content: String,
    }

    let _: serde_json::Value = client
        .post(
            &url,
            Some(&ReactionPayload {
                content: content.to_string(),
            }),
        ) // Uses POST /repos/{owner}/{repo}/issues/{issue_number}/reactions
        .await?;

    Ok(())
}

// Check if a user has commented on a specific PR
pub async fn has_user_commented(
    token: &str,
    org: &str,
    repo: &str,
    pr_number: u64,
    username: &str,
) -> Result<bool> {
    let client = new_client(token);

    let mut page = client
        .issues(org, repo)
        .list_comments(pr_number)
        .per_page(100)
        .send()
        .await?;

    loop {
        for comment in &page.items {
            if comment.user.login.to_lowercase() == username.to_lowercase() {
                return Ok(true);
            }
        }
        if page.next.is_none() {
            break;
        }
        match client.get_page(&page.next).await {
            Ok(Some(next)) => page = next,
            _ => break,
        }
    }

    Ok(false)
}

// Fetch PRs where the user has commented
pub async fn fetch_prs_user_commented_on(
    token: &str,
    org: &str,
    repos: &[String],
    username: &str,
) -> Result<Vec<PendingReview>> {
    // Phase 1: Collect all PRs across all repos in parallel with pagination
    #[derive(Clone)]
    struct CandidatePr {
        repo: String,
        number: u64,
        title: String,
        author: String,
        url: String,
        created_at: DateTime<Utc>,
        draft: bool,
        branch: String,
    }

    let client = new_client(token);

    let repo_futures = repos.iter().map(|repo| {
        let client = client.clone();
        let repo = repo.clone();
        let org = org.to_string();
        async move {
            let mut all_prs = Vec::new();
            let first_page = client
                .pulls(&org, &repo)
                .list()
                .state(octocrab::params::State::Open)
                .per_page(100)
                .send()
                .await;

            if let Ok(page) = first_page {
                let candidates: Vec<CandidatePr> = page
                    .items
                    .iter()
                    .map(|pr: &octocrab::models::pulls::PullRequest| CandidatePr {
                        repo: repo.clone(),
                        number: pr.number,
                        title: pr.title.clone().unwrap_or_default(),
                        author: pr
                            .user
                            .as_ref()
                            .map(|u| u.login.clone())
                            .unwrap_or_default(),
                        url: pr
                            .html_url
                            .as_ref()
                            .map(|u| u.to_string())
                            .unwrap_or_default(),
                        created_at: pr.created_at.unwrap_or_default(),
                        draft: pr.draft.unwrap_or(false),
                        branch: pr.head.label.clone().unwrap_or_default(),
                    })
                    .collect();
                all_prs.extend(candidates);

                let mut next_page = page.next;
                while next_page.is_some() {
                    match client.get_page(&next_page).await {
                        Ok(Some(p)) => {
                            let more: Vec<CandidatePr> = p
                                .items
                                .iter()
                                .map(|pr: &octocrab::models::pulls::PullRequest| CandidatePr {
                                    repo: repo.clone(),
                                    number: pr.number,
                                    title: pr.title.clone().unwrap_or_default(),
                                    author: pr
                                        .user
                                        .as_ref()
                                        .map(|u| u.login.clone())
                                        .unwrap_or_default(),
                                    url: pr
                                        .html_url
                                        .as_ref()
                                        .map(|u| u.to_string())
                                        .unwrap_or_default(),
                                    created_at: pr.created_at.unwrap_or_default(),
                                    draft: pr.draft.unwrap_or(false),
                                    branch: pr.head.label.clone().unwrap_or_default(),
                                })
                                .collect();
                            next_page = p.next.clone();
                            all_prs.extend(more);
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }

            all_prs
        }
    });

    let all_candidates: Vec<CandidatePr> =
        join_all(repo_futures).await.into_iter().flatten().collect();

    // Phase 2: Check comments for all PRs in parallel using shared client
    let check_futures = all_candidates.iter().map(|c| {
        let client = client.clone();
        let repo = c.repo.clone();
        let org = org.to_string();
        let number = c.number;
        let username = username.to_string();
        let c = c.clone();

        async move {
            let mut found = false;
            let mut page = match client
                .issues(&org, &repo)
                .list_comments(number)
                .per_page(100)
                .send()
                .await
            {
                Ok(p) => p,
                Err(_) => return (c, false),
            };

            loop {
                if page.items.iter().any(|cm| cm.user.login == username) {
                    found = true;
                    break;
                }
                if page.next.is_none() {
                    break;
                }
                match client.get_page(&page.next).await {
                    Ok(Some(next)) => page = next,
                    _ => break,
                }
            }

            (c, found)
        }
    });

    let results: Vec<_> = join_all(check_futures).await;

    // Phase 3: Collect PRs where user commented
    let commented_prs: Vec<PendingReview> = results
        .into_iter()
        .filter(|(_, commented)| *commented)
        .map(|(c, _)| PendingReview {
            repo: c.repo,
            pr_number: c.number,
            pr_title: c.title,
            pr_author: c.author,
            pr_url: c.url,
            created_at: c.created_at,
            additions: 0,
            deletions: 0,
            draft: c.draft,
            branch: c.branch,
        })
        .collect();

    Ok(commented_prs)
}
