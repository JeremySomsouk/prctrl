use crate::cache::{CacheKey, CacheKeyBuilder, PrCache};
use crate::config::Config;
use crate::github::{fetch_my_open_prs, fetch_pending_reviews, PendingReview};
use anyhow::Result;
use chrono::DateTime;
use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// Actions available for a selected PR
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrAction {
    /// Open PR in browser
    OpenInBrowser,
    /// Launch Claude Code review
    ClaudeReview,
    /// Copy PR URL to clipboard
    CopyUrl,
    /// Show PR diff
    ShowDiff,
    /// Approve PR
    Approve,
    /// Request changes
    RequestChanges,
    /// Cancel/close menu
    Cancel,
}

impl PrAction {
    pub fn all() -> Vec<PrAction> {
        vec![
            PrAction::OpenInBrowser,
            PrAction::ClaudeReview,
            PrAction::CopyUrl,
            PrAction::ShowDiff,
            PrAction::Approve,
            PrAction::RequestChanges,
            PrAction::Cancel,
        ]
    }

    pub fn display(&self) -> &'static str {
        match self {
            PrAction::OpenInBrowser => "Open in Browser",
            PrAction::ClaudeReview => "Claude Code Review",
            PrAction::CopyUrl => "Copy URL",
            PrAction::ShowDiff => "Show Diff",
            PrAction::Approve => "Approve PR",
            PrAction::RequestChanges => "Request Changes",
            PrAction::Cancel => "Cancel",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            PrAction::OpenInBrowser => "🌐",
            PrAction::ClaudeReview => "🤖",
            PrAction::CopyUrl => "📋",
            PrAction::ShowDiff => "📊",
            PrAction::Approve => "✅",
            PrAction::RequestChanges => "❌",
            PrAction::Cancel => "↩️",
        }
    }
}

/// Application state for the TUI
pub struct App {
    /// Configuration loaded from file or environment
    pub config: Config,
    /// List of pending reviews to display
    pub reviews: Vec<PendingReview>,
    /// Currently selected PR index in the list
    pub selected_pr: usize,
    /// Current tab/active command
    pub active_tab: Tab,
    /// List of available tabs/commands
    pub tabs: Vec<Tab>,
    /// Refresh interval in seconds
    pub refresh_interval: u64,
    /// Last refresh time
    pub last_refresh: Option<DateTime<chrono::Utc>>,
    /// Whether we're currently loading data
    pub loading: bool,
    /// Error message to display, if any
    pub error: Option<String>,
    /// Info message to display (non-error notifications)
    pub info: Option<String>,
    /// Filter string for PR list
    pub filter: String,
    /// Whether to show the help overlay
    pub show_help: bool,
    /// Whether to show the action menu for selected PR
    pub show_action_menu: bool,
    /// Currently selected action menu index
    pub selected_action: usize,
    /// Spinner animation frame
    pub spinner_frame: usize,
    /// Cache for storing PR data
    pub cache: PrCache,
    /// Current filtered indices (cached for performance)
    pub filtered_indices: Vec<usize>,
    /// Current position in filtered list
    pub filtered_position: usize,
    pub(crate) loads: JoinSet<(Tab, Result<Vec<PendingReview>>)>,
    pending: HashSet<Tab>,
    last_attempt: Option<Instant>,
}

/// Represents a tab/command in the left sidebar
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// List all pending reviews
    PendingReviews,
    /// My open PRs
    MyPullRequests,
    /// PRs from crew members
    Crew,
    /// Stats and metrics
    Statistics,
    /// Monitor Live - always shows fresh data
    MonitorLive,
}

impl std::fmt::Display for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tab::PendingReviews => write!(f, "Pending Reviews"),
            Tab::MyPullRequests => write!(f, "My PRs"),
            Tab::Crew => write!(f, "Crew"),
            Tab::Statistics => write!(f, "Statistics"),
            Tab::MonitorLive => write!(f, "Monitor Live"),
        }
    }
}

impl Tab {
    /// Get all available tabs
    pub fn all() -> Vec<Tab> {
        vec![
            Tab::PendingReviews,
            Tab::MyPullRequests,
            Tab::Crew,
            Tab::Statistics,
            Tab::MonitorLive,
        ]
    }

    /// Get the icon for the tab
    pub fn icon(&self) -> &'static str {
        match self {
            Tab::PendingReviews => "📋",
            Tab::MyPullRequests => "👤",
            Tab::Crew => "👥",
            Tab::Statistics => "📊",
            Tab::MonitorLive => "🔍",
        }
    }
}

impl App {
    /// Create a new App instance
    pub async fn new(config: Config, refresh_interval: u64) -> Result<Self> {
        let tabs = Tab::all();

        // Create app with empty reviews and loading state
        // The actual data fetch will happen in the first refresh
        Ok(Self {
            config,
            reviews: vec![],
            selected_pr: 0,
            active_tab: Tab::PendingReviews,
            tabs,
            refresh_interval,
            last_refresh: None,
            loading: true, // Show loading indicator initially
            error: None,
            info: None,
            filter: String::new(),
            show_help: false,
            show_action_menu: false,
            selected_action: 0,
            spinner_frame: 0,
            cache: PrCache::with_ttl(60), // 1 minute TTL
            filtered_indices: vec![],
            filtered_position: 0,
            loads: JoinSet::new(),
            pending: HashSet::new(),
            last_attempt: None,
        })
    }

    /// Fetch reviews based on current tab
    pub async fn fetch_reviews_for_tab(
        config: &Config,
        tab: &Tab,
        crew_members: &[String],
    ) -> Result<Vec<PendingReview>> {
        match tab {
            Tab::PendingReviews => {
                // Fetch PRs where user is requested as reviewer (exclude own PRs, exclude drafts)
                fetch_pending_reviews(
                    &config.github_token,
                    &config.github_org,
                    &config.github_repos,
                    &config.github_username,
                    &config.github_teams,
                    false, // include_mine - don't include own PRs
                    false, // include_drafts
                    &config.exclude_prefix,
                    crew_members,
                    config.max_pr_age_days,
                )
                .await
            }
            Tab::MyPullRequests => {
                // Fetch PRs authored by the current user
                fetch_my_open_prs(
                    &config.github_token,
                    &config.github_org,
                    &config.github_repos,
                    &config.github_username,
                    true, // include_drafts
                    &config.exclude_prefix,
                    config.max_pr_age_days,
                )
                .await
            }
            Tab::Crew => {
                // Fetch PRs from crew members only (uses crew_members from parameter)
                fetch_pending_reviews(
                    &config.github_token,
                    &config.github_org,
                    &config.github_repos,
                    &config.github_username,
                    &config.github_teams,
                    false,
                    false,
                    &config.exclude_prefix,
                    crew_members,
                    config.max_pr_age_days,
                )
                .await
            }
            Tab::Statistics => {
                // For stats, fetch all pending reviews
                fetch_pending_reviews(
                    &config.github_token,
                    &config.github_org,
                    &config.github_repos,
                    &config.github_username,
                    &config.github_teams,
                    false,
                    false,
                    &config.exclude_prefix,
                    crew_members,
                    config.max_pr_age_days,
                )
                .await
            }
            Tab::MonitorLive => {
                // Monitor Live always fetches fresh data - PRs needing review
                fetch_pending_reviews(
                    &config.github_token,
                    &config.github_org,
                    &config.github_repos,
                    &config.github_username,
                    &config.github_teams,
                    false,
                    false,
                    &config.exclude_prefix,
                    crew_members,
                    config.max_pr_age_days,
                )
                .await
            }
        }
    }

    /// Generate a cache key for the current tab and config
    fn cache_key_for(&self, tab: Tab) -> CacheKey {
        let crew_members: Vec<String> = match tab {
            Tab::Crew => self.config.crew_members.clone(),
            _ => vec![],
        };

        let tab_type = match tab {
            Tab::PendingReviews => "pending_reviews",
            Tab::MyPullRequests => "my_prs",
            Tab::Crew => "crew",
            Tab::Statistics => "statistics",
            Tab::MonitorLive => "monitor_live",
        };

        CacheKeyBuilder::new()
            .org(&self.config.github_org)
            .repos(&self.config.github_repos)
            .username(&self.config.github_username)
            .tab_type(tab_type)
            .include_mine(false) // this varies by tab, handled in fetch
            .include_drafts(false) // this varies by tab, handled in fetch
            .exclude_prefixes(&self.config.exclude_prefix)
            .crew_members(&crew_members)
            .max_age_days(self.config.max_pr_age_days)
            .build()
    }

    /// Schedule a load without awaiting the network in the input loop.
    pub async fn refresh(&mut self) -> Result<()> {
        self.request_tab(self.active_tab, false).await;
        Ok(())
    }

    pub async fn force_refresh(&mut self) -> Result<()> {
        self.request_tab(self.active_tab, true).await;
        Ok(())
    }

    async fn request_tab(&mut self, tab: Tab, force: bool) {
        if tab == self.active_tab {
            self.error = None;
            self.info = None;
        }
        let key = self.cache_key_for(tab);
        if !force && tab != Tab::MonitorLive {
            if let Some(reviews) = self.cache.get(&key).await {
                if tab == self.active_tab {
                    self.set_reviews(reviews);
                    self.loading = self.pending.contains(&tab);
                    self.last_attempt = Some(Instant::now());
                }
                return;
            }
        }
        if tab == self.active_tab {
            self.loading = true;
        }
        if self.pending.contains(&tab) {
            return;
        }
        let config = self.config.clone();
        let crew = if tab == Tab::Crew {
            config.crew_members.clone()
        } else {
            vec![]
        };
        self.start_load(tab, async move {
            tokio::time::timeout(
                Duration::from_secs(30),
                Self::fetch_reviews_for_tab(&config, &tab, &crew),
            )
            .await
            .map_err(|_| anyhow::anyhow!("GitHub load timed out. Press r to retry."))?
        });
    }

    fn start_load(
        &mut self,
        tab: Tab,
        load: impl Future<Output = Result<Vec<PendingReview>>> + Send + 'static,
    ) {
        self.pending.insert(tab);
        self.loads.spawn(async move { (tab, load.await) });
    }

    pub(crate) async fn finish_load(&mut self, tab: Tab, result: Result<Vec<PendingReview>>) {
        self.pending.remove(&tab);
        match result {
            Ok(reviews) => {
                self.cache
                    .set(self.cache_key_for(tab), reviews.clone())
                    .await;
                if tab == self.active_tab {
                    self.set_reviews(reviews);
                    self.last_refresh = Some(chrono::Utc::now());
                }
            }
            Err(error) if tab == self.active_tab => self.error = Some(format!("{error:#}")),
            Err(_) => {}
        }
        if tab == self.active_tab {
            self.loading = false;
            self.last_attempt = Some(Instant::now());
        }
    }

    fn set_reviews(&mut self, reviews: Vec<PendingReview>) {
        let selected = self
            .selected_pr_item()
            .map(|pr| (pr.repo.clone(), pr.pr_number));
        self.reviews = reviews;
        self.selected_pr = selected
            .and_then(|(repo, number)| {
                self.reviews
                    .iter()
                    .position(|pr| pr.repo == repo && pr.pr_number == number)
            })
            .unwrap_or(0);
        self.update_filtered_indices();
    }

    /// Each tab becomes usable as soon as its own load completes.
    pub async fn preload_all_tabs(&mut self) -> Result<()> {
        for tab in [
            Tab::PendingReviews,
            Tab::MyPullRequests,
            Tab::Crew,
            Tab::Statistics,
        ] {
            self.request_tab(tab, false).await;
        }
        Ok(())
    }

    /// Update filtered indices when reviews or filter changes
    pub fn update_filtered_indices(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.reviews.len()).collect();
        } else {
            let filter = self.filter.to_lowercase();
            self.filtered_indices = self
                .reviews
                .iter()
                .enumerate()
                .filter(|(_, pr)| {
                    pr.pr_title.to_lowercase().contains(&filter)
                        || pr.pr_author.to_lowercase().contains(&filter)
                        || pr.repo.to_lowercase().contains(&filter)
                        || pr.pr_number.to_string().contains(&filter)
                })
                .map(|(idx, _)| idx)
                .collect();
        }

        // Reset filtered position
        if let Some(pos) = self
            .filtered_indices
            .iter()
            .position(|&idx| idx == self.selected_pr)
        {
            self.filtered_position = pos;
        } else if !self.filtered_indices.is_empty() {
            self.filtered_position = 0;
            self.selected_pr = self.filtered_indices[0];
        } else {
            self.filtered_position = 0;
        }
    }

    /// Get filtered reviews based on current filter string
    pub fn filtered_reviews(&self) -> Vec<&PendingReview> {
        self.filtered_indices
            .iter()
            .map(|&idx| &self.reviews[idx])
            .collect()
    }

    /// Get the next refresh duration
    pub fn next_refresh_duration(&self) -> Duration {
        if self.refresh_interval == 0 {
            return Duration::MAX;
        }
        self.last_attempt
            .map(|last| Duration::from_secs(self.refresh_interval).saturating_sub(last.elapsed()))
            .unwrap_or(Duration::ZERO)
    }

    /// Select next PR in the list
    pub fn next_pr(&mut self) {
        if self.filtered_position < self.filtered_indices.len().saturating_sub(1) {
            self.filtered_position += 1;
            self.selected_pr = self.filtered_indices[self.filtered_position];
        }
    }

    /// Select previous PR in the list
    pub fn prev_pr(&mut self) {
        if self.filtered_position > 0 {
            self.filtered_position -= 1;
            self.selected_pr = self.filtered_indices[self.filtered_position];
        }
    }

    /// Set the active tab by index
    pub fn set_active_tab(&mut self, index: usize) {
        if index < self.tabs.len() && self.active_tab != self.tabs[index] {
            self.active_tab = self.tabs[index];
            self.reviews.clear();
            self.selected_pr = 0;
            self.update_filtered_indices();
            self.last_refresh = None;
            self.last_attempt = None;
            self.show_action_menu = false;
        }
    }

    /// Switch to next tab
    pub fn next_tab(&mut self) {
        let current_index = self
            .tabs
            .iter()
            .position(|t| *t == self.active_tab)
            .unwrap_or(0);
        self.set_active_tab((current_index + 1) % self.tabs.len());
    }

    /// Switch to previous tab
    pub fn prev_tab(&mut self) {
        let current_index = self
            .tabs
            .iter()
            .position(|t| *t == self.active_tab)
            .unwrap_or(0);
        let new_index = if current_index == 0 {
            self.tabs.len() - 1
        } else {
            current_index - 1
        };
        self.set_active_tab(new_index);
    }

    /// Get the currently selected PR
    pub fn selected_pr_item(&self) -> Option<&PendingReview> {
        self.reviews.get(self.selected_pr)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn config() -> Config {
        Config {
            github_token: "test-token".into(),
            github_username: "reviewer".into(),
            github_org: "example".into(),
            github_repos: vec![],
            github_teams: vec![],
            crew_members: vec!["teammate".into()],
            anthropic_api_key: None,
            exclude_prefix: vec![],
            max_pr_age_days: Some(60),
        }
    }

    pub(crate) fn review(number: u64) -> PendingReview {
        PendingReview {
            repo: "demo".into(),
            pr_number: number,
            pr_title: format!("Improve feature {number}"),
            pr_author: "teammate".into(),
            pr_url: format!("https://github.com/example/demo/pull/{number}"),
            created_at: chrono::Utc::now(),
            additions: 12,
            deletions: 3,
            draft: false,
            branch: "feature".into(),
        }
    }

    async fn complete(app: &mut App) {
        let (tab, result) = app.loads.join_next().await.unwrap().unwrap();
        app.finish_load(tab, result).await;
    }

    #[tokio::test]
    async fn tab_changes_do_not_wait_for_slow_loads_or_apply_other_tabs() {
        let mut app = App::new(config(), 30).await.unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        app.start_load(Tab::PendingReviews, async move { Ok(receive.await?) });
        app.set_active_tab(1);
        app.refresh().await.unwrap();
        assert_eq!(app.active_tab, Tab::MyPullRequests);
        assert_eq!(app.pending.len(), 2);
        complete(&mut app).await; // Empty My PRs finishes before pending reviews.
        assert!(!app.loading);
        send.send(vec![review(7)]).unwrap();
        complete(&mut app).await;
        assert!(app.reviews.is_empty());
        app.set_active_tab(0);
        app.refresh().await.unwrap();
        assert_eq!(app.reviews[0].pr_number, 7);
    }

    #[tokio::test]
    async fn refresh_deduplicates_running_loads() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.start_load(Tab::PendingReviews, std::future::pending());
        app.refresh().await.unwrap();
        app.force_refresh().await.unwrap();
        assert_eq!(app.loads.len(), 1);
        assert!(app.loading);
    }

    #[tokio::test]
    async fn failures_keep_existing_data_and_schedule_retry() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.set_reviews(vec![review(7)]);
        app.start_load(Tab::PendingReviews, async { anyhow::bail!("offline") });
        complete(&mut app).await;
        assert_eq!(app.reviews[0].pr_number, 7);
        assert!(!app.loading);
        assert!(app.error.as_deref().unwrap().contains("offline"));
        assert!(!app.next_refresh_duration().is_zero());
    }

    #[tokio::test]
    async fn refresh_deadline_expires_and_zero_disables_auto_refresh() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.last_attempt = Some(Instant::now() - Duration::from_secs(31));
        assert!(app.next_refresh_duration().is_zero());
        app.refresh_interval = 0;
        assert_eq!(app.next_refresh_duration(), Duration::MAX);
    }

    #[tokio::test]
    async fn selection_survives_reordering_and_shrinking_results() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.set_reviews(vec![review(1), review(2), review(3)]);
        app.next_pr();
        app.set_reviews(vec![review(2), review(1)]);
        assert_eq!(app.selected_pr_item().unwrap().pr_number, 2);
        app.set_reviews(vec![]);
        assert!(app.selected_pr_item().is_none());
        assert!(app.filtered_indices.is_empty());
    }
}
