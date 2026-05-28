use dashmap::DashMap;
use proviz_core::models::SearchResult;
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct CacheEntry {
    pub results: Vec<SearchResult>,
    pub provider_slug: String,
    pub api_key_id: String,
    pub stored_at: Instant,
    pub ttl: Duration,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        self.stored_at.elapsed() >= self.ttl
    }
}

#[derive(Clone, Default)]
pub struct QueryCache {
    inner: Arc<DashMap<String, CacheEntry>>,
}

impl QueryCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cache_key(
        query: &str,
        language: Option<&str>,
        country: Option<&str>,
        group_slug: Option<&str>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(query.as_bytes());
        hasher.update(b"\x00");
        hasher.update(language.unwrap_or("").as_bytes());
        hasher.update(b"\x00");
        hasher.update(country.unwrap_or("").as_bytes());
        hasher.update(b"\x00");
        hasher.update(group_slug.unwrap_or("").as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        let entry = self.inner.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.inner.remove(key);
            return None;
        }
        Some(entry.clone())
    }

    pub fn set(&self, key: String, entry: CacheEntry) {
        self.inner.insert(key, entry);
    }

    /// Remove expired entries. Call periodically to prevent unbounded growth.
    pub fn evict_expired(&self) {
        self.inner.retain(|_, v| !v.is_expired());
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_inline {
    use super::*;

    #[test]
    fn test_cache_key_deterministic() {
        let k1 = QueryCache::cache_key("rust", Some("en"), Some("us"), None);
        let k2 = QueryCache::cache_key("rust", Some("en"), Some("us"), None);
        assert_eq!(k1, k2);

        let k3 = QueryCache::cache_key("rust", Some("fr"), Some("us"), None);
        assert_ne!(k1, k3);
    }

    #[test]
    fn test_cache_hit_and_evict() {
        let cache = QueryCache::new();
        let key = "testkey".to_string();
        cache.set(
            key.clone(),
            CacheEntry {
                results: vec![],
                provider_slug: "brave".to_string(),
                api_key_id: "id".to_string(),
                stored_at: Instant::now(),
                ttl: Duration::from_secs(3600),
            },
        );
        assert!(cache.get(&key).is_some());
    }
}
