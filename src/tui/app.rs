use crate::cache::{CacheKey, CacheKeyBuilder, PrCache};
use crate::config::Config;
use crate::github::{fetch_my_open_prs, fetch_pending_reviews, PendingReview};
use anyhow::Result;
use chrono::DateTime;
use std::time::Duration;

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
}

/// Represents a tab/command in the left sidebar
#[derive(Debug, Clone, PartialEq)]
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
    fn get_cache_key(&self) -> CacheKey {
        let crew_members: Vec<String> = match self.active_tab {
            Tab::Crew => self.config.crew_members.clone(),
            _ => vec![],
        };

        let tab_type = match self.active_tab {
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

    /// Refresh the data based on current tab
    pub async fn refresh(&mut self) -> Result<()> {
        self.loading = true;
        self.error = None;
        self.info = None;

        // Monitor Live tab always fetches fresh data (bypasses cache)
        if self.active_tab != Tab::MonitorLive {
            let cache_key = self.get_cache_key();

            // Try to get from cache first
            if let Some(cached_reviews) = self.cache.get(&cache_key).await {
                self.reviews = cached_reviews;
                self.loading = false;
                self.last_refresh = Some(chrono::Utc::now());
                self.selected_pr = self.selected_pr.min(self.reviews.len().saturating_sub(1));
                self.update_filtered_indices();
                return Ok(());
            }
        }

        // For Crew tab, pass crew_members from config; for other tabs, pass empty vec
        let crew_members: Vec<String> = match self.active_tab {
            Tab::Crew => self.config.crew_members.clone(),
            _ => vec![],
        };

        self.reviews =
            Self::fetch_reviews_for_tab(&self.config, &self.active_tab, &crew_members).await?;

        // Cache the results (except for Monitor Live tab)
        if self.active_tab != Tab::MonitorLive {
            let cache_key = self.get_cache_key();
            self.cache.set(cache_key, self.reviews.clone()).await;
        }

        // Update filtered indices after reviews change
        self.update_filtered_indices();

        self.loading = false;
        self.last_refresh = Some(chrono::Utc::now());
        self.selected_pr = self.selected_pr.min(self.reviews.len().saturating_sub(1));

        Ok(())
    }

    /// Force refresh - clears cache for current tab and reloads
    pub async fn force_refresh(&mut self) -> Result<()> {
        // Clear cache for current tab
        let cache_key = self.get_cache_key();
        self.cache.invalidate(&cache_key).await;

        // Reload
        self.refresh().await
    }

    /// Clear all cache and reload current tab
    pub async fn clear_cache_and_refresh(&mut self) -> Result<()> {
        // Clear all cache
        self.cache.clear().await;

        // Reload current tab
        self.refresh().await
    }

    /// Check if cache is empty and reload if needed
    pub async fn refresh_if_cache_empty(&mut self) -> Result<()> {
        let cache_key = self.get_cache_key();

        // Check if cache has data for current tab
        if self.cache.get(&cache_key).await.is_none() {
            // Cache is empty/missing for this tab, reload
            self.refresh().await?;
        }

        Ok(())
    }

    /// Preload all tabs asynchronously and populate the cache
    /// This is called during initial launch to load all tab data in parallel
    pub async fn preload_all_tabs(&self) -> Result<()> {
        use futures::future::join_all;

        let tabs = Tab::all();
        let mut futures = Vec::new();

        for tab in &tabs {
            let config = self.config.clone();
            let cache = self.cache.clone();
            let crew_members = match tab {
                Tab::Crew => config.crew_members.clone(),
                _ => vec![],
            };

            // Get cache key for this tab
            let tab_type = match tab {
                Tab::PendingReviews => "pending_reviews",
                Tab::MyPullRequests => "my_prs",
                Tab::Crew => "crew",
                Tab::Statistics => "statistics",
                Tab::MonitorLive => "monitor_live",
            };

            let cache_key = CacheKeyBuilder::new()
                .org(&config.github_org)
                .repos(&config.github_repos)
                .username(&config.github_username)
                .tab_type(tab_type)
                .include_mine(false)
                .include_drafts(false)
                .exclude_prefixes(&config.exclude_prefix)
                .crew_members(&crew_members)
                .max_age_days(config.max_pr_age_days)
                .build();

            // Check if already cached
            if cache.get(&cache_key).await.is_some() {
                continue; // Skip if already cached
            }

            // Spawn a future to load this tab's data
            let future = async move {
                match tab {
                    Tab::PendingReviews => {
                        fetch_pending_reviews(
                            &config.github_token,
                            &config.github_org,
                            &config.github_repos,
                            &config.github_username,
                            &config.github_teams,
                            false,
                            false,
                            &config.exclude_prefix,
                            &[], // No crew filter for pending reviews
                            config.max_pr_age_days,
                        )
                        .await
                    }
                    Tab::MyPullRequests => {
                        fetch_my_open_prs(
                            &config.github_token,
                            &config.github_org,
                            &config.github_repos,
                            &config.github_username,
                            true,
                            &config.exclude_prefix,
                            config.max_pr_age_days,
                        )
                        .await
                    }
                    Tab::Crew => {
                        fetch_pending_reviews(
                            &config.github_token,
                            &config.github_org,
                            &config.github_repos,
                            &config.github_username,
                            &config.github_teams,
                            false,
                            false,
                            &config.exclude_prefix,
                            &config.crew_members,
                            config.max_pr_age_days,
                        )
                        .await
                    }
                    Tab::Statistics => {
                        fetch_pending_reviews(
                            &config.github_token,
                            &config.github_org,
                            &config.github_repos,
                            &config.github_username,
                            &config.github_teams,
                            false,
                            false,
                            &config.exclude_prefix,
                            &[],
                            config.max_pr_age_days,
                        )
                        .await
                    }
                    Tab::MonitorLive => {
                        fetch_pending_reviews(
                            &config.github_token,
                            &config.github_org,
                            &config.github_repos,
                            &config.github_username,
                            &config.github_teams,
                            false,
                            false,
                            &config.exclude_prefix,
                            &[],
                            config.max_pr_age_days,
                        )
                        .await
                    }
                }
            };

            // Store the future along with its cache key
            futures.push((cache_key, future));
        }

        // Execute all futures concurrently
        let results = join_all(futures.into_iter().map(|(cache_key, future)| async move {
            match future.await {
                Ok(reviews) => Some((cache_key, reviews)),
                Err(e) => {
                    eprintln!("Warning: Failed to preload tab: {}", e);
                    None
                }
            }
        }))
        .await;

        // Cache all successful results
        for (cache_key, reviews) in results.into_iter().flatten() {
            self.cache.set(cache_key, reviews).await;
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
        let interval = Duration::from_secs(self.refresh_interval);

        if let Some(last) = self.last_refresh {
            let elapsed = last.signed_duration_since(chrono::Utc::now());
            let remaining = interval.saturating_sub(elapsed.to_std().unwrap_or(Duration::ZERO));
            return remaining;
        }

        Duration::ZERO
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
        if index < self.tabs.len() {
            self.active_tab = self.tabs[index].clone();
            // Refresh data when switching tabs
            // Note: In the actual TUI, we'd trigger a refresh here
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
