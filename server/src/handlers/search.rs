use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{extract::State, Json};
use cache::{CacheEntry, QueryCache};
use serde::{Deserialize, Serialize};
use tracing::info;

use providers::operators::Freshness;
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
    #[serde(default)]
    pub priority: crate::pacing::Priority,
    #[serde(default = "default_true")]
    pub fallback_on_empty: bool,
    pub cache_ttl_secs: Option<u64>,
    #[serde(default)]
    pub debug: bool,
    /// Mirrors staan's "Web Search for AI" enrichment params (staan_2.md) - same
    /// shape works for tavily since it's the same concept. Fetch result pages and
    /// return semantically scored chunks.
    #[serde(default)]
    pub extra_snippets: bool,
    /// Return the full page body: "markdown" | "html" | "text".
    pub full_content: Option<String>,
    pub max_snippets: Option<usize>,
    pub min_score: Option<f64>,
    /// When an enrichment field (`extra_snippets` / `full_content`) is requested:
    /// `true` (default) keeps the strict behaviour - the candidate pool is filtered
    /// to providers that can actually deliver it, and the request 503s if none can.
    /// `false` makes enrichment best-effort - the normal group cascade runs
    /// unfiltered, capable providers fill the enriched fields when they're reached,
    /// and a fallback to a non-capable provider simply returns bare results
    /// (`extra_snippets` / `full_content` absent) rather than failing.
    #[serde(default = "default_true")]
    pub require_enrichment: bool,
    #[serde(default)]
    pub include_domains: Vec<String>,
    #[serde(default)]
    pub exclude_domains: Vec<String>,
    /// Relative publication window: "day" | "week" | "month" | "year".
    pub freshness: Option<String>,
    /// Explicit publication range, `YYYY-MM-DD` (either bound optional). Takes precedence
    /// over `freshness` for providers with a date-range parameter; window-only engines get
    /// the smallest window covering it (none for a historical range).
    pub date_from: Option<String>,
    pub date_to: Option<String>,
}

fn default_n() -> usize {
    10
}

fn default_true() -> bool {
    true
}

/// Folds every result-affecting field beyond query/language/country/group into one
/// discriminator string so the per-query SERP cache never crosses a plain request with
/// an enriched, domain-restricted or date-restricted one. A plain request keeps the
/// empty key (and so its existing cache entries).
fn enrichment_cache_key(req: &SearchRequest, freshness: &Freshness) -> String {
    let mut key = String::new();
    if req.extra_snippets || req.full_content.is_some() {
        key = format!(
            "es={}&fc={}&ms={}&mn={}&inc={}&exc={}",
            req.extra_snippets,
            req.full_content.as_deref().unwrap_or(""),
            req.max_snippets.map(|v| v.to_string()).unwrap_or_default(),
            req.min_score.map(|v| v.to_string()).unwrap_or_default(),
            req.include_domains.join(","),
            req.exclude_domains.join(","),
        );
    } else if !req.include_domains.is_empty() || !req.exclude_domains.is_empty() {
        // Before: a domain-restricted plain search shared the unrestricted one's entry.
        key = format!(
            "inc={}&exc={}",
            req.include_domains.join(","),
            req.exclude_domains.join(",")
        );
    }
    if !req.fallback_on_empty {
        key.push_str("&fallback_on_empty=false");
    }
    let f = freshness.cache_key();
    if !f.is_empty() {
        key.push_str(&format!("&fr={f}"));
    }
    key
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
    // Enrichment (extra_snippets/full_content) fetches + reranks pages - budget
    // more than the plain-search default (staan_2.md: "8-10s when enrichment is
    // enabled"). Still fully overridable via the request's own timeout_ms.
    let wants_enrichment = req.extra_snippets || req.full_content.is_some();
    let default_timeout_ms = if wants_enrichment { 10_000 } else { 8_000 };
    let timeout_ms = req.timeout_ms.unwrap_or(default_timeout_ms);
    let max_fallbacks = req.max_fallbacks.unwrap_or(state.config.max_fallbacks);
    let freshness = Freshness::from_request(
        req.freshness.as_deref(),
        req.date_from.as_deref(),
        req.date_to.as_deref(),
    );
    let enrichment_key = enrichment_cache_key(&req, &freshness);

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
            &enrichment_key,
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
        &enrichment_key,
    );

    let mut result = state
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
            priority: req.priority,
            fallback_on_empty: req.fallback_on_empty,
            debug: req.debug,
            exclude_key_ids: req.exclude_key_ids,
            exclude_provider_slugs: req.exclude_providers,
            extra_snippets: req.extra_snippets,
            full_content: req.full_content.clone(),
            max_snippets: req.max_snippets,
            min_score: req.min_score,
            require_enrichment: req.require_enrichment,
            include_domains: req.include_domains.clone(),
            exclude_domains: req.exclude_domains.clone(),
            freshness,
        })
        .await?;

    // Doc cache: store any freshly-fetched content by URL so a later query - even
    // a completely different one - that surfaces the same page reuses it instead
    // of paying Tavily/Staan again; and backfill from a prior hit when this
    // provider's own response came back without it (e.g. staan returned
    // length=0 for a page it couldn't fetch this time, but we have an older copy).
    if wants_enrichment {
        let doc_ttl = Duration::from_secs(state.config.doc_cache_ttl_secs);
        for r in result.results.iter_mut() {
            if r.full_content.is_some() || r.extra_snippets.is_some() {
                state.doc_cache.set(
                    &r.url,
                    r.full_content.clone(),
                    r.extra_snippets.clone(),
                    doc_ttl,
                );
            } else if let Some(doc) = state.doc_cache.get(&r.url) {
                r.full_content = doc.full_content;
                r.extra_snippets = doc.extra_snippets;
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::Priority;

    #[test]
    fn defaults_unknown_priority_and_empty_policy_cache_partition() {
        let old: SearchRequest =
            serde_json::from_value(serde_json::json!({"query":"test"})).unwrap();
        assert!(matches!(old.priority, Priority::Main));
        assert!(old.fallback_on_empty);
        assert_eq!(enrichment_cache_key(&old, &Freshness::default()), "");
        let aux: SearchRequest =
            serde_json::from_value(serde_json::json!({"query":"test", "priority":"aux"})).unwrap();
        assert!(matches!(aux.priority, Priority::Aux));
        let unknown: SearchRequest =
            serde_json::from_value(serde_json::json!({"query":"test", "priority":"future-value"}))
                .unwrap();
        assert!(matches!(unknown.priority, Priority::Main));
        let no_fallback: SearchRequest =
            serde_json::from_value(serde_json::json!({"query":"test", "fallback_on_empty":false}))
                .unwrap();
        assert_ne!(
            enrichment_cache_key(&no_fallback, &Freshness::default()),
            enrichment_cache_key(&old, &Freshness::default())
        );
    }
}
