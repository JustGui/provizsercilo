use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use providers::{operators, ProviderError, SearchProvider, SearchQuery};
use proviz_core::{
    key_resolver::{resolve_key, ResolveError},
    models::{Candidate, SearchLog, SearchResult},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{DebugDecision, SelectRequest, Selector},
};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    catalog::CatalogStore,
    pacing::{Pacer, Priority},
    stats::StatsTracker,
};

pub struct SearchParams {
    pub query: String,
    pub query_hash: String,
    pub language: Option<String>,
    pub country: Option<String>,
    pub group_slug: Option<String>,
    pub n: usize,
    pub timeout_ms: u64,
    pub priority: Priority,
    pub fallback_on_empty: bool,
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
    /// Publication window, mapped by each adapter that has a date parameter.
    pub freshness: operators::Freshness,
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
    pub pacer: Pacer,
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
            pacer: Pacer::default(),
            catalog,
            selector,
            providers,
            rate_limit,
            usage,
            secrets_dir,
            stats,
        }
    }

    pub fn with_pacing(mut self, safety: f64, aux_fraction: f64) -> Self {
        self.pacer = Pacer::new(safety, aux_fraction);
        self
    }

    /// A single minimal provider call. It bypasses user-search logs, cache, stats,
    /// enrichment and fallback; result bodies are discarded immediately.
    pub async fn probe_key(
        &self,
        key_id: &str,
        query: &str,
        timeout_ms: u64,
    ) -> Result<(), ProviderError> {
        let catalog = self.catalog.read().await;
        let candidate = catalog
            .candidates(None)
            .into_iter()
            .find(|c| c.api_key.id == key_id)
            .ok_or(ProviderError::Blocked)?;
        drop(catalog);
        let provider = self
            .providers
            .get(&candidate.provider.slug)
            .ok_or_else(|| ProviderError::Parse("probe: no search adapter".into()))?;
        let api_key = resolve_key(&candidate.api_key.key_ref, &self.secrets_dir)
            .map_err(|_| ProviderError::Blocked)?;
        if self.usage.budget_exhausted(
            key_id,
            candidate.api_key.rps_limit,
            candidate.api_key.rpm_limit,
            candidate.api_key.rpd_limit,
        ) {
            return Err(ProviderError::RateLimit);
        }
        // Count real upstream traffic against configured budgets, while excluding
        // probes from user-search metrics/logs. Drop also handles cancellation.
        self.usage.reserve(key_id);
        struct Reservation<'a>(&'a UsageTracker, &'a str);
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                self.0.complete(self.1);
            }
        }
        let _reservation = Reservation(&self.usage, key_id);
        let result = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            provider.search(SearchQuery {
                query,
                n: 1,
                language: None,
                country: None,
                api_key: &api_key,
                extra_snippets: false,
                full_content: None,
                max_snippets: None,
                min_score: None,
                include_domains: &[],
                exclude_domains: &[],
                freshness: None,
            }),
        )
        .await
        .map_err(|_| ProviderError::Timeout)?;
        match result {
            Ok(_) | Err(ProviderError::Empty) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn search(
        &self,
        params: SearchParams,
    ) -> Result<ExecutionResult, crate::error::AppError> {
        let start = Instant::now();
        let result = self.search_inner(&params).await;
        if let Err(error) = &result {
            let last = error
                .attempts
                .as_ref()
                .and_then(|v| v.as_array())
                .and_then(|a| {
                    a.iter().rev().find(|a| {
                        a["error"]
                            .as_str()
                            .is_some_and(|e| !e.starts_with("skipped:"))
                    })
                });
            let log = SearchLog {
                id: Uuid::new_v4().to_string(),
                query_hash: params.query_hash.clone(),
                group_slug: params.group_slug.clone(),
                language: params.language.clone(),
                country: params.country.clone(),
                provider_slug: last
                    .and_then(|v| v["provider"].as_str())
                    .map(str::to_string),
                api_key_id: None,
                n_requested: Some(params.n as i64),
                n_returned: Some(0),
                duration_ms: Some(start.elapsed().as_millis() as i64),
                cache_hit: false,
                success: Some(false),
                error_type: Some(
                    last.and_then(|v| v["error"].as_str())
                        .unwrap_or(&error.code)
                        .to_string(),
                ),
                fallback_chain: Some(error.fallback_chain.clone().unwrap_or_default()),
                requested_at: String::new(),
                cost: None,
                currency: None,
            };
            if self.catalog.storage().log_search(log).await.is_err() {
                warn!("failed to persist unsuccessful search audit row");
            }
        }
        result
    }

    async fn search_inner(
        &self,
        params: &SearchParams,
    ) -> Result<ExecutionResult, crate::error::AppError> {
        if params.query.trim().is_empty() {
            return Err(crate::error::AppError::request_error(
                "query must not be blank".into(),
                String::new(),
                &Vec::<AttemptRecord>::new(),
            ));
        }
        let start = Instant::now();
        let deadline = start + Duration::from_millis(params.timeout_ms);
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

        let mut req = SelectRequest {
            allow_pacing: true,
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
        let mut query_refused = false;

        loop {
            if real_attempts > params.max_fallbacks || Instant::now() >= deadline {
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

            let slot = match self
                .pacer
                .reserve(&candidate.api_key, params.priority, deadline)
            {
                Ok(slot) => slot,
                Err(()) => {
                    chain_parts.push(format!(
                        "{}:skipped:budget_exhausted",
                        candidate.provider.slug
                    ));
                    attempt_records.push(AttemptRecord {
                        provider: candidate.provider.slug.clone(),
                        success: false,
                        error: Some("skipped:budget_exhausted".into()),
                        duration_ms: 0,
                    });
                    if params.debug {
                        all_decisions.push(DebugDecision {
                            provider: candidate.provider.slug.clone(),
                            key_label: candidate.api_key.label.clone(),
                            outcome: "skipped".into(),
                            reason: Some("budget_exhausted".into()),
                            detail: Some("pacing queue or deadline exhausted".into()),
                            cooldown_remaining_ms: None,
                            score: None,
                            score_components: None,
                        });
                    }
                    excluded.push(candidate.api_key.id.clone());
                    continue;
                }
            };
            if let Some(slot) = slot {
                slot.wait().await;
            }
            // Cooldowns and daily budget may have changed while this request waited.
            if candidate.api_key.pacing_rate().is_some()
                && (self.rate_limit.is_limited(&candidate.api_key.id)
                    || self
                        .usage
                        .rpd_headroom(&candidate.api_key.id, candidate.api_key.rpd_limit)
                        <= 0.0)
            {
                excluded.push(candidate.api_key.id.clone());
                continue;
            }

            real_attempts += 1;
            debug!(
                attempt = real_attempts,
                provider = candidate.provider.slug,
                "trying candidate"
            );
            let attempt_start = Instant::now();
            let result = self.try_candidate(&candidate, params, deadline).await;
            let result = match result {
                Err(ProviderError::Empty) if !params.fallback_on_empty => {
                    Ok(providers::SearchOutput::new(vec![]))
                }
                other => other,
            };

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
                    let outcome = if output.results.is_empty() {
                        "empty"
                    } else {
                        "ok"
                    };
                    chain_parts.push(format!("{provider_slug}:{outcome}"));
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
                    if let Some(reason) = e.query_refusal_reason() {
                        // A valid Sercilo request can exceed one provider's limits.
                        // Skip all its keys, without spending the fallback budget or
                        // penalizing the key; another provider may accept the query.
                        real_attempts -= 1;
                        query_refused = true;
                        chain_parts.push(format!("{}:skipped:{reason}", candidate.provider.slug));
                        attempt_records.push(AttemptRecord {
                            provider: candidate.provider.slug.clone(),
                            success: false,
                            error: Some(format!("skipped:{reason}")),
                            duration_ms: attempt_start.elapsed().as_millis() as u64,
                        });
                        req.exclude_provider_slugs
                            .push(candidate.provider.slug.clone());
                        continue;
                    }
                    e.log_auth_billing_status();
                    let error_type = e.error_type_str();
                    info!(
                        provider = candidate.provider.slug,
                        error_type, "candidate failed"
                    );
                    chain_parts.push(format!("{}:{}", candidate.provider.slug, error_type));
                    let et = e.error_type();
                    if self.rate_limit.report_error(&candidate.api_key.id, et) {
                        warn!(
                            provider = candidate.provider.slug,
                            key_id = candidate.api_key.id,
                            error_type,
                            "key parked pending successful probe"
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

        if query_refused && real_attempts == 0 {
            return Err(crate::error::AppError::request_error(
                "No provider accepted the query".into(),
                chain_parts.join(","),
                &attempt_records,
            ));
        }

        warn!(
            chain = chain_parts.join(","),
            attempts = real_attempts,
            skipped = attempt_records.len() - real_attempts,
            "all candidates exhausted, no result"
        );
        let retry_after = tiers
            .iter()
            .flatten()
            .filter(|c| {
                !params.exclude_key_ids.contains(&c.api_key.id)
                    && !params.exclude_provider_slugs.contains(&c.provider.slug)
                    && self.rate_limit.is_limited(&c.api_key.id)
            })
            .map(|c| {
                self.rate_limit
                    .cooldown_remaining_ms(&c.api_key.id)
                    .div_ceil(1000)
                    .max(1)
            })
            .min()
            .unwrap_or(1);
        Err(crate::error::AppError::service_unavailable_with_attempts(
            "All provider candidates exhausted or rate-limited",
            chain_parts.join(","),
            &attempt_records,
        )
        .with_retry_after(retry_after))
    }

    async fn try_candidate(
        &self,
        candidate: &Candidate,
        params: &SearchParams,
        deadline: Instant,
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

        // Domain restriction on every tier: a provider with a structured include/exclude
        // field gets the bare hosts; every other one gets `site:` operators in the query
        // text. Either way the answer is filtered to the requested domains (a path entry
        // like `lemonde.fr/les-decodeurs` is only expressible that way), and an answer
        // that had results but none inside them counts as empty, so the chain moves on.
        let restricted = !params.include_domains.is_empty() || !params.exclude_domains.is_empty();
        let native_domains = provider.supports_include_domains();
        let query_text = if restricted && !native_domains {
            operators::with_site_operators(
                &params.query,
                &params.include_domains,
                &params.exclude_domains,
            )
        } else {
            params.query.clone()
        };
        let (include_hosts, exclude_hosts) = if restricted && native_domains {
            (
                operators::hosts(&params.include_domains),
                operators::hosts(&params.exclude_domains),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let freshness = (!params.freshness.is_empty()).then_some(&params.freshness);
        let query = SearchQuery {
            query: &query_text,
            n: params.n,
            language: params.language.as_deref(),
            country: params.country.as_deref(),
            api_key: &api_key,
            extra_snippets: params.extra_snippets,
            full_content: params.full_content.as_deref(),
            max_snippets: params.max_snippets,
            min_score: params.min_score,
            include_domains: &include_hosts,
            exclude_domains: &exclude_hosts,
            freshness,
        };
        provider.validate_search(&query)?;

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
                if Instant::now() + RETRY_BACKOFF >= deadline {
                    return Err(last_err);
                }
                tokio::time::sleep(RETRY_BACKOFF).await;
                match self
                    .pacer
                    .reserve(&candidate.api_key, params.priority, deadline)
                {
                    Ok(Some(slot)) => slot.wait().await,
                    Ok(None) => {}
                    Err(()) => return Err(last_err),
                }
                debug!(
                    provider = candidate.provider.slug,
                    attempt, "retrying after transient error"
                );
            }

            if Instant::now() >= deadline {
                return Err(ProviderError::Timeout);
            }
            if candidate.api_key.pacing_rate().is_some()
                && (self.rate_limit.is_limited(&candidate.api_key.id)
                    || self
                        .usage
                        .rpd_headroom(&candidate.api_key.id, candidate.api_key.rpd_limit)
                        <= 0.0)
            {
                return Err(last_err);
            }
            self.usage.reserve(&candidate.api_key.id);
            struct InFlight<'a>(&'a UsageTracker, &'a str);
            impl Drop for InFlight<'_> {
                fn drop(&mut self) {
                    self.0.complete(self.1);
                }
            }
            let in_flight = InFlight(&self.usage, &candidate.api_key.id);

            let result = tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                provider.search(query),
            )
            .await;

            drop(in_flight);

            let storage = Arc::clone(self.catalog.storage());
            let kid = candidate.api_key.id.clone();
            tokio::spawn(async move {
                let _ = storage.touch_api_key(&kid).await;
            });

            let err = match result {
                Ok(Ok(output)) if output.results.is_empty() && params.fallback_on_empty => {
                    ProviderError::Empty
                }
                Ok(Ok(output)) if !restricted => return Ok(output),
                Ok(Ok(mut output)) => {
                    let before = output.results.len();
                    output.results = operators::filter_results(
                        output.results,
                        &params.include_domains,
                        &params.exclude_domains,
                    );
                    if output.results.is_empty() && before > 0 && params.fallback_on_empty {
                        debug!(
                            provider = candidate.provider.slug,
                            before, "no result inside the requested domains"
                        );
                        ProviderError::Empty
                    } else {
                        return Ok(output);
                    }
                }
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
        catalog_in(
            storage_sqlite::Storage::open_in_memory().unwrap(),
            providers,
        )
        .await
    }

    async fn catalog_in(
        s: storage_sqlite::Storage,
        providers: &[(&str, &str, i64)],
    ) -> CatalogStore {
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
                max_queue_ms: None,
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
            priority: Priority::Main,
            fallback_on_empty: true,
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
            freshness: Default::default(),
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

    async fn deepseek_executor() -> Executor {
        let mut ex = executor(0).await;
        ex.catalog = catalog_with(&[
            ("deepseek", "FALLBACK_TEST_DEEPSEEK", 1),
            ("staan", "FALLBACK_TEST_STAAN", 2),
        ])
        .await;
        ex.providers.insert(
            "deepseek".into(),
            Arc::new(providers::deepseek::DeepSeekProvider::default()),
        );
        let storage = ex.catalog.storage();
        let group = storage
            .create_group(proviz_core::models::Group {
                id: Uuid::new_v4().to_string(),
                slug: "paid-mapping".into(),
                name: "Paid mapping".into(),
                description: None,
                is_active: true,
                created_at: String::new(),
            })
            .await
            .unwrap();
        for key in storage.list_api_keys().await.unwrap() {
            storage
                .add_group_member(
                    &group.id,
                    &key.id,
                    if key.label == "deepseek" { 1 } else { 2 },
                )
                .await
                .unwrap();
        }
        ex.catalog.reload().await.unwrap();
        ex
    }

    async fn search_router(ex: Executor) -> axum::Router {
        let mut config = crate::config::Config::from_env();
        config.database_url = None;
        config.database_path = ":memory:".into();
        let (_, mut state) = crate::app::build_app(config).await.unwrap();
        state.catalog = ex.catalog.clone();
        state.storage = ex.catalog.storage().clone();
        state.no_cache_providers = Default::default();
        state.executor = Arc::new(ex);
        axum::Router::new()
            .route(
                "/search",
                axum::routing::post(crate::handlers::search::handle_search),
            )
            .with_state(state)
    }

    async fn post_search(
        router: axum::Router,
        request: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        use axum::{
            body::{to_bytes, Body},
            http::Request,
        };
        use tower::ServiceExt;
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/search")
                    .header("content-type", "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn long_deepseek_query_falls_back_to_staan_with_skip_trace() {
        // Each query fits on its own; provider-added operators cross the limit.
        for extra in [
            serde_json::json!({"language":"fr"}),
            serde_json::json!({"date_from":"2026-01-01", "date_to":"2026-10-01"}),
            serde_json::json!({"include_domains":["staan.test"]}),
        ] {
            let ex = deepseek_executor().await;
            let usage = ex.usage.clone();
            let key = ex
                .catalog
                .read()
                .await
                .api_keys
                .iter()
                .find(|k| k.label == "deepseek")
                .unwrap()
                .id
                .clone();
            let mut request = serde_json::json!({
                "query":"é".repeat(125), "group":"paid-mapping", "max_fallbacks":0,
                "cache_ttl_secs":0
            });
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let (status, body) = post_search(search_router(ex).await, request).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["meta"]["provider"], "staan");
            assert!(body["meta"]["fallback_chain"]
                .as_str()
                .unwrap()
                .contains("deepseek:skipped:query_too_long"));
            assert!(body["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["provider"] == "deepseek" && a["error"] == "skipped:query_too_long"));
            assert_eq!(
                usage.rpd_headroom(&key, Some(1)),
                1.0,
                "local refusal must not consume the key's daily budget"
            );
        }
    }

    #[tokio::test]
    async fn all_providers_refusing_returns_sanitized_error_after_fallback() {
        let mut ex = deepseek_executor().await;
        let mock = Arc::new(ControlledProvider::new(400));
        ex.providers.insert("staan".into(), mock.clone());
        let (status, body) = post_search(
            search_router(ex).await,
            serde_json::json!({
                "query":"x".repeat(129), "group":"paid-mapping", "max_fallbacks":0
            }),
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["code"], "request_error");
        let chain = body["fallback_chain"].as_str().unwrap();
        assert!(chain.contains("deepseek:skipped:query_too_long"));
        assert!(chain.contains("staan:skipped:invalid_query"));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        assert_eq!(body["error"], "No provider accepted the query");
    }

    #[tokio::test]
    async fn malformed_or_blank_client_requests_remain_client_errors() {
        let router = search_router(executor(0).await).await;
        for request in [
            serde_json::json!({"query":"  "}),
            serde_json::json!({"n":5}),
        ] {
            use axum::{body::Body, http::Request};
            use tower::ServiceExt;
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/search")
                        .header("content-type", "application/json")
                        .body(Body::from(request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 422);
        }
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
    async fn repeated_empty_answers_leave_the_key_in_rotation() {
        let ex = executor(3).await;
        for _ in 0..3 {
            let r = ex.search(params()).await.unwrap();
            assert_eq!(r.fallback_chain, "staan:empty,exa:ok");
        }
        let fourth = ex.search(params()).await.unwrap();
        assert_eq!(fourth.provider_slug, "staan");
        assert_eq!(fourth.fallback_chain, "staan:ok");
    }

    async fn paced_executor(cap: Option<i64>) -> Executor {
        let ex = executor(0).await;
        let mut key = ex
            .catalog
            .read()
            .await
            .api_keys
            .iter()
            .find(|k| k.label == "staan")
            .unwrap()
            .clone();
        let storage = ex.catalog.storage();
        storage.soft_delete_api_key(&key.id).await.unwrap();
        key.id = Uuid::new_v4().to_string();
        key.rps_limit = Some(20.0);
        key.max_queue_ms = cap;
        storage.create_api_key(key).await.unwrap();
        let group = storage
            .create_group(proviz_core::models::Group {
                id: Uuid::new_v4().to_string(),
                slug: "paced".into(),
                name: "paced".into(),
                description: None,
                is_active: true,
                created_at: String::new(),
            })
            .await
            .unwrap();
        for key in storage
            .list_api_keys()
            .await
            .unwrap()
            .into_iter()
            .filter(|k| k.is_active)
        {
            storage
                .add_group_member(&group.id, &key.id, if key.label == "staan" { 1 } else { 2 })
                .await
                .unwrap();
        }
        ex.catalog.reload().await.unwrap();
        ex
    }

    fn paced_params() -> SearchParams {
        SearchParams {
            group_slug: Some("paced".into()),
            timeout_ms: 10_000,
            max_fallbacks: 0,
            ..params()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sixty_main_searches_stay_on_key_with_sufficient_queue_window() {
        let ex = Arc::new(paced_executor(Some(3200)).await);
        let mut tasks = tokio::task::JoinSet::new();
        let start = Instant::now();
        for _ in 0..60 {
            let ex = ex.clone();
            tasks.spawn(async move { ex.search(paced_params()).await.unwrap() });
        }
        let mut results = vec![];
        while let Some(r) = tasks.join_next().await {
            results.push(r.unwrap());
        }
        assert!(results.iter().all(|r| r.provider_slug == "staan"));
        assert!(start.elapsed() >= Duration::from_millis(3105));
        assert!(start.elapsed() < Duration::from_millis(3200));
        assert!(results.iter().all(|r| r.fallback_chain == "staan:ok"));
    }

    #[tokio::test(start_paused = true)]
    async fn overflow_reaches_next_tier_without_spending_attempt_budget() {
        let ex = Arc::new(paced_executor(Some(3000)).await);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..120 {
            let ex = ex.clone();
            tasks.spawn(async move { ex.search(paced_params()).await.unwrap() });
        }
        let mut primary = 0;
        let mut fallback = 0;
        while let Some(r) = tasks.join_next().await {
            let r = r.unwrap();
            if r.provider_slug == "staan" {
                primary += 1;
            } else {
                assert_eq!(r.provider_slug, "exa");
                assert!(r
                    .fallback_chain
                    .starts_with("staan:skipped:budget_exhausted"));
                fallback += 1;
            }
        }
        assert_eq!((primary, fallback), (58, 62));
    }

    #[tokio::test(start_paused = true)]
    async fn short_deadline_overflows_without_waiting_and_null_queue_skips_as_before() {
        for cap in [Some(3000), None, Some(0)] {
            let ex = paced_executor(cap).await;
            let key = ex
                .catalog
                .read()
                .await
                .api_keys
                .iter()
                .find(|k| k.label == "staan" && k.is_active)
                .unwrap()
                .clone();
            if cap == Some(3000) {
                let deadline = Instant::now() + Duration::from_secs(10);
                let slots: Vec<_> = (0..20)
                    .map(|_| ex.pacer.reserve(&key, Priority::Main, deadline).unwrap())
                    .collect();
                drop(slots);
            } else {
                for _ in 0..20 {
                    ex.usage.reserve(&key.id);
                    ex.usage.complete(&key.id);
                }
            }
            let start = Instant::now();
            let request = SearchParams {
                timeout_ms: 100,
                ..paced_params()
            };
            let result = ex.search(request).await.unwrap();
            assert_eq!(result.provider_slug, "exa");
            assert!(result
                .fallback_chain
                .contains("staan:skipped:budget_exhausted"));
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    #[tokio::test]
    async fn no_fallback_on_empty_stops_for_both_successful_empty_and_empty_error() {
        for status in [0, 1] {
            let mut ex = executor(0).await;
            let mock = Arc::new(ControlledProvider::new(status));
            ex.providers.insert("staan".into(), mock.clone());
            let request = SearchParams {
                fallback_on_empty: false,
                ..params()
            };
            let result = ex.search(request).await.unwrap();
            assert_eq!(result.provider_slug, "staan");
            assert!(result.results.is_empty());
            assert_eq!(result.fallback_chain, "staan:empty");
            assert_eq!(result.attempts.len(), 1);
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.log.success, Some(true));
            assert!(ex.rate_limit.parked_keys().is_empty());
        }
    }

    #[tokio::test]
    async fn http_empty_policy_partitions_cache_and_health_exposes_queue_metrics() {
        use axum::{
            body::{to_bytes, Body},
            http::{Request, StatusCode},
            routing::{get, post},
            Router,
        };
        use tower::ServiceExt;
        let mut ex = paced_executor(Some(3000)).await;
        let mock = Arc::new(ControlledProvider::new(0));
        ex.providers.insert("staan".into(), mock.clone());
        let mut config = crate::config::Config::from_env();
        config.database_url = None;
        config.database_path = ":memory:".into();
        let (_, mut state) = crate::app::build_app(config).await.unwrap();
        state.catalog = ex.catalog.clone();
        state.storage = ex.catalog.storage().clone();
        state.rate_limit = ex.rate_limit.clone();
        state.usage = ex.usage.clone();
        state.no_cache_providers = Default::default();
        state.executor = Arc::new(ex);
        let key = state
            .catalog
            .read()
            .await
            .api_keys
            .iter()
            .find(|k| k.label == "staan" && k.is_active)
            .unwrap()
            .clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        let first = state
            .executor
            .pacer
            .reserve(&key, Priority::Main, deadline)
            .unwrap();
        let waiting = state
            .executor
            .pacer
            .reserve(&key, Priority::Main, deadline)
            .unwrap();
        let router = Router::new()
            .route("/search", post(crate::handlers::search::handle_search))
            .route("/health", get(crate::handlers::health::handle_health))
            .with_state(state);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let health: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let keys = health["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["slug"] == "staan")
            .unwrap()["keys"]
            .as_array()
            .unwrap();
        let metrics = keys.iter().find(|k| k["id"] == key.id).unwrap();
        assert_eq!(metrics["queue_depth"], 1);
        assert_eq!(metrics["queued"], 1);
        assert!(metrics["next_slot_delay_ms"].as_u64().unwrap() > 0);
        assert!(metrics["wait_ms"].as_u64().unwrap() > 0);
        assert_eq!(metrics["overflow"], 0);
        drop((first, waiting));
        for (fallback, provider, empty, cached) in [
            (false, "staan", true, false),
            (true, "exa", false, false),
            (false, "staan", true, true),
        ] {
            let body =
                serde_json::json!({"query":"test", "group":"paced", "fallback_on_empty":fallback});
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/search")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["results"].as_array().unwrap().is_empty(), empty);
            assert_eq!(body["meta"]["provider"], provider);
            assert_eq!(body["meta"]["cache_hit"], cached);
            if !fallback && !cached {
                assert_eq!(body["meta"]["fallback_chain"], "staan:empty");
            }
        }
        assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn disabling_empty_fallback_still_falls_back_on_provider_errors() {
        for status in [401, 402, 429, 503] {
            let mut ex = executor(0).await;
            ex.providers
                .insert("staan".into(), Arc::new(ControlledProvider::new(status)));
            let result = ex
                .search(SearchParams {
                    fallback_on_empty: false,
                    ..params()
                })
                .await
                .unwrap();
            assert_eq!(result.provider_slug, "exa");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn paced_retries_do_not_exceed_daily_budget() {
        let mut ex = paced_executor(Some(3000)).await;
        let key = ex
            .catalog
            .read()
            .await
            .api_keys
            .iter()
            .find(|k| k.label == "staan" && k.is_active)
            .unwrap()
            .clone();
        ex.catalog
            .storage()
            .update_api_key_fields(
                &key.id,
                None,
                None,
                None,
                None,
                Some(Some(1)),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        ex.catalog.reload().await.unwrap();
        let mock = Arc::new(ControlledProvider::new(503));
        ex.providers.insert("staan".into(), mock.clone());
        let result = ex
            .search(SearchParams {
                max_fallbacks: 1,
                ..paced_params()
            })
            .await
            .unwrap();
        assert_eq!(result.provider_slug, "exa");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn total_search_deadline_bounds_provider_timeout_and_retry() {
        let mut ex = paced_executor(Some(3000)).await;
        ex.providers.insert(
            "staan".into(),
            Arc::new(ControlledProvider {
                delay_ms: 1000,
                ..ControlledProvider::new(503)
            }),
        );
        let start = Instant::now();
        assert!(ex
            .search(SearchParams {
                timeout_ms: 100,
                ..paced_params()
            })
            .await
            .is_err());
        assert!(start.elapsed() <= Duration::from_millis(100));
        assert_eq!(
            ex.usage.rps_headroom(
                &ex.catalog
                    .read()
                    .await
                    .api_keys
                    .iter()
                    .find(|k| k.label == "staan" && k.is_active)
                    .unwrap()
                    .id,
                Some(20.0)
            ),
            0.95
        );
    }

    /// Free-engine stand-in: ignores `include_domains`, records the query text it was
    /// sent, answers pages on and off the requested domains.
    /// (query text, include_domains, freshness) of one call.
    type SeenCall = (String, Vec<String>, Option<operators::Freshness>);

    struct IgnoresDomains {
        native: bool,
        seen: std::sync::Mutex<Vec<SeenCall>>,
    }

    #[async_trait]
    impl SearchProvider for IgnoresDomains {
        fn slug(&self) -> &str {
            "stub"
        }
        fn supports_include_domains(&self) -> bool {
            self.native
        }
        async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
            self.seen.lock().unwrap().push((
                q.query.to_string(),
                q.include_domains.to_vec(),
                q.freshness.cloned(),
            ));
            let r = |url: &str, rank| SearchResult {
                url: url.into(),
                title: "t".into(),
                snippet: "s".into(),
                domain: String::new(),
                rank,
                published_date: None,
                language: None,
                full_content: None,
                extra_snippets: None,
            };
            Ok(SearchOutput::new(vec![
                r("https://www.latimes.com/a", 0),
                r("https://www.insee.fr/fr/statistiques/1", 1),
                r("https://www.lemonde.fr/les-decodeurs/article/x.html", 2),
                r("https://www.lemonde.fr/sport/x.html", 3),
            ]))
        }
    }

    async fn stub_executor(native: bool) -> (Executor, Arc<IgnoresDomains>) {
        let catalog = catalog_with(&[("stub", "EXEC_TEST_STUB", 1)]).await;
        let stub = Arc::new(IgnoresDomains {
            native,
            seen: Default::default(),
        });
        let mut map: HashMap<String, Arc<dyn SearchProvider>> = HashMap::new();
        map.insert("stub".into(), stub.clone());
        let rl = RateLimitState::default();
        let usage = UsageTracker::default();
        let selector = Arc::new(Selector::new(
            rl.clone(),
            usage.clone(),
            ProfileMatcher::new(vec![]),
        ));
        (
            Executor::new(
                catalog,
                selector,
                map,
                rl,
                usage,
                PathBuf::from("/nonexistent"),
                Arc::new(StatsTracker::new()),
            ),
            stub,
        )
    }

    #[tokio::test]
    async fn include_domains_become_site_operators_for_an_engine_that_ignores_them() {
        let (ex, stub) = stub_executor(false).await;
        let mut p = params();
        p.query = "taux de croissance 1962".into();
        p.include_domains = vec!["insee.fr".into(), "lemonde.fr/les-decodeurs".into()];
        p.freshness = operators::Freshness::from_request(Some("year"), None, None);
        let out = ex.search(p).await.unwrap();
        let urls: Vec<&str> = out.results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.insee.fr/fr/statistiques/1",
                "https://www.lemonde.fr/les-decodeurs/article/x.html"
            ]
        );
        let seen = stub.seen.lock().unwrap();
        assert_eq!(
            seen[0].0,
            "taux de croissance 1962 (site:insee.fr OR site:lemonde.fr)"
        );
        assert!(seen[0].1.is_empty(), "the engine gets no structured list");
        assert_eq!(
            seen[0].2.as_ref().and_then(|f| f.recency),
            Some(operators::Recency::Year)
        );
    }

    #[tokio::test]
    async fn a_native_provider_gets_hosts_and_the_plain_query() {
        let (ex, stub) = stub_executor(true).await;
        let mut p = params();
        p.query = "élèves 6e lire".into();
        p.include_domains = vec!["lemonde.fr/les-decodeurs".into()];
        let out = ex.search(p).await.unwrap();
        assert_eq!(
            out.results.len(),
            1,
            "the path is still enforced by the filter"
        );
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen[0].0, "élèves 6e lire");
        assert_eq!(seen[0].1, vec!["lemonde.fr".to_string()]);
        assert!(seen[0].2.is_none());
    }

    #[tokio::test]
    async fn nothing_inside_the_domains_is_an_empty_answer() {
        let (ex, _) = stub_executor(false).await;
        let mut p = params();
        p.include_domains = vec!["ined.fr".into()];
        assert!(ex.search(p).await.is_err());
    }
    struct ControlledProvider {
        status: std::sync::atomic::AtomicU16,
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        delay_ms: u64,
        seen: std::sync::Mutex<Vec<(String, usize)>>,
    }

    impl ControlledProvider {
        fn new(status: u16) -> Self {
            Self {
                status: std::sync::atomic::AtomicU16::new(status),
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                delay_ms: 0,
                seen: Default::default(),
            }
        }
    }

    #[async_trait]
    impl SearchProvider for ControlledProvider {
        fn slug(&self) -> &str {
            "controlled"
        }
        async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push((q.query.into(), q.n));
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            match self.status.load(Ordering::SeqCst) {
                0 => Ok(SearchOutput::new(vec![])), // empty successful API answer
                1 => Err(ProviderError::Empty),
                status => Err(ProviderError::Http {
                    status,
                    message: if status == 400 {
                        r#"{"message":["include_domains contains an invalid domain"],"statusCode":400}"#.into()
                    } else {
                        "payment_required".into()
                    },
                }),
            }
        }
    }

    fn controlled_executor(catalog: CatalogStore, mock: Arc<ControlledProvider>) -> Executor {
        let rl = RateLimitState::default().with_billing_cooldown(1);
        let usage = UsageTracker::default();
        Executor::new(
            catalog,
            Arc::new(Selector::new(
                rl.clone(),
                usage.clone(),
                ProfileMatcher::new(vec![]),
            )),
            HashMap::from([("staan".into(), mock as Arc<dyn SearchProvider>)]),
            rl,
            usage,
            PathBuf::from("/nonexistent"),
            Arc::new(StatsTracker::new()),
        )
    }

    #[tokio::test]
    async fn failed_search_rows_preserve_classification_and_chain_without_query_text() {
        use axum::response::IntoResponse;
        for (status, expected, code) in [
            (402, "billing", 503),
            (400, "request_error", 422),
            (1, "empty", 503),
            (0, "empty", 503),
        ] {
            let dir = tempdir::TempDir::new("sercilo6-search-log").unwrap();
            let path = dir.path().join("test.db");
            let catalog = catalog_in(
                storage_sqlite::Storage::open(&path).unwrap(),
                &[("staan", "AUDIT_TEST_KEY", 1)],
            )
            .await;
            let mock = Arc::new(ControlledProvider::new(status));
            let ex = controlled_executor(catalog, Arc::clone(&mock));
            let mut request = params();
            request.query = "private request text must never reach audit".into();
            let error = ex.search(request).await.err().unwrap();
            assert_eq!(error.status.as_u16(), code);
            if status == 400 {
                assert_eq!(error.message, "No provider accepted the query");
                assert!(!ex
                    .rate_limit
                    .is_limited(&ex.catalog.read().await.api_keys[0].id));
            }
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
            let response = error.into_response();
            if code == 503 {
                assert!(
                    response.headers()["Retry-After"]
                        .to_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        >= 1
                );
            } else {
                assert!(!response.headers().contains_key("Retry-After"));
            }
            let conn = rusqlite::Connection::open(&path).unwrap();
            let (success, kind, chain, hash): (bool, String, String, String) = conn
                .query_row(
                    "SELECT success, error_type, fallback_chain, query_hash FROM search_log",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert!(!success);
            assert_eq!(kind, expected);
            if status == 400 {
                assert!(chain.contains("staan:skipped:invalid_query"));
            } else {
                assert!(chain.contains(&format!("staan:{expected}")));
            }
            assert_eq!(hash, "h");
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM search_log", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            // No key penalties for empty answers or invalid parameters.
            if expected != "billing" {
                assert!(ex.rate_limit.parked_keys().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn upstream_query_refusals_fall_back_without_penalizing_the_key() {
        for status in [400, 422] {
            let mut ex = executor(0).await;
            let mock = Arc::new(ControlledProvider::new(status));
            ex.providers.insert("staan".into(), mock.clone());
            let result = ex
                .search(SearchParams {
                    max_fallbacks: 0,
                    ..params()
                })
                .await
                .unwrap();
            assert_eq!(result.provider_slug, "exa");
            assert!(result
                .fallback_chain
                .contains("staan:skipped:invalid_query"));
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
            assert!(ex.rate_limit.parked_keys().is_empty());
        }
    }

    #[tokio::test]
    async fn retry_after_uses_the_shortest_candidate_cooldown() {
        let ex = executor(0).await;
        let catalog = ex.catalog.read().await;
        let ids: Vec<_> = catalog.api_keys.iter().map(|k| k.id.clone()).collect();
        drop(catalog);
        ex.rate_limit.report_error(&ids[0], ErrorType::Auth);
        ex.rate_limit.report_error(&ids[1], ErrorType::Rpm);
        let error = ex.search(params()).await.err().unwrap();
        assert_eq!(error.retry_after_secs, Some(60));
        let ex = controlled_executor(
            catalog_with(&[]).await,
            Arc::new(ControlledProvider::new(0)),
        );
        let error = ex.search(params()).await.err().unwrap();
        assert_eq!(error.retry_after_secs, Some(1));
        assert_eq!(
            ex.catalog.storage().stats_window(60).await.unwrap(),
            (1, 0, 1)
        );
    }

    #[tokio::test]
    async fn probes_recover_parked_keys_without_user_search_logs() {
        let dir = tempdir::TempDir::new("sercilo6-probes").unwrap();
        let path = dir.path().join("test.db");
        let catalog = catalog_in(
            storage_sqlite::Storage::open(&path).unwrap(),
            &[("staan", "PROBE_TEST_KEY", 1)],
        )
        .await;
        let mock = Arc::new(ControlledProvider::new(402));
        let ex = Arc::new(controlled_executor(catalog.clone(), mock.clone()));
        let id = catalog.read().await.api_keys[0].id.clone();
        assert!(ex.search(params()).await.is_err());
        assert_eq!(ex.rate_limit.parked_keys()[0].reason, ErrorType::Billing);
        let mut config = crate::config::Config::from_env();
        config.probe_backoff_secs = 1;
        config.probe_query = "test".into();
        let runner = Arc::new(crate::probes::ProbeRunner::new(
            ex.clone(),
            catalog,
            ex.rate_limit.clone(),
            Arc::new(config),
        ));
        assert!(!runner.run(&id, true).await.unwrap().success);
        assert!(ex.rate_limit.is_limited(&id));
        assert_eq!(ex.rate_limit.parked_keys()[0].probe_failures, 1);
        mock.status.store(1, Ordering::SeqCst); // healthy API with no matches is also a successful ping
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(ex.rate_limit.is_limited(&id)); // elapsed cooldown alone never re-enables
        runner.run_due().await;
        assert!(!ex.rate_limit.is_limited(&id));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            mock.seen.lock().unwrap()[1..],
            [("test".into(), 1), ("test".into(), 1)]
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM search_log", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        // The billing event is written independently from the search audit.
        tokio::task::yield_now().await;
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM rate_events WHERE event_type='billing'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        // On-demand probes also recover auth-parked keys immediately after repair.
        ex.rate_limit.report_error(&id, ErrorType::Auth);
        assert!(runner.run(&id, true).await.unwrap().success);
        assert!(!ex.rate_limit.is_limited(&id));
    }

    #[tokio::test]
    async fn background_probes_bound_concurrency_and_ignore_disabled_keys() {
        let catalog = catalog_with(&[
            ("p1", "PROBE_BOUND1", 1),
            ("p2", "PROBE_BOUND2", 1),
            ("p3", "PROBE_BOUND3", 1),
            ("p4", "PROBE_BOUND4", 1),
        ])
        .await;
        let mock = Arc::new(ControlledProvider {
            delay_ms: 30,
            ..ControlledProvider::new(0)
        });
        let mut ex = controlled_executor(catalog.clone(), mock.clone());
        for c in catalog.read().await.candidates(None) {
            ex.providers.insert(c.provider.slug, mock.clone());
            ex.rate_limit
                .report_error(&c.api_key.id, ErrorType::Billing);
        }
        let ex = Arc::new(ex);
        let mut config = crate::config::Config::from_env();
        config.probe_concurrency = 2;
        let runner = Arc::new(crate::probes::ProbeRunner::new(
            ex.clone(),
            catalog.clone(),
            ex.rate_limit.clone(),
            Arc::new(config),
        ));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        runner.run_due().await;
        assert_eq!(mock.calls.load(Ordering::SeqCst), 4);
        assert_eq!(mock.max_active.load(Ordering::SeqCst), 2);
        assert!(ex.rate_limit.parked_keys().is_empty());
        let id = catalog.read().await.api_keys[0].id.clone();
        catalog.storage().soft_delete_api_key(&id).await.unwrap();
        catalog.reload().await.unwrap();
        assert_eq!(
            runner.run(&id, true).await.err().unwrap().status.as_u16(),
            404
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 4);
    }
    #[tokio::test]
    async fn timed_out_or_cancelled_probes_release_claims_and_keep_keys_parked() {
        let catalog = catalog_with(&[("staan", "PROBE_TIMEOUT_KEY", 1)]).await;
        let mock = Arc::new(ControlledProvider {
            delay_ms: 100,
            ..ControlledProvider::new(0)
        });
        let ex = Arc::new(controlled_executor(catalog.clone(), mock));
        let id = catalog.read().await.api_keys[0].id.clone();
        ex.rate_limit.report_error(&id, ErrorType::Auth);
        let mut config = crate::config::Config::from_env();
        config.probe_timeout_ms = 5;
        let runner = Arc::new(crate::probes::ProbeRunner::new(
            ex.clone(),
            catalog.clone(),
            ex.rate_limit.clone(),
            Arc::new(config),
        ));
        let result = runner.run(&id, true).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.error_type, Some("timeout"));
        assert_eq!(ex.rate_limit.parked_keys()[0].probe_failures, 0);
        assert!(!ex.rate_limit.parked_keys()[0].probing);
        let mut config = crate::config::Config::from_env();
        config.probe_timeout_ms = 1000;
        let runner = Arc::new(crate::probes::ProbeRunner::new(
            ex.clone(),
            catalog,
            ex.rate_limit.clone(),
            Arc::new(config),
        ));
        // Dropping an in-progress admin request must not strand the key in probing.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), runner.run(&id, true))
                .await
                .is_err()
        );
        assert!(ex.rate_limit.is_limited(&id));
        assert!(!ex.rate_limit.parked_keys()[0].probing);
        assert_eq!(ex.rate_limit.parked_keys()[0].probe_failures, 1);
    }
}
