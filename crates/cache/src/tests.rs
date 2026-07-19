use std::time::{Duration, Instant};

use crate::{CacheEntry, DocCache, QueryCache};
use proviz_core::models::FullContent;

#[test]
fn test_cache_key_stable_across_calls() {
    let k1 = QueryCache::cache_key(
        "rust programming",
        Some("en"),
        Some("us"),
        Some("mygroup"),
        "",
    );
    let k2 = QueryCache::cache_key(
        "rust programming",
        Some("en"),
        Some("us"),
        Some("mygroup"),
        "",
    );
    assert_eq!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_query() {
    let k1 = QueryCache::cache_key("rust", Some("en"), None, None, "");
    let k2 = QueryCache::cache_key("python", Some("en"), None, None, "");
    assert_ne!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_language() {
    let k1 = QueryCache::cache_key("rust", Some("en"), None, None, "");
    let k2 = QueryCache::cache_key("rust", Some("fr"), None, None, "");
    assert_ne!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_group() {
    let k1 = QueryCache::cache_key("rust", None, None, Some("group-a"), "");
    let k2 = QueryCache::cache_key("rust", None, None, Some("group-b"), "");
    assert_ne!(k1, k2);
}

#[test]
fn test_cache_key_differs_by_enrichment() {
    let k1 = QueryCache::cache_key("rust", None, None, None, "");
    let k2 = QueryCache::cache_key("rust", None, None, None, "extra_snippets=true");
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

#[test]
fn test_doc_cache_set_and_get_hit() {
    let cache = DocCache::new();
    cache.set(
        "https://example.com/article",
        Some(FullContent {
            text: "body".into(),
            format: "markdown".into(),
            length: 4,
        }),
        None,
        Duration::from_secs(3600),
    );
    let entry = cache.get("https://example.com/article").unwrap();
    assert_eq!(entry.full_content.unwrap().text, "body");
}

#[test]
fn test_doc_cache_normalizes_trailing_slash_and_fragment() {
    let cache = DocCache::new();
    cache.set(
        "https://example.com/article/",
        Some(FullContent {
            text: "body".into(),
            format: "markdown".into(),
            length: 4,
        }),
        None,
        Duration::from_secs(3600),
    );
    assert!(cache.get("https://example.com/article#section-2").is_some());
}

#[test]
fn test_doc_cache_skips_empty_entry() {
    let cache = DocCache::new();
    cache.set(
        "https://example.com/x",
        None,
        None,
        Duration::from_secs(3600),
    );
    assert!(cache.is_empty());
}

#[test]
fn test_doc_cache_expired_entry_not_returned() {
    let cache = DocCache::new();
    let key = "https://example.com/stale";
    cache.inner.insert(
        crate::DocCache::normalize(key),
        crate::DocEntry {
            full_content: Some(FullContent {
                text: "body".into(),
                format: "markdown".into(),
                length: 4,
            }),
            extra_snippets: None,
            stored_at: Instant::now() - Duration::from_secs(7200),
            ttl: Duration::from_secs(3600),
        },
    );
    assert!(cache.get(key).is_none());
}
