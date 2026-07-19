use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

use providers::{ProviderError, SearchProvider, SearchQuery};
use proviz_core::{
    key_resolver::{resolve_key, ResolveError},
    models::{Candidate, SearchLog, SearchResult},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{DebugDecision, SelectRequest, Selector},
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{catalog::CatalogStore, stats::StatsTracker};

pub struct SearchParams {
    pub query: String,
    pub query_hash: String,
    pub language: Option<String>,
    pub country: Option<String>,
    pub group_slug: Option<String>,
    pub n: usize,
    pub timeout_ms: u64,
    pub max_fallbacks: usize,
    pub debug: bool,
    pub exclude_key_ids: Vec<String>,
    pub exclude_provider_slugs: Vec<String>,
    pub extra_snippets: bool,
    pub full_content: Option<String>,
    pub max_snippets: Option<usize>,
    pub min_score: Option<f64>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
}

impl SearchParams {
    fn wants_enrichment(&self) -> bool {
        self.extra_snippets || self.full_content.is_some()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AttemptRecord {
    pub provider: String,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Result of a full search execution including fallback chain.
pub struct ExecutionResult {
    pub results: Vec<SearchResult>,
    pub provider_slug: String,
    pub api_key_id: String,
    pub duration_ms: u64,
    pub fallback_chain: String,
    pub debug_decisions: Option<Vec<DebugDecision>>,
    pub log: SearchLog,
    pub attempts: Vec<AttemptRecord>,
}

pub struct Executor {
    catalog: CatalogStore,
    selector: Arc<Selector>,
    providers: HashMap<String, Arc<dyn SearchProvider>>,
    rate_limit: RateLimitState,
    usage: UsageTracker,
    secrets_dir: PathBuf,
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
        stats: Arc<StatsTracker>,
    ) -> Self {
        Self {
            catalog,
            selector,
            providers,
            rate_limit,
            usage,
            secrets_dir,
            stats,
        }
    }

    pub async fn search(
        &self,
        params: SearchParams,
    ) -> Result<ExecutionResult, crate::error::AppError> {
        let start = Instant::now();
        let catalog = self.catalog.read().await;
        let mut pool = catalog.candidates(params.group_slug.as_deref());
        drop(catalog);

        // Enrichment requested → only keep candidates whose adapter can actually
        // deliver every requested field. Otherwise a mid-chain fallback to a
        // non-enrichment provider (e.g. Brave) would silently hand the caller
        // bare results with no signal that full_content/extra_snippets never came.
        if params.wants_enrichment() {
            pool.retain(|c| {
                let Some(provider) = self.providers.get(&c.provider.slug) else {
                    return false;
                };
                (!params.extra_snippets || provider.supports_extra_snippets())
                    && (params.full_content.is_none() || provider.supports_full_content())
            });
        }

        if pool.is_empty() {
            debug!("no provider candidates in pool");
            return Err(crate::error::AppError::service_unavailable(
                if params.wants_enrichment() {
                    "No enrichment-capable provider candidates available"
                } else {
                    "No provider candidates available"
                },
            ));
        }
        debug!(pool_size = pool.len(), "candidate pool ready");

        // When using a group (member_priority is set), enforce strict tier ordering:
        // all candidates in priority tier N are exhausted before tier N+1 is tried.
        // Without a group, every candidate shares one implicit tier.
        let tiers: Vec<Vec<Candidate>> = if pool.iter().any(|c| c.member_priority.is_some()) {
            let mut by_priority: std::collections::BTreeMap<i64, Vec<Candidate>> =
                Default::default();
            for c in &pool {
                by_priority
                    .entry(c.effective_priority())
                    .or_default()
                    .push(c.clone());
            }
            by_priority.into_values().collect()
        } else {
            vec![pool]
        };

        let req = SelectRequest {
            language: params.language.clone(),
            country: params.country.clone(),
            exclude_key_ids: params.exclude_key_ids.clone(),
            exclude_provider_slugs: params.exclude_provider_slugs.clone(),
        };

        let mut excluded: Vec<String> = Vec::new();
        let mut chain_parts: Vec<String> = Vec::new();
        let mut all_decisions: Vec<DebugDecision> = Vec::new();
        let mut attempt_records: Vec<AttemptRecord> = Vec::new();
        let mut tier_idx: usize = 0;

        loop {
            if attempt_records.len() > params.max_fallbacks {
                break;
            }

            let current_tier = &tiers[tier_idx];
            let selection = self
                .selector
                .select(current_tier, &req, &excluded, params.debug);

            // Current tier exhausted — advance to the next one (if any).
            let (candidate, decisions) = match selection {
                Some(pair) => pair,
                None => {
                    tier_idx += 1;
                    if tier_idx >= tiers.len() {
                        break;
                    }
                    debug!(tier = tier_idx, "advancing to next priority tier");
                    continue;
                }
            };

            if params.debug {
                all_decisions.extend(decisions);
            }

            debug!(
                attempt = attempt_records.len() + 1,
                provider = candidate.provider.slug,
                key_ref = candidate.api_key.key_ref,
                "trying candidate"
            );
            let attempt_start = Instant::now();
            let result = self.try_candidate(&candidate, &params).await;

            match result {
                Ok(output) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    // Use the effective slug (e.g. "ddg-yandex") when the provider
                    // reports one; otherwise fall back to the DB slug.
                    let provider_slug = output
                        .effective_slug
                        .clone()
                        .unwrap_or_else(|| candidate.provider.slug.clone());
                    chain_parts.push(format!("{provider_slug}:ok"));
                    attempt_records.push(AttemptRecord {
                        provider: provider_slug.clone(),
                        success: true,
                        error: None,
                        duration_ms: attempt_start.elapsed().as_millis() as u64,
                    });

                    let storage = Arc::clone(self.catalog.storage());
                    let pid = candidate.provider.id.clone();
                    let lat = duration_ms as i64;
                    tokio::spawn(async move {
                        let _ = storage.update_avg_latency(&pid, lat).await;
                    });

                    self.stats.record_search(&provider_slug, false, duration_ms);

                    let log = SearchLog {
                        id: Uuid::new_v4().to_string(),
                        query_hash: params.query_hash.clone(),
                        group_slug: params.group_slug.clone(),
                        language: params.language.clone(),
                        country: params.country.clone(),
                        provider_slug: Some(provider_slug.clone()),
                        api_key_id: Some(candidate.api_key.id.clone()),
                        n_requested: Some(params.n as i64),
                        n_returned: Some(output.results.len() as i64),
                        duration_ms: Some(duration_ms as i64),
                        cache_hit: false,
                        success: Some(true),
                        error_type: None,
                        fallback_chain: Some(chain_parts.join(",")),
                        requested_at: String::new(),
                    };

                    return Ok(ExecutionResult {
                        results: output.results,
                        provider_slug,
                        api_key_id: candidate.api_key.id,
                        duration_ms,
                        fallback_chain: chain_parts.join(","),
                        debug_decisions: params.debug.then_some(all_decisions),
                        log,
                        attempts: attempt_records,
                    });
                }
                Err(e) => {
                    let error_type = e.error_type_str();
                    debug!(
                        provider = candidate.provider.slug,
                        key_ref = candidate.api_key.key_ref,
                        error_type,
                        error = %e,
                        "candidate failed, moving to next"
                    );
                    chain_parts.push(format!("{}:{}", candidate.provider.slug, error_type));

                    let et = match error_type {
                        "rpm" => ErrorType::Rpm,
                        "auth" => {
                            warn!(
                                key_id = candidate.api_key.id,
                                key_ref = candidate.api_key.key_ref,
                                "auth error - key disabled for 300s"
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
                    attempt_records.push(AttemptRecord {
                        provider: candidate.provider.slug.clone(),
                        success: false,
                        error: Some(error_type.to_string()),
                        duration_ms: attempt_start.elapsed().as_millis() as u64,
                    });
                    excluded.push(candidate.api_key.id.clone());
                }
            }
        }

        debug!(
            chain = chain_parts.join(","),
            "all candidates exhausted, no result"
        );
        Err(crate::error::AppError::service_unavailable(
            "All provider candidates exhausted",
        ))
    }

    async fn try_candidate(
        &self,
        candidate: &Candidate,
        params: &SearchParams,
    ) -> Result<providers::SearchOutput, ProviderError> {
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

        let query = SearchQuery {
            query: &params.query,
            n: params.n,
            language: params.language.as_deref(),
            country: params.country.as_deref(),
            api_key: &api_key,
            extra_snippets: params.extra_snippets,
            full_content: params.full_content.as_deref(),
            max_snippets: params.max_snippets,
            min_score: params.min_score,
            include_domains: &params.include_domains,
            exclude_domains: &params.exclude_domains,
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(params.timeout_ms),
            provider.search(query),
        )
        .await;

        self.usage.complete(&candidate.api_key.id);

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
