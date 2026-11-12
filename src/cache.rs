//! Lookup cache (1.7). Remembers what a search or link resolved to (titles
//! and watch links, never stream URLs), so asking for the same song or
//! playlist again starts faster and still works if YouTube search hiccups.
//! Kept in memory only.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(6 * 60 * 60);
pub const CAPACITY: usize = 300;

pub struct Cache<V> {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, (Instant, V)>>,
}

/// Same words, any case or spacing, are the same lookup. Links keep their
/// case: YouTube IDs like `AbC` and `abc` are different videos.
pub fn key(kind: &str, query: &str, limit: usize) -> String {
    let words: Vec<&str> = query.split_whitespace().collect();
    let joined = words.join(" ");
    let text = if joined.contains("://") { joined } else { joined.to_lowercase() };
    format!("{kind}|{limit}|{text}")
}

impl<V: Clone> Cache<V> {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self { ttl, capacity: capacity.max(1), entries: Mutex::new(HashMap::new()) }
    }

    pub fn get_at(&self, key: &str, now: Instant) -> Option<V> {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        match entries.get(key) {
            Some((at, value)) if now.saturating_duration_since(*at) < self.ttl => Some(value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn put_at(&self, key: String, value: V, now: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.retain(|_, (at, _)| now.saturating_duration_since(*at) < self.ttl);
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            if let Some(oldest) = entries.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (now, value));
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.get_at(key, Instant::now())
    }
    pub fn put(&self, key: String, value: V) {
        self.put_at(key, value, Instant::now())
    }
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Resolved links and searches.
pub fn items() -> &'static Cache<Vec<crate::media::Item>> {
    static C: std::sync::OnceLock<Cache<Vec<crate::media::Item>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Cache::new(TTL, CAPACITY))
}

/// Search result pickers.
pub fn hits() -> &'static Cache<Vec<crate::media::Hit>> {
    static C: std::sync::OnceLock<Cache<Vec<crate::media::Hit>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Cache::new(TTL, CAPACITY))
}

/// `media::resolve` through the cache. Failures and empty results aren't kept.
pub async fn resolve(query: &str, limit: usize, allow_private: bool) -> Result<Vec<crate::media::Item>, String> {
    let k = key(if allow_private { "resolve-owner" } else { "resolve" }, query, limit);
    if let Some(found) = items().get(&k) {
        return Ok(found);
    }
    let found = crate::media::resolve(query, limit, allow_private).await?;
    if !found.is_empty() && !found.iter().any(|i| i.kind == crate::media::Kind::File) {
        items().put(k, found.clone());
    }
    Ok(found)
}

pub async fn youtube_hits(query: &str, limit: usize) -> Result<Vec<crate::media::Hit>, String> {
    let k = key("hits", query, limit);
    if let Some(found) = hits().get(&k) {
        return Ok(found);
    }
    let found = crate::media::youtube_hits(query, limit).await?;
    if !found.is_empty() {
        hits().put(k, found.clone());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_that_differ_only_in_case_are_different_songs() {
        assert_ne!(key("resolve", "https://youtu.be/AbCdEfGhIjK", 1), key("resolve", "https://youtu.be/abcdefghijk", 1));
    }

    #[test]
    fn keys_ignore_case_and_spacing() {
        assert_eq!(key("r", "  Never  Gonna ", 25), key("r", "never gonna", 25));
        assert_ne!(key("r", "a", 25), key("r", "a", 10));
    }

    #[test]
    fn entries_expire_and_oldest_is_evicted() {
        let cache: Cache<u32> = Cache::new(Duration::from_secs(10), 2);
        let t0 = Instant::now();
        cache.put_at("a".into(), 1, t0);
        cache.put_at("b".into(), 2, t0 + Duration::from_secs(1));
        assert_eq!(cache.get_at("a", t0 + Duration::from_secs(5)), Some(1));
        cache.put_at("c".into(), 3, t0 + Duration::from_secs(2));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get_at("a", t0 + Duration::from_secs(2)), None);
        assert_eq!(cache.get_at("c", t0 + Duration::from_secs(11)), Some(3));
        assert_eq!(cache.get_at("b", t0 + Duration::from_secs(11)), None);
    }
}
