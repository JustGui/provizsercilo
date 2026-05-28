use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

use providers::{ProviderError, SearchProvider};
use proviz_core::{
    key_resolver::{resolve_key, ResolveError},
    models::{Candidate, SearchLog, SearchResult},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{DebugDecision, SelectRequest, Selector},
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{catalog::CatalogStore, stats::StatsTracker};

/// Result of a full search execution including fallback chain.
pub struct ExecutionResult {
    pub results: Vec<SearchResult>,
    pub provider_slug: String,
    pub api_key_id: String,
    pub duration_ms: u64,
    pub fallback_chain: String,
    pub debug_decisions: Option<Vec<DebugDecision>>,
    pub log: SearchLog,
}

pub struct Executor {
    catalog: CatalogStore,
    selector: Arc<Selector>,
    providers: HashMap<String, Arc<dyn SearchProvider>>,
    rate_limit: RateLimitState,
    usage: UsageTracker,
    secrets_dir: PathBuf,
    storage: Arc<storage_sqlite::Storage>,
    stats: Arc<StatsTracker>,
}

impl Executor {
    pub fn new(
        catalog: CatalogStore,
        selector: Arc<Selector>,
        providers: HashMap<String, Arc<dyn SearchProvider>>,
        rate_limit: RateLimitState,
        usage: UsageTracker,
        secrets_dir: PathBuf,
        storage: Arc<storage_sqlite::Storage>,
        stats: Arc<StatsTracker>,
    ) -> Self {
        Self {
            catalog,
            selector,
            providers,
            rate_limit,
            usage,
            secrets_dir,
            storage,
            stats,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        query_hash: &str,
        language: Option<&str>,
        country: Option<&str>,
        group_slug: Option<&str>,
        n: usize,
        timeout_ms: u64,
        max_fallbacks: usize,
        debug: bool,
        exclude_key_ids: Vec<String>,
        exclude_provider_slugs: Vec<String>,
    ) -> Result<ExecutionResult, crate::error::AppError> {
        let start = Instant::now();
        let catalog = self.catalog.read().await;
        let pool = catalog.candidates(group_slug);
        drop(catalog);

        if pool.is_empty() {
            return Err(crate::error::AppError::service_unavailable(
                "No provider candidates available",
            ));
        }

        let req = SelectRequest {
            language: language.map(str::to_string),
            country: country.map(str::to_string),
            exclude_key_ids,
            exclude_provider_slugs,
        };

        let mut excluded: Vec<String> = Vec::new();
        let mut chain_parts: Vec<String> = Vec::new();
        let mut all_decisions: Vec<DebugDecision> = Vec::new();
        let mut attempts = 0;

        loop {
            if attempts > max_fallbacks {
                break;
            }
            attempts += 1;

            let selection = self.selector.select(&pool, &req, &excluded, debug);
            let Some((candidate, decisions)) = selection else {
                break;
            };

            if debug {
                all_decisions.extend(decisions);
            }

            let result = self
                .try_candidate(&candidate, query, n, language, country, timeout_ms)
                .await;

            match result {
                Ok(results) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    chain_parts.push(format!("{}:{}", candidate.provider.slug, "ok"));

                    // Update latency rolling average
                    let storage = Arc::clone(self.catalog.storage());
                    let pid = candidate.provider.id.clone();
                    let lat = duration_ms as i64;
                    tokio::spawn(async move {
                        let _ = storage.update_avg_latency(&pid, lat).await;
                    });

                    self.stats
                        .record_search(&candidate.provider.slug, false, duration_ms);

                    let log = SearchLog {
                        id: Uuid::new_v4().to_string(),
                        query_hash: query_hash.to_string(),
                        group_slug: group_slug.map(str::to_string),
                        language: language.map(str::to_string),
                        country: country.map(str::to_string),
                        provider_slug: Some(candidate.provider.slug.clone()),
                        api_key_id: Some(candidate.api_key.id.clone()),
                        n_requested: Some(n as i64),
                        n_returned: Some(results.len() as i64),
                        duration_ms: Some(duration_ms as i64),
                        cache_hit: false,
                        success: Some(true),
                        error_type: None,
                        fallback_chain: Some(chain_parts.join(",")),
                        requested_at: String::new(),
                    };

                    return Ok(ExecutionResult {
                        results,
                        provider_slug: candidate.provider.slug,
                        api_key_id: candidate.api_key.id,
                        duration_ms,
                        fallback_chain: chain_parts.join(","),
                        debug_decisions: debug.then_some(all_decisions),
                        log,
                    });
                }
                Err(e) => {
                    let error_type = e.error_type_str();
                    chain_parts.push(format!("{}:{}", candidate.provider.slug, error_type));

                    let et = match error_type {
                        "rpm" => ErrorType::Rpm,
                        "auth" => {
                            warn!(
                                key_id = candidate.api_key.id,
                                key_ref = candidate.api_key.key_ref,
                                "auth error — key disabled for 300s"
                            );
                            ErrorType::Auth
                        }
                        "timeout" => ErrorType::Timeout,
                        "empty" => ErrorType::Empty,
                        _ => ErrorType::Error,
                    };

                    self.rate_limit.report_error(&candidate.api_key.id, et);

                    let storage = Arc::clone(self.catalog.storage());
                    let kid = candidate.api_key.id.clone();
                    let et_str = error_type.to_string();
                    tokio::spawn(async move {
                        let _ = storage.record_rate_event(&kid, &et_str).await;
                    });

                    self.stats.record_search(&candidate.provider.slug, true, 0);
                    excluded.push(candidate.api_key.id.clone());
                }
            }
        }

        Err(crate::error::AppError::service_unavailable(
            "All provider candidates exhausted",
        ))
    }

    async fn try_candidate(
        &self,
        candidate: &Candidate,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Vec<SearchResult>, ProviderError> {
        let provider = self
            .providers
            .get(&candidate.provider.slug)
            .ok_or_else(|| ProviderError::Http {
                status: 0,
                message: format!("No adapter for provider '{}'", candidate.provider.slug),
            })?;

        let api_key =
            resolve_key(&candidate.api_key.key_ref, &self.secrets_dir).map_err(|e| match e {
                ResolveError::NotFound(_) | ResolveError::FileRead { .. } => ProviderError::Http {
                    status: 401,
                    message: format!(
                        "key_ref '{}' could not be resolved",
                        candidate.api_key.key_ref
                    ),
                },
            })?;

        self.usage.reserve(&candidate.api_key.id);

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            provider.search(query, n, language, country, &api_key),
        )
        .await;

        self.usage.complete(&candidate.api_key.id);

        // Touch the key's last_used_at asynchronously
        let storage = Arc::clone(self.catalog.storage());
        let kid = candidate.api_key.id.clone();
        tokio::spawn(async move {
            let _ = storage.touch_api_key(&kid).await;
        });

        match result {
            Ok(inner) => inner,
            Err(_elapsed) => Err(ProviderError::Timeout),
        }
    }
}
