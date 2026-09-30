use crate::config::Config;
use crate::github::{
    fetch_my_open_prs_with_client, fetch_pending_reviews_with_client, PendingReview, ReviewClient,
};
use anyhow::Result;
use chrono::DateTime;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
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
    pub reviews: Arc<[PendingReview]>,
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
    snapshots: HashMap<Tab, Snapshot>,
    transport: ReviewClient,
    /// Current filtered indices (cached for performance)
    pub filtered_indices: Vec<usize>,
    /// Current position in filtered list
    pub filtered_position: usize,
    pub(crate) loads: JoinSet<(Tab, Result<Vec<PendingReview>>)>,
    pending: HashSet<Tab>,
    last_attempt: Option<Instant>,
}

struct Snapshot {
    reviews: Arc<[PendingReview]>,
    fetched_at: DateTime<chrono::Utc>,
    loaded_at: Instant,
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
        let transport = ReviewClient::new(&config.github_token, true);
        Ok(Self {
            config,
            transport,
            reviews: Arc::from([]),
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
            snapshots: HashMap::new(),
            filtered_indices: vec![],
            filtered_position: 0,
            loads: JoinSet::new(),
            pending: HashSet::new(),
            last_attempt: None,
        })
    }

    /// Views with identical filters share a snapshot and an in-flight request.
    fn dataset(&self, tab: Tab) -> Tab {
        match tab {
            Tab::Statistics | Tab::MonitorLive => Tab::PendingReviews,
            Tab::Crew if self.config.crew_members.is_empty() => Tab::PendingReviews,
            _ => tab,
        }
    }

    async fn fetch_reviews_for_tab(
        transport: &ReviewClient,
        config: &Config,
        tab: Tab,
    ) -> Result<Vec<PendingReview>> {
        if tab == Tab::MyPullRequests {
            fetch_my_open_prs_with_client(
                transport,
                &config.github_org,
                &config.github_repos,
                &config.github_username,
                true,
                &config.exclude_prefix,
                config.max_pr_age_days,
            )
            .await
        } else {
            let crew = if tab == Tab::Crew {
                config.crew_members.as_slice()
            } else {
                &[]
            };
            fetch_pending_reviews_with_client(
                transport,
                &config.github_org,
                &config.github_repos,
                &config.github_username,
                &config.github_teams,
                false,
                false,
                &config.exclude_prefix,
                crew,
                config.max_pr_age_days,
            )
            .await
        }
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
        let dataset = self.dataset(tab);
        let active = dataset == self.dataset(self.active_tab);
        if active {
            self.error = None;
            self.info = None;
        }
        if let Some(snapshot) = self.snapshots.get(&dataset) {
            let reviews = snapshot.reviews.clone();
            let fetched_at = snapshot.fetched_at;
            let loaded_at = snapshot.loaded_at;
            if active {
                self.set_reviews(reviews);
                self.last_refresh = Some(fetched_at);
                self.last_attempt = Some(loaded_at);
                self.loading = self.pending.contains(&dataset);
            }
            if !force && tab != Tab::MonitorLive && loaded_at.elapsed() < Duration::from_secs(60) {
                return;
            }
        }
        if active {
            self.loading = true;
        }
        if self.pending.contains(&dataset) {
            return;
        }
        let config = self.config.clone();
        let transport = self.transport.clone();
        self.start_load(dataset, async move {
            tokio::time::timeout(
                Duration::from_secs(30),
                Self::fetch_reviews_for_tab(&transport, &config, dataset),
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
        let active = tab == self.dataset(self.active_tab);
        match result {
            Ok(reviews) => {
                let reviews: Arc<[PendingReview]> = reviews.into();
                let fetched_at = chrono::Utc::now();
                self.snapshots.insert(
                    tab,
                    Snapshot {
                        reviews: reviews.clone(),
                        fetched_at,
                        loaded_at: Instant::now(),
                    },
                );
                if active {
                    self.set_reviews(reviews);
                    self.last_refresh = Some(fetched_at);
                }
            }
            Err(error) if active => self.error = Some(format!("{error:#}")),
            Err(_) => {}
        }
        if active {
            self.loading = false;
            self.last_attempt = Some(Instant::now());
        }
    }

    fn set_reviews(&mut self, reviews: Arc<[PendingReview]>) {
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
        for tab in [Tab::PendingReviews, Tab::MyPullRequests, Tab::Crew] {
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
            self.reviews = Arc::from([]);
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
        app.set_reviews(vec![review(7)].into());
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
        app.set_reviews(vec![review(1), review(2), review(3)].into());
        app.next_pr();
        app.set_reviews(vec![review(2), review(1)].into());
        assert_eq!(app.selected_pr_item().unwrap().pr_number, 2);
        app.set_reviews(Arc::from([]));
        assert!(app.selected_pr_item().is_none());
        assert!(app.filtered_indices.is_empty());
    }
    #[tokio::test]
    async fn related_views_share_in_flight_load_and_completed_snapshot() {
        let mut app = App::new(config(), 30).await.unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        app.start_load(Tab::PendingReviews, async move { Ok(receive.await?) });
        app.set_active_tab(3);
        app.refresh().await.unwrap();
        app.set_active_tab(4);
        app.refresh().await.unwrap();
        assert_eq!(app.loads.len(), 1);
        send.send(vec![review(7)]).unwrap();
        complete(&mut app).await;
        let shared = app.reviews.clone();
        app.set_active_tab(0);
        app.refresh().await.unwrap();
        assert!(Arc::ptr_eq(&shared, &app.reviews));
        assert!(app.loads.is_empty());
        assert!(app.last_refresh.is_some());
    }

    #[tokio::test]
    async fn preloads_are_published_independently_and_live_is_not_preloaded() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.start_load(Tab::Crew, std::future::pending());
        app.start_load(Tab::PendingReviews, async { Ok(vec![review(7)]) });
        complete(&mut app).await;
        assert_eq!(app.reviews[0].pr_number, 7);
        assert_eq!(app.loads.len(), 1);
        app.preload_all_tabs().await.unwrap();
        assert_eq!(app.loads.len(), 2); // Crew and My PRs only.
    }

    #[tokio::test]
    async fn expired_snapshots_stay_visible_while_refreshing() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.finish_load(Tab::PendingReviews, Ok(vec![review(7)]))
            .await;
        app.snapshots
            .get_mut(&Tab::PendingReviews)
            .unwrap()
            .loaded_at = Instant::now() - Duration::from_secs(61);
        app.refresh().await.unwrap();
        assert_eq!(app.reviews[0].pr_number, 7);
        assert!(app.loading);
        assert_eq!(app.loads.len(), 1);
    }
}
