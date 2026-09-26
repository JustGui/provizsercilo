use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use providers::{ProviderError, SearchProvider, SearchQuery};
use proviz_core::{
    key_resolver::{resolve_key, ResolveError},
    models::{Candidate, SearchLog, SearchResult},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{DebugDecision, SelectRequest, Selector},
};
use tracing::{debug, info, warn};
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
    /// See `SearchRequest::require_enrichment`. `false` = enrichment is best-effort:
    /// don't filter the pool to capable providers, don't 503 when none can enrich.
    pub require_enrichment: bool,
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
        if params.wants_enrichment() && params.require_enrichment {
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
                if params.wants_enrichment() && params.require_enrichment {
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
        // Counts only real provider calls (try_candidate invocations), not
        // tier-skip bookkeeping — skips must never eat into this budget or a
        // tier full of rate-limited free candidates could burn it before the
        // paid fallback tier is ever reached.
        let mut real_attempts: usize = 0;

        loop {
            if real_attempts > params.max_fallbacks {
                break;
            }

            let current_tier = &tiers[tier_idx];
            let outcome = self
                .selector
                .select(current_tier, &req, &excluded, params.debug);

            if params.debug {
                all_decisions.extend(outcome.decisions.clone());
            }

            // Current tier exhausted — record why every candidate in it was
            // skipped (rate-limit cooldown, inactive, excluded, ...) so a
            // final 503 can explain itself instead of returning an empty
            // attempts/chain, then advance to the next tier (if any).
            let Some(candidate) = outcome.candidate else {
                for d in &outcome.decisions {
                    let reason = d.reason.clone().unwrap_or_else(|| d.outcome.clone());
                    chain_parts.push(format!("{}:skipped:{reason}", d.provider));
                    attempt_records.push(AttemptRecord {
                        provider: d.provider.clone(),
                        success: false,
                        error: Some(format!("skipped:{reason}")),
                        duration_ms: 0,
                    });
                }
                tier_idx += 1;
                if tier_idx >= tiers.len() {
                    break;
                }
                debug!(tier = tier_idx, "advancing to next priority tier");
                continue;
            };

            real_attempts += 1;
            debug!(
                attempt = real_attempts,
                provider = candidate.provider.slug,
                key_ref = candidate.api_key.key_ref,
                "trying candidate"
            );
            let attempt_start = Instant::now();
            let result = self.try_candidate(&candidate, &params).await;

            match result {
                Ok(output) => {
                    self.rate_limit.report_success(&candidate.api_key.id);
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

                    // Snapshot the cost at the CPM in effect right now - a later CPM
                    // edit must not retroactively change historical totals.
                    let cost = candidate.api_key.cost_per_mille.map(|cpm| cpm / 1000.0);
                    let currency = cost.and_then(|_| candidate.api_key.currency.clone());

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
                        cost,
                        currency,
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
                    // info, not debug: the provider's own answer (status, message) is
                    // the only way to tell why a paid key keeps failing.
                    info!(
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

                    if self.rate_limit.report_error(&candidate.api_key.id, et) {
                        warn!(
                            provider = candidate.provider.slug,
                            key_ref = candidate.api_key.key_ref,
                            error_type,
                            cooldown_secs = et.cooldown_secs(),
                            "key cooled down"
                        );
                    }

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

        warn!(
            chain = chain_parts.join(","),
            attempts = real_attempts,
            skipped = attempt_records.len() - real_attempts,
            "all candidates exhausted, no result"
        );
        Err(crate::error::AppError::service_unavailable_with_attempts(
            "All provider candidates exhausted or rate-limited",
            chain_parts.join(","),
            &attempt_records,
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

        // A transient blip (timeout, network error, 5xx) gets one short-backoff
        // retry against the *same* candidate before it's marked down and
        // excluded from the rest of this call's fallback chain — avoids
        // burning a whole (rate-limit-cooldown-triggering) attempt on a
        // one-off glitch. Non-transient errors (429, 401/403, empty results)
        // fail fast since a retry can't help.
        const MAX_ATTEMPTS: u32 = 2;
        const RETRY_BACKOFF: Duration = Duration::from_millis(200);

        let mut last_err = ProviderError::Timeout;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BACKOFF).await;
                debug!(
                    provider = candidate.provider.slug,
                    attempt, "retrying after transient error"
                );
            }

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
                Duration::from_millis(params.timeout_ms),
                provider.search(query),
            )
            .await;

            self.usage.complete(&candidate.api_key.id);

            let storage = Arc::clone(self.catalog.storage());
            let kid = candidate.api_key.id.clone();
            tokio::spawn(async move {
                let _ = storage.touch_api_key(&kid).await;
            });

            let err = match result {
                Ok(Ok(output)) => return Ok(output),
                Ok(Err(e)) => e,
                Err(_elapsed) => ProviderError::Timeout,
            };

            let is_last_attempt = attempt + 1 >= MAX_ATTEMPTS;
            if is_last_attempt || !is_transient(&err) {
                return Err(err);
            }
            last_err = err;
        }
        Err(last_err)
    }
}

/// Whether retrying the same candidate right away might succeed: network
/// blips, timeouts, and 5xx responses are worth one retry; rate limits,
/// auth failures, and empty results are not (a retry can't fix them).
fn is_transient(err: &ProviderError) -> bool {
    matches!(err, ProviderError::Timeout | ProviderError::Request(_))
        || matches!(err, ProviderError::Http { status, .. } if *status >= 500)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use providers::SearchOutput;
    use proviz_core::{
        language_profile::ProfileMatcher,
        models::{ApiKey, Provider},
        storage::StorageBackend,
    };
    use std::collections::HashMap as Map;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers `Empty` for its first `fail_first` calls, then one result.
    struct FlakyProvider {
        slug: &'static str,
        fail_first: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SearchProvider for FlakyProvider {
        fn slug(&self) -> &str {
            self.slug
        }
        async fn search(&self, _q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.fail_first {
                return Err(ProviderError::Empty);
            }
            Ok(SearchOutput::new(vec![SearchResult {
                url: format!("https://{}.test/r", self.slug),
                title: "t".into(),
                snippet: "s".into(),
                domain: format!("{}.test", self.slug),
                rank: 0,
                published_date: None,
                language: None,
                full_content: None,
                extra_snippets: None,
            }]))
        }
    }

    /// (slug, key env var, tier priority)
    async fn catalog_with(providers: &[(&str, &str, i64)]) -> CatalogStore {
        let s = storage_sqlite::Storage::open_in_memory().unwrap();
        for (slug, key_ref, priority) in providers {
            std::env::set_var(key_ref, "k");
            let p = s
                .create_provider(Provider {
                    id: uuid::Uuid::new_v4().to_string(),
                    slug: slug.to_string(),
                    name: slug.to_string(),
                    base_url: None,
                    is_active: true,
                    priority: *priority,
                    avg_latency_ms: None,
                    coverage_scores: Map::new(),
                    notes: None,
                    created_at: String::new(),
                    no_cache: false,
                })
                .await
                .unwrap();
            s.create_api_key(ApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                provider_id: p.id,
                label: slug.to_string(),
                key_ref: key_ref.to_string(),
                is_active: true,
                rps_limit: None,
                rpm_limit: None,
                rpd_limit: None,
                last_used_at: None,
                created_at: String::new(),
                cost_per_mille: None,
                currency: None,
            })
            .await
            .unwrap();
        }
        let storage: Arc<dyn StorageBackend> = Arc::new(s);
        CatalogStore::new(storage).await.unwrap()
    }

    fn params() -> SearchParams {
        SearchParams {
            query: "q".into(),
            query_hash: "h".into(),
            language: None,
            country: None,
            group_slug: None,
            n: 5,
            timeout_ms: 2000,
            max_fallbacks: 3,
            debug: false,
            exclude_key_ids: vec![],
            exclude_provider_slugs: vec![],
            extra_snippets: false,
            full_content: None,
            max_snippets: None,
            min_score: None,
            require_enrichment: false,
            include_domains: vec![],
            exclude_domains: vec![],
        }
    }

    async fn executor(staan_fail_first: usize) -> Executor {
        let catalog =
            catalog_with(&[("staan", "EXEC_TEST_STAAN", 1), ("exa", "EXEC_TEST_EXA", 2)]).await;
        let mut map: HashMap<String, Arc<dyn SearchProvider>> = HashMap::new();
        for (slug, fail_first) in [("staan", staan_fail_first), ("exa", 0)] {
            map.insert(
                slug.to_string(),
                Arc::new(FlakyProvider {
                    slug,
                    fail_first,
                    calls: AtomicUsize::new(0),
                }),
            );
        }
        let rl = RateLimitState::default();
        let usage = UsageTracker::default();
        let selector = Arc::new(Selector::new(
            rl.clone(),
            usage.clone(),
            ProfileMatcher::new(vec![]),
        ));
        Executor::new(
            catalog,
            selector,
            map,
            rl,
            usage,
            PathBuf::from("/nonexistent"),
            Arc::new(StatsTracker::new()),
        )
    }

    #[tokio::test]
    async fn one_failure_does_not_send_the_next_search_to_the_fallback() {
        let ex = executor(1).await;
        let first = ex.search(params()).await.unwrap();
        assert_eq!(first.provider_slug, "exa"); // staan failed once -> fallback
        let second = ex.search(params()).await.unwrap();
        assert_eq!(
            second.provider_slug, "staan",
            "chain: {}",
            second.fallback_chain
        );
    }

    #[tokio::test]
    async fn a_run_of_failures_still_cools_the_key_down() {
        let ex = executor(3).await;
        for _ in 0..3 {
            let r = ex.search(params()).await.unwrap();
            assert_eq!(r.fallback_chain, "staan:empty,exa:ok");
        }
        let fourth = ex.search(params()).await.unwrap();
        assert_eq!(fourth.provider_slug, "exa");
        // staan is on cooldown: not called at all (it would answer ok by now)
        assert!(
            !fourth.fallback_chain.contains("staan:ok")
                && !fourth.fallback_chain.contains("staan:empty"),
            "chain: {}",
            fourth.fallback_chain
        );
    }
}
