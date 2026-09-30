//! Render a synthetic review desk to JSON cells for screenshots, without GitHub access.
//! Usage: cargo run --example tui_preview -- /tmp/prctrl-preview.json 120 32
use anyhow::Result;
use chrono::{Duration, Utc};
use prctrl::{
    config::Config,
    github::PendingReview,
    tui::{app::ReviewSummary, App, Ui},
};
use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};

fn color(value: Color, fallback: [u8; 3]) -> [u8; 3] {
    match value {
        Color::Rgb(r, g, b) => [r, g, b],
        _ => fallback,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("/tmp/prctrl-preview.json");
    let width = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);
    let height = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(32);
    let config = Config {
        github_token: "preview-only".into(),
        github_username: "developer".into(),
        github_org: "example".into(),
        github_repos: vec!["api".into(), "web".into(), "tools".into()],
        github_teams: vec![],
        crew_members: vec![],
        anthropic_api_key: None,
        exclude_prefix: vec![],
        max_pr_age_days: Some(60),
    };
    let mut app = App::new(config, 0).await?;
    app.reviews = [
        (
            4821,
            "Reuse HTTP connections across requests",
            "api",
            "alex",
            10,
            false,
        ),
        (
            4818,
            "Keep keyboard input active during sync",
            "tools",
            "sam",
            5,
            false,
        ),
        (4812, "Add accessible focus styles", "web", "lee", 3, false),
        (
            4809,
            "Simplify deployment configuration",
            "api",
            "alex",
            2,
            false,
        ),
        (
            4804,
            "Explore a new search interaction",
            "tools",
            "sam",
            1,
            true,
        ),
    ]
    .into_iter()
    .map(|(number, title, repo, author, days, draft)| PendingReview {
        repo: repo.into(),
        pr_number: number,
        pr_title: title.into(),
        pr_author: author.into(),
        pr_url: format!("https://github.com/example/{repo}/pull/{number}"),
        created_at: Utc::now() - Duration::days(days),
        additions: 128,
        deletions: 34,
        draft,
        branch: "feature/shared-transport".into(),
    })
    .collect::<Vec<_>>()
    .into();
    app.summary = ReviewSummary {
        drafts: 1,
        older: 1,
        authors: 3,
        repos: 3,
        additions: 640,
        deletions: 170,
    };
    app.loading = false;
    app.last_refresh = Some(Utc::now());
    app.update_filtered_indices();
    app.use_color = true;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| Ui::draw(frame, &mut app))?;
    let cells: Vec<_> = terminal.backend().buffer().content.iter().map(|cell| serde_json::json!({
        "text": cell.symbol(), "fg": color(cell.fg, [232, 237, 245]), "bg": color(cell.bg, [18, 24, 35]),
        "bold": cell.modifier.contains(Modifier::BOLD),
    })).collect();
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({"width": width, "height": height, "cells": cells}))?,
    )?;
    Ok(())
}
