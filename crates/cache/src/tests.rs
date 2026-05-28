use std::time::{Duration, Instant};

use crate::{CacheEntry, QueryCache};

#[test]
fn test_cache_key_stable_across_calls() {
    let k1 = QueryCache::cache_key("rust programming", Some("en"), Some("us"), Some("mygroup"));
    let k2 = QueryCache::cache_key("rust programming", Some("en"), Some("us"), Some("mygroup"));
    assert_eq!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_query() {
    let k1 = QueryCache::cache_key("rust", Some("en"), None, None);
    let k2 = QueryCache::cache_key("python", Some("en"), None, None);
    assert_ne!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_language() {
    let k1 = QueryCache::cache_key("rust", Some("en"), None, None);
    let k2 = QueryCache::cache_key("rust", Some("fr"), None, None);
    assert_ne!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_group() {
    let k1 = QueryCache::cache_key("rust", None, None, Some("group-a"));
    let k2 = QueryCache::cache_key("rust", None, None, Some("group-b"));
    assert_ne!(k1, k2);
}

#[test]
fn test_set_and_get_hit() {
    let cache = QueryCache::new();
    let key = "testkey".to_string();
    cache.set(
        key.clone(),
        CacheEntry {
            results: vec![],
            provider_slug: "brave".into(),
            api_key_id: "key-id".into(),
            stored_at: Instant::now(),
            ttl: Duration::from_secs(3600),
        },
    );
    let entry = cache.get(&key).unwrap();
    assert_eq!(entry.provider_slug, "brave");
}

#[test]
fn test_expired_entry_not_returned() {
    let cache = QueryCache::new();
    let key = "expired-key".to_string();
    cache.set(
        key.clone(),
        CacheEntry {
            results: vec![],
            provider_slug: "brave".into(),
            api_key_id: "key-id".into(),
            stored_at: Instant::now() - Duration::from_secs(7200),
            ttl: Duration::from_secs(3600), // expired 1h ago
        },
    );
    assert!(cache.get(&key).is_none());
}

#[test]
fn test_evict_expired_clears_cache() {
    let cache = QueryCache::new();
    cache.set(
        "fresh".into(),
        CacheEntry {
            results: vec![],
            provider_slug: "brave".into(),
            api_key_id: "k".into(),
            stored_at: Instant::now(),
            ttl: Duration::from_secs(3600),
        },
    );
    cache.set(
        "stale".into(),
        CacheEntry {
            results: vec![],
            provider_slug: "tavily".into(),
            api_key_id: "k".into(),
            stored_at: Instant::now() - Duration::from_secs(7200),
            ttl: Duration::from_secs(3600),
        },
    );
    assert_eq!(cache.len(), 2);
    cache.evict_expired();
    assert_eq!(cache.len(), 1);
}
