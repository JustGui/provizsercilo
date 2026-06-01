use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{extract::State, Json};
use cache::{CacheEntry, QueryCache};
use serde::{Deserialize, Serialize};
use tracing::info;

use proviz_core::{models::SearchResult, selector::DebugDecision};

use crate::{
    app::AppState,
    error::AppError,
    executor::{AttemptRecord, SearchParams},
};

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub language: Option<String>,
    pub country: Option<String>,
    pub group: Option<String>,
    #[serde(default = "default_n")]
    pub n: usize,
    #[serde(default)]
    pub exclude_providers: Vec<String>,
    #[serde(default)]
    pub exclude_key_ids: Vec<String>,
    pub max_fallbacks: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub cache_ttl_secs: Option<u64>,
    #[serde(default)]
    pub debug: bool,
}

fn default_n() -> usize {
    10
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub meta: SearchMeta,
    pub debug: Option<Vec<DebugDecision>>,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Serialize)]
pub struct SearchMeta {
    pub provider: String,
    pub api_key_id: String,
    pub fallback_chain: String,
    pub cache_hit: bool,
    pub duration_ms: u64,
    pub n_returned: usize,
}

pub async fn handle_search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let n = req.n.clamp(1, 50);
    let cache_ttl = req.cache_ttl_secs.unwrap_or(state.config.cache_ttl_secs);
    let timeout_ms = req.timeout_ms.unwrap_or(8000);
    let max_fallbacks = req.max_fallbacks.unwrap_or(state.config.max_fallbacks);

    // Determine whether all providers in the requested group are no-cache.
    // If so, skip the cache read (and also the write, handled below).
    let group_all_no_cache = {
        let catalog = state.catalog.read().await;
        let candidates = catalog.candidates(req.group.as_deref());
        !candidates.is_empty()
            && candidates
                .iter()
                .all(|c| state.no_cache_providers.contains(&c.provider.slug))
    };

    // Cache read (bypass when ttl=0 or all providers in the group are no-cache)
    if cache_ttl > 0 && !group_all_no_cache {
        let key = QueryCache::cache_key(
            &req.query,
            req.language.as_deref(),
            req.country.as_deref(),
            req.group.as_deref(),
        );
        if let Some(entry) = state.cache.get(&key) {
            let n_returned = entry.results.len();
            let cached_chain = format!("{}:cached", entry.provider_slug);
            return Ok(Json(SearchResponse {
                results: entry.results,
                meta: SearchMeta {
                    provider: entry.provider_slug,
                    api_key_id: entry.api_key_id,
                    fallback_chain: cached_chain,
                    cache_hit: true,
                    duration_ms: 0,
                    n_returned,
                },
                debug: None,
                attempts: vec![],
            }));
        }
    }

    let query_hash = QueryCache::cache_key(
        &req.query,
        req.language.as_deref(),
        req.country.as_deref(),
        req.group.as_deref(),
    );

    let result = state
        .executor
        .search(SearchParams {
            query: req.query.clone(),
            query_hash: query_hash.clone(),
            language: req.language.clone(),
            country: req.country.clone(),
            group_slug: req.group.clone(),
            n,
            timeout_ms,
            max_fallbacks,
            debug: req.debug,
            exclude_key_ids: req.exclude_key_ids,
            exclude_provider_slugs: req.exclude_providers,
        })
        .await?;

    info!(
        query_hash = query_hash,
        provider = result.provider_slug,
        duration_ms = result.duration_ms,
        n_results = result.results.len(),
        fallback_chain = result.fallback_chain,
        cache_hit = false,
    );

    // Store result in cache — skip if the winning provider is marked no_cache.
    if cache_ttl > 0 && !state.no_cache_providers.contains(&result.provider_slug) {
        let key = query_hash.clone();
        state.cache.set(
            key,
            CacheEntry {
                results: result.results.clone(),
                provider_slug: result.provider_slug.clone(),
                api_key_id: result.api_key_id.clone(),
                stored_at: Instant::now(),
                ttl: Duration::from_secs(cache_ttl),
            },
        );
    }

    // Async log to DB
    let storage = Arc::clone(state.catalog.storage());
    let log = result.log.clone();
    tokio::spawn(async move {
        let _ = storage.log_search(log).await;
    });

    let n_returned = result.results.len();
    Ok(Json(SearchResponse {
        results: result.results,
        meta: SearchMeta {
            provider: result.provider_slug,
            api_key_id: result.api_key_id,
            fallback_chain: result.fallback_chain,
            cache_hit: false,
            duration_ms: result.duration_ms,
            n_returned,
        },
        debug: result.debug_decisions,
        attempts: result.attempts,
    }))
}
