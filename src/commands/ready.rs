use crate::config::Config;
use crate::github::{PendingReview, ReviewClient};
use crate::readiness::{safe_text, Readiness, ReadinessClient, ReadinessState};
use anyhow::{Context, Result};
use futures::{stream, StreamExt, TryStreamExt};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Serialize)]
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
    has_conflicts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority_score: Option<u8>,
    #[serde(flatten)]
    readiness: Readiness,
}
impl ReadyPr {
    fn new(pr: PendingReview, readiness: Readiness, priority: bool) -> Self {
        Self {
            priority_score: priority.then(|| crate::logger::calculate_priority_score(&pr)),
            age_days: (chrono::Utc::now() - pr.created_at).num_days(),
            approved: readiness.review_decision == "APPROVED",
            has_conflicts: readiness.merge_state == "DIRTY"
                || readiness.blockers.iter().any(|s| s == "Merge conflicts"),
            repo: pr.repo,
            pr_number: pr.pr_number,
            pr_title: pr.pr_title,
            pr_author: pr.pr_author,
            pr_url: pr.pr_url,
            additions: pr.additions,
            deletions: pr.deletions,
            readiness,
        }
    }
}

pub(crate) async fn fetch_targets(config: &Config, numbers: &[u64]) -> Result<Vec<PendingReview>> {
    anyhow::ensure!(
        !numbers.is_empty() && numbers.iter().all(|n| *n > 0 && *n <= i32::MAX as u64),
        "PR numbers must be between 1 and {}",
        i32::MAX
    );
    let transport = ReviewClient::new(&config.github_token, true);
    let targets = config
        .github_repos
        .iter()
        .flat_map(|repo| numbers.iter().map(move |number| (repo, *number)));
    let results: Vec<Option<PendingReview>> = stream::iter(targets)
        .map(|(repo, number)| {
            let transport = &transport;
            async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(30),
                    transport.request(transport.client.pulls(&config.github_org, repo).get(number)),
                )
                .await
                .context("GitHub target lookup timed out")?;
                match result {
                    Ok(pr) => Ok(Some(PendingReview {
                        repo: repo.clone(),
                        pr_number: pr.number,
                        pr_title: pr.title.unwrap_or_default(),
                        pr_author: pr
                            .user
                            .as_ref()
                            .map(|u| u.login.clone())
                            .unwrap_or_default(),
                        pr_url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                        created_at: pr.created_at.unwrap_or_default(),
                        additions: pr.additions.unwrap_or(0),
                        deletions: pr.deletions.unwrap_or(0),
                        draft: pr.draft.unwrap_or(false),
                        branch: pr.head.label.unwrap_or_default(),
                    })),
                    Err(octocrab::Error::GitHub { source, .. })
                        if source.status_code.as_u16() == 404 =>
                    {
                        Ok(None)
                    }
                    Err(_) => Err(anyhow::anyhow!(
                        "Could not resolve requested PR; check GitHub access and rate limits"
                    )),
                }
            }
        })
        .buffer_unordered(4)
        .try_collect()
        .await?;
    let reviews: Vec<_> = results.into_iter().flatten().collect();
    anyhow::ensure!(
        !reviews.is_empty(),
        "Requested PRs were not found or are inaccessible"
    );
    Ok(reviews)
}

pub(crate) async fn report(
    config: &Config,
    reviews: Vec<PendingReview>,
    json: bool,
    priority: bool,
) -> Result<()> {
    let client = ReadinessClient::new(ReviewClient::new(&config.github_token, true));
    let mut seen = HashSet::new();
    let mut rows: Vec<_> = stream::iter(
        reviews
            .into_iter()
            .filter(|pr| seen.insert((pr.repo.clone(), pr.pr_number))),
    )
    .map(|pr| {
        let client = &client;
        async move {
            let readiness = client
                .fetch(&config.github_org, &pr.repo, pr.pr_number)
                .await;
            ReadyPr::new(pr, readiness, priority)
        }
    })
    .buffer_unordered(4)
    .collect()
    .await;
    rows.sort_by(|a, b| {
        (a.readiness.state, a.age_days, &a.repo, a.pr_number).cmp(&(
            b.readiness.state,
            b.age_days,
            &b.repo,
            b.pr_number,
        ))
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!(
            "Readiness: {} PRs, {} ready",
            rows.len(),
            rows.iter()
                .filter(|p| p.readiness.state == ReadinessState::Ready)
                .count()
        );
        for row in rows {
            let r = &row.readiness;
            println!(
                "\n{}  {} / #{}  {}",
                r.state.label(),
                safe_text(&row.repo),
                row.pr_number,
                safe_text(&row.pr_title)
            );
            println!(
                "  CI: {}  /  Review: {}  /  Merge: {}",
                r.ci_status, r.review_decision, r.merge_state
            );
            println!(
                "  Head: {}  /  Observed: {}",
                r.head_sha.as_deref().unwrap_or("unknown"),
                r.observed_at.to_rfc3339()
            );
            for blocker in &r.blockers {
                println!("  - {blocker}");
            }
            if let Some(score) = row.priority_score {
                println!("  Priority: {score}/5");
            }
            println!("  {}", safe_text(&row.pr_url));
        }
        println!("\nRead-only snapshot. GitHub enforces rules and permissions at merge time.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_preserves_metadata_and_does_not_invent_approval_or_draft() {
        let row = ReadyPr::new(
            crate::tui::app::tests::review(7),
            Readiness::unknown("unavailable"),
            false,
        );
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["pr_number"], 7);
        assert_eq!(value["state"], "unknown");
        assert_eq!(value["approved"], false);
        assert!(value["draft"].is_null());
        assert!(value["head_sha"].is_null());
        assert!(value.get("priority_score").is_none());
        assert!(value["observed_at"].is_string());
    }
}
