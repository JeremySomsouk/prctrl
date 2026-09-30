# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.2.0] - 2026-10-01

### Performance
- Keep TUI input and rendering responsive during GitHub loads; share snapshots and in-flight requests across related views.
- Reuse one HTTP client and cap PR-list and detail requests at eight concurrent requests across the TUI.
- Format only visible PR rows and preserve the scrolling viewport; compute summary metrics when data changes.

### Fixed
- Restore automatic refresh deadlines, support `--interval 0` for manual refresh, and retain results after failed syncs.
- Publish preloaded views independently; preserve selection by repository and PR number after updates.
- Treat shortcut letters as text in the filter, confine modal input, and restore the terminal on errors and quit.

### Changed
- Replace the TUI with a responsive review desk, selected PR details, contextual shortcuts, textual status labels and `NO_COLOR` support.
- Make `r` request fresh data and make `Esc` close or clear the focused control without exiting.
- Remove unimplemented actions from the TUI menu instead of claiming review or clipboard operations succeeded.

## [1.0.0] - 2026-04-08

### Performance
- **Shared Octocrab client** — Replaced 21+ per-request `Octocrab::builder()` calls with a single `new_client()` helper that reuses HTTP connection pools (keep-alive, TLS sessions) across all API calls
- **Lazy detail fetch** — `fetch_pending_reviews` and `fetch_my_open_prs` now capture `additions`/`deletions` from the list endpoint; only PRs missing those fields trigger an individual `GET /pulls/{number}` call, eliminating N extra round-trips in most cases
- **Full pagination** — All PR-listing functions (`fetch_pending_reviews`, `fetch_my_open_prs`, `stack::detect_stacks`, `fetch_prs_user_commented_on`, `has_user_commented`) now follow all pages instead of silently capping at 50–100 PRs per repo
- **Non-blocking monitor** — Replaced `std::thread::sleep` with `tokio::time::sleep().await` in the monitor loop so it no longer blocks the async runtime
- Removed unnecessary `.clone()` on `first_page.items` in pagination loops

### Security
- **Token masking** — `prctrl config show` now masks sensitive values (token, api_key, anthropic_api_key) to `ghp_****xxxx` format instead of printing them in plaintext

### Fixed
- Log a warning when the snooze file has malformed JSON instead of silently ignoring it
- Removed duplicate `history` entry in `PRCTRL_DOCS` (chat command reference); added missing commands (`mine`, `ping`, `stack`, `chat`)
- Updated stale `commands/mod.rs` doc comment referencing deleted `AppContext`
- Deduplicated priority scoring logic in `logger.rs` (`calculate_priority_score_for_stats` now delegates to `calculate_priority_score`)
- Removed redundant `claude --version` check in `start_chat()` (already validated by `get_backend()`)
- Removed dead code: `retry.rs` module, `client.rs` module (`AppContext`), and unused `commands/stack.rs::run()`

### Changed
- Version bumped to 1.0.0

## [0.9.5] - 2026-04-08

### Fixed
- **Stack detection rewrite** — Replaced `gh` CLI with octocrab API for reliable multi-repo fetching
- Added convention-based stack detection: groups PRs by ticket key (e.g. `TAHC-1666`) with `[N/M]` position markers
- Fixed branch-chain detection creating overlapping stacks (root-first top-down approach)
- Fixed `repository` field being invalid in `gh pr list --json` (caused empty repo names)
- Pass token/org/repos/author/drafts params to `detect_stacks` so `mine -d` filters correctly

### Added
- `LICENSE` file (MIT)
- `--version` flag via clap
- CI workflow (`.github/workflows/ci.yml`) — test, clippy, fmt, check
- `--json` flag on `stack` command with `Serialize` on stack types
- `--author` flag on `stack` command
- `CHANGELOG.md`
- `V1_TODO.md` — v1.0.0 readiness tracking

### Changed
- Replaced `.unwrap()` on `Octocrab::builder().build()` with `.expect("failed to build GitHub client")`
- Removed unused `--tree` flag from `stack` command
- Deduplicated `detect_stacks` calls in `mine` command (was calling 3×, now 1×)
- Fixed unclosed TOML code block in `README.md`
- Added `regex` dependency for ticket key extraction

## [0.9.4] - 2026-03-XX

### Added
- Initial public release with 50+ commands
- Core commands: list, mine, delegate, stats, monitor
- Review actions: approve, assign, claim, comment, chase
- Discovery: search, filter, top, focus, quick
- Analytics: report, review-time, review-velocity, trends, digest
- Stack detection (branch-chaining only)
- Claude AI integration (delegate, chat)
- macOS native notifications
- Snooze management
- Follow/unfollow PRs
- CI/status/blocked PR detection

[0.9.5]: https://github.com/JeremySomsouk/prctrl/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/JeremySomsouk/prctrl/releases/tag/v0.9.4
