use std::time::Duration;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tracing::info;

use proviz_core::selector::DebugDecision;

use crate::{
    app::AppState,
    content_executor::{ContentParams, ExtractedDoc},
    error::AppError,
    executor::AttemptRecord,
};

const MAX_URLS: usize = 50;
const DEFAULT_TIMEOUT_MS: u64 = 20_000;

#[derive(Deserialize)]
pub struct ContentsRequest {
    pub urls: Vec<String>,
    /// "markdown" (default) | "html" | "text".
    pub format: Option<String>,
    /// Optional focus hint forwarded to providers that support it (Parallel).
    pub objective: Option<String>,
    /// Restrict/prioritize the extractor pool to a group's members.
    pub group: Option<String>,
    pub max_fallbacks: Option<usize>,
    pub timeout_ms: Option<u64>,
    /// Bypass the URL-keyed doc cache and always re-fetch.
    #[serde(default)]
    pub fresh: bool,
    #[serde(default)]
    pub debug: bool,
}

#[derive(Serialize)]
pub struct ContentsResponse {
    pub docs: Vec<ExtractedDoc>,
    pub meta: ContentsMeta,
    pub attempts: Vec<AttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<Vec<DebugDecision>>,
}

#[derive(Serialize)]
pub struct ContentsMeta {
    pub providers_used: Vec<String>,
    pub fallback_chain: String,
    pub cache_hits: usize,
    pub n_ok: usize,
    pub n_failed: usize,
    pub duration_ms: u64,
}

fn normalize_format(raw: Option<&str>) -> Result<String, AppError> {
    match raw.unwrap_or("markdown") {
        f @ ("markdown" | "html" | "text") => Ok(f.to_string()),
        other => Err(AppError::not_found(format!(
            "unsupported format '{other}' (expected markdown|html|text)"
        ))),
    }
}

pub async fn handle_contents(
    State(state): State<AppState>,
    Json(req): Json<ContentsRequest>,
) -> Result<Json<ContentsResponse>, AppError> {
    // Dedup preserving request order; cap the batch.
    let mut seen = std::collections::HashSet::new();
    let urls: Vec<String> = req
        .urls
        .iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty() && seen.insert(u.clone()))
        .take(MAX_URLS)
        .collect();

    if urls.is_empty() {
        return Err(AppError::not_found("no urls provided"));
    }

    let format = normalize_format(req.format.as_deref())?;
    let timeout_ms = req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let max_fallbacks = req.max_fallbacks.unwrap_or(state.config.max_fallbacks);
    let doc_ttl = Duration::from_secs(state.config.doc_cache_ttl_secs);

    // Serve what we can from the URL-keyed doc cache (same store the /search
    // enrichment path fills), leaving only genuine misses for the pool.
    let mut docs_by_url: std::collections::HashMap<String, ExtractedDoc> =
        std::collections::HashMap::new();
    let mut cache_hits = 0usize;
    let mut to_fetch: Vec<String> = Vec::new();

    for url in &urls {
        let cached = (!req.fresh)
            .then(|| state.doc_cache.get(url))
            .flatten()
            .and_then(|e| e.full_content)
            .filter(|fc| fc.format == format);
        match cached {
            Some(fc) => {
                cache_hits += 1;
                docs_by_url.insert(
                    url.clone(),
                    ExtractedDoc {
                        url: url.clone(),
                        title: None,
                        published_date: None,
                        content: Some(fc),
                        source: "cache".to_string(),
                        error: None,
                    },
                );
            }
            None => to_fetch.push(url.clone()),
        }
    }

    let result = state
        .content_executor
        .fetch(ContentParams {
            urls: to_fetch,
            format: format.clone(),
            objective: req.objective.clone(),
            group_slug: req.group.clone(),
            timeout_ms,
            max_fallbacks,
            debug: req.debug,
        })
        .await?;

    // Stash freshly-extracted bodies for reuse by later /contents and /search.
    for doc in &result.docs {
        if let Some(fc) = &doc.content {
            state
                .doc_cache
                .set(&doc.url, Some(fc.clone()), None, doc_ttl);
        }
    }
    for doc in result.docs {
        docs_by_url.insert(doc.url.clone(), doc);
    }

    // Re-emit in request order.
    let docs: Vec<ExtractedDoc> = urls.iter().filter_map(|u| docs_by_url.remove(u)).collect();
    let n_ok = docs.iter().filter(|d| d.content.is_some()).count();
    let n_failed = docs.len() - n_ok;

    info!(
        n_urls = urls.len(),
        cache_hits,
        n_ok,
        n_failed,
        fallback_chain = result.fallback_chain,
        "contents request complete"
    );

    Ok(Json(ContentsResponse {
        meta: ContentsMeta {
            providers_used: result.providers_used,
            fallback_chain: result.fallback_chain,
            cache_hits,
            n_ok,
            n_failed,
            duration_ms: result.duration_ms,
        },
        docs,
        attempts: result.attempts,
        debug: result.debug_decisions,
    }))
}
