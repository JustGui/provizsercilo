use dashmap::DashMap;
use proviz_core::models::{ExtraSnippet, FullContent, SearchResult};
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

    /// `enrichment` is an opaque discriminator (e.g. built by the caller from
    /// extra_snippets/full_content/max_snippets/min_score/domain filters) folded
    /// into the key so a plain-search cache entry is never served back for an
    /// enriched request, or vice versa. Pass "" when enrichment isn't in play.
    pub fn cache_key(
        query: &str,
        language: Option<&str>,
        country: Option<&str>,
        group_slug: Option<&str>,
        enrichment: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(query.as_bytes());
        hasher.update(b"\x00");
        hasher.update(language.unwrap_or("").as_bytes());
        hasher.update(b"\x00");
        hasher.update(country.unwrap_or("").as_bytes());
        hasher.update(b"\x00");
        hasher.update(group_slug.unwrap_or("").as_bytes());
        hasher.update(b"\x00");
        hasher.update(enrichment.as_bytes());
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

/// URL-keyed store for enrichment content (`full_content` / `extra_snippets`)
/// pulled from Tavily/Staan. Independent of `QueryCache`: a page fetched once
/// stays reusable across *any* future query that happens to surface the same
/// URL, not just an identical repeat of the same query - and it outlives the
/// (short) SERP cache TTL since page content changes far less than rankings.
#[derive(Clone)]
pub struct DocEntry {
    pub full_content: Option<FullContent>,
    pub extra_snippets: Option<Vec<ExtraSnippet>>,
    pub stored_at: Instant,
    pub ttl: Duration,
}

impl DocEntry {
    fn is_expired(&self) -> bool {
        self.stored_at.elapsed() >= self.ttl
    }
}

#[derive(Clone, Default)]
pub struct DocCache {
    inner: Arc<DashMap<String, DocEntry>>,
}

impl DocCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize a URL for use as a cache key: strip the fragment (never
    /// affects page content) and a trailing slash, so trivially-different
    /// URLs pointing at the same document still share one cache entry.
    fn normalize(url: &str) -> String {
        let without_fragment = url.split('#').next().unwrap_or(url);
        without_fragment.trim_end_matches('/').to_string()
    }

    pub fn get(&self, url: &str) -> Option<DocEntry> {
        let key = Self::normalize(url);
        let entry = self.inner.get(&key)?;
        if entry.is_expired() {
            drop(entry);
            self.inner.remove(&key);
            return None;
        }
        Some(entry.clone())
    }

    /// Store enrichment content for a URL. No-op when both fields are empty -
    /// nothing worth remembering.
    pub fn set(
        &self,
        url: &str,
        full_content: Option<FullContent>,
        extra_snippets: Option<Vec<ExtraSnippet>>,
        ttl: Duration,
    ) {
        if full_content.is_none() && extra_snippets.is_none() {
            return;
        }
        self.inner.insert(
            Self::normalize(url),
            DocEntry {
                full_content,
                extra_snippets,
                stored_at: Instant::now(),
                ttl,
            },
        );
    }

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
        let k1 = QueryCache::cache_key("rust", Some("en"), Some("us"), None, "");
        let k2 = QueryCache::cache_key("rust", Some("en"), Some("us"), None, "");
        assert_eq!(k1, k2);

        let k3 = QueryCache::cache_key("rust", Some("fr"), Some("us"), None, "");
        assert_ne!(k1, k3);

        let k4 = QueryCache::cache_key(
            "rust",
            Some("en"),
            Some("us"),
            None,
            "full_content=markdown",
        );
        assert_ne!(k1, k4, "enrichment must be folded into the key");
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
