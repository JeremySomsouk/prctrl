//! Selected-PR loads are debounced, cancellable and bounded in memory.
use crate::readiness::{Readiness, ReadinessClient};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

type Key = (String, u64);
type CacheKey = (String, u64, Option<String>);
const TTL: Duration = Duration::from_secs(60);
const DEBOUNCE: Duration = Duration::from_millis(250);
const CAPACITY: usize = 64;

pub struct ReadinessDesk {
    client: ReadinessClient,
    owner: String,
    cache: HashMap<CacheKey, (Readiness, Instant)>,
    selected: Option<Key>,
    settled_since: Instant,
    pending: Option<Key>,
    generation: u64,
    pub(crate) loads: JoinSet<(u64, Key, Readiness)>,
}
impl ReadinessDesk {
    pub(crate) fn new(client: ReadinessClient, owner: String) -> Self {
        Self {
            client,
            owner,
            cache: HashMap::new(),
            selected: None,
            settled_since: Instant::now(),
            pending: None,
            generation: 0,
            loads: JoinSet::new(),
        }
    }
    pub(crate) fn select(&mut self, key: Option<Key>) {
        if self.selected != key {
            self.generation = self.generation.wrapping_add(1);
            self.loads.abort_all();
            self.pending = None;
            self.selected = key;
            self.settled_since = Instant::now();
        }
        let Some(key) = self.selected.clone() else {
            return;
        };
        if self.settled_since.elapsed() < DEBOUNCE
            || self.pending.is_some()
            || self.observation(&key.0, key.1).is_some()
        {
            return;
        }
        self.pending = Some(key.clone());
        let client = self.client.clone();
        let owner = self.owner.clone();
        let generation = self.generation;
        self.loads.spawn(async move {
            let result = client.fetch(&owner, &key.0, key.1).await;
            (generation, key, result)
        });
    }
    pub fn observation(&self, repo: &str, number: u64) -> Option<&Readiness> {
        self.cache
            .iter()
            .find_map(|((cached_repo, cached_number, _), (data, at))| {
                (cached_repo == repo && *cached_number == number && at.elapsed() < TTL)
                    .then_some(data)
            })
    }
    pub(crate) fn finish(&mut self, generation: u64, key: Key, data: Readiness) {
        if generation != self.generation || self.pending.as_ref() != Some(&key) {
            return;
        }
        self.pending = None;
        self.cache
            .retain(|(repo, number, _), _| repo != &key.0 || *number != key.1);
        if self.cache.len() >= CAPACITY {
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            {
                self.cache.remove(&oldest);
            }
        }
        self.cache.insert(
            (key.0, key.1, data.head_sha.clone()),
            (data, Instant::now()),
        );
    }
    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.loads.abort_all();
        self.pending = None;
        self.selected = None;
        self.cache.clear();
    }
    #[cfg(test)]
    pub(crate) fn fixture(&mut self, key: Key, data: Readiness) {
        self.pending = Some(key.clone());
        self.finish(self.generation, key, data);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::ReviewClient;
    fn desk() -> ReadinessDesk {
        ReadinessDesk::new(
            ReadinessClient::new(ReviewClient::new("test", true)),
            "example".into(),
        )
    }
    #[tokio::test]
    async fn rapid_navigation_and_old_generations_do_not_load_or_overwrite() {
        let mut desk = desk();
        let a = ("demo".into(), 1);
        let b = ("demo".into(), 2);
        desk.select(Some(a.clone()));
        let old_generation = desk.generation;
        desk.select(Some(b));
        desk.select(Some(a.clone()));
        assert!(desk.loads.is_empty());
        desk.pending = Some(a.clone());
        desk.finish(old_generation, a, Readiness::unknown("old"));
        assert!(desk.cache.is_empty());
    }
    #[tokio::test]
    async fn cache_expires_invalidates_and_stays_bounded() {
        let mut desk = desk();
        for number in 0..1000 {
            desk.fixture(("demo".into(), number), Readiness::unknown("fixture"));
        }
        assert_eq!(desk.cache.len(), CAPACITY);
        let key = desk.cache.keys().next().unwrap().clone();
        desk.cache.get_mut(&key).unwrap().1 = Instant::now() - TTL;
        assert!(desk.observation(&key.0, key.1).is_none());
        desk.invalidate();
        assert!(desk.cache.is_empty());
    }
    #[tokio::test]
    async fn settled_selection_deduplicates_and_cancels() {
        let mut desk = desk();
        let key = ("demo".into(), 1);
        desk.select(Some(key.clone()));
        desk.settled_since = Instant::now() - DEBOUNCE;
        desk.select(Some(key.clone()));
        desk.select(Some(key));
        assert_eq!(desk.loads.len(), 1);
        desk.invalidate();
        assert!(desk
            .loads
            .join_next()
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled());
    }
}
