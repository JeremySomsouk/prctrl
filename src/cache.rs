use crate::github::PendingReview;
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Cache key for identifying cached data
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CacheKey {
    pub org: String,
    pub repos: Vec<String>,
    pub username: String,
    pub tab_type: String,
    pub include_mine: bool,
    pub include_drafts: bool,
    pub exclude_prefixes: Vec<String>,
    pub crew_members: Vec<String>,
    pub max_age_days: Option<u32>,
}

/// Builder for CacheKey to avoid too many constructor arguments
#[derive(Debug, Default)]
pub struct CacheKeyBuilder {
    org: Option<String>,
    repos: Option<Vec<String>>,
    username: Option<String>,
    tab_type: Option<String>,
    include_mine: Option<bool>,
    include_drafts: Option<bool>,
    exclude_prefixes: Option<Vec<String>>,
    crew_members: Option<Vec<String>>,
    max_age_days: Option<u32>,
}

impl CacheKeyBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn org(mut self, org: impl Into<String>) -> Self {
        self.org = Some(org.into());
        self
    }

    pub fn repos(mut self, repos: &[String]) -> Self {
        self.repos = Some(repos.to_vec());
        self
    }

    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    pub fn tab_type(mut self, tab_type: impl Into<String>) -> Self {
        self.tab_type = Some(tab_type.into());
        self
    }

    pub fn include_mine(mut self, include_mine: bool) -> Self {
        self.include_mine = Some(include_mine);
        self
    }

    pub fn include_drafts(mut self, include_drafts: bool) -> Self {
        self.include_drafts = Some(include_drafts);
        self
    }

    pub fn exclude_prefixes(mut self, exclude_prefixes: &[String]) -> Self {
        self.exclude_prefixes = Some(exclude_prefixes.to_vec());
        self
    }

    pub fn crew_members(mut self, crew_members: &[String]) -> Self {
        self.crew_members = Some(crew_members.to_vec());
        self
    }

    pub fn max_age_days(mut self, max_age_days: Option<u32>) -> Self {
        self.max_age_days = max_age_days;
        self
    }

    pub fn build(self) -> CacheKey {
        CacheKey {
            org: self.org.expect("org must be set"),
            repos: self.repos.expect("repos must be set"),
            username: self.username.expect("username must be set"),
            tab_type: self.tab_type.expect("tab_type must be set"),
            include_mine: self.include_mine.unwrap_or(false),
            include_drafts: self.include_drafts.unwrap_or(false),
            exclude_prefixes: self.exclude_prefixes.unwrap_or_default(),
            crew_members: self.crew_members.unwrap_or_default(),
            max_age_days: self.max_age_days,
        }
    }
}

/// Cached data with metadata
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub data: Vec<PendingReview>,
    pub cached_at: DateTime<Utc>,
    pub ttl_seconds: u64,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        let now = Utc::now();
        let elapsed = now - self.cached_at;
        elapsed.num_seconds() > self.ttl_seconds as i64
    }
}

/// Thread-safe cache for PR data
#[derive(Debug, Clone)]
pub struct PrCache {
    inner: Arc<RwLock<CacheInner>>,
}

#[derive(Debug)]
struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
    default_ttl_seconds: u64,
}

impl PrCache {
    /// Create a new cache with default TTL of 60 seconds
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheInner {
                entries: HashMap::new(),
                default_ttl_seconds: 60,
            })),
        }
    }

    /// Create a new cache with custom TTL
    pub fn with_ttl(ttl_seconds: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheInner {
                entries: HashMap::new(),
                default_ttl_seconds: ttl_seconds,
            })),
        }
    }

    /// Get cached data if it exists and is not expired
    pub async fn get(&self, key: &CacheKey) -> Option<Vec<PendingReview>> {
        let inner = self.inner.read().await;
        inner.entries.get(key).and_then(|entry| {
            if entry.is_expired() {
                None
            } else {
                Some(entry.data.clone())
            }
        })
    }

    /// Set cached data with default TTL
    pub async fn set(&self, key: CacheKey, data: Vec<PendingReview>) {
        let mut inner = self.inner.write().await;
        let entry = CacheEntry {
            data,
            cached_at: Utc::now(),
            ttl_seconds: inner.default_ttl_seconds,
        };
        inner.entries.insert(key, entry);
    }

    /// Set cached data with custom TTL
    pub async fn set_with_ttl(&self, key: CacheKey, data: Vec<PendingReview>, ttl_seconds: u64) {
        let mut inner = self.inner.write().await;
        let entry = CacheEntry {
            data,
            cached_at: Utc::now(),
            ttl_seconds,
        };
        inner.entries.insert(key, entry);
    }

    /// Invalidate cache for a specific key
    pub async fn invalidate(&self, key: &CacheKey) {
        let mut inner = self.inner.write().await;
        inner.entries.remove(key);
    }

    /// Clear all cache entries
    pub async fn clear(&self) {
        let mut inner = self.inner.write().await;
        inner.entries.clear();
    }

    /// Invalidate all entries older than a certain duration
    pub async fn invalidate_old(&self, max_age: Duration) {
        let mut inner = self.inner.write().await;
        let now = Utc::now();
        inner.entries.retain(|_, entry| {
            let elapsed = now - entry.cached_at;
            elapsed < max_age
        });
    }

    /// Get cache statistics
    pub async fn stats(&self) -> (usize, usize) {
        let inner = self.inner.read().await;
        let total = inner.entries.len();
        let valid = inner.entries.values().filter(|e| !e.is_expired()).count();
        (total, valid)
    }
}

impl Default for PrCache {
    fn default() -> Self {
        Self::new()
    }
}

// Global cache instance for convenience
pub static GLOBAL_CACHE: Lazy<PrCache> = Lazy::new(|| PrCache::with_ttl(60));
