use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use providers::{ContentProvider, ContentQuery, ProviderError};
use proviz_core::{
    key_resolver::resolve_key,
    models::{Candidate, FullContent},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{DebugDecision, SelectRequest, Selector},
};
use tracing::{debug, warn};

use crate::{catalog::CatalogStore, executor::AttemptRecord, stats::StatsTracker};

/// Inputs for one `/contents` execution. `urls` are already deduped and
/// cache-filtered by the handler — everything here still needs fetching.
pub struct ContentParams {
    pub urls: Vec<String>,
    pub format: String,
    pub objective: Option<String>,
    pub group_slug: Option<String>,
    pub timeout_ms: u64,
    pub max_fallbacks: usize,
    pub debug: bool,
}

/// One page's outcome. `content` is `Some` on success; `error` is `Some` when no
/// provider in the chain could extract it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractedDoc {
    pub url: String,
    pub title: Option<String>,
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<FullContent>,
    /// Provider slug that produced `content`, or "" when unresolved.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct ContentExecutionResult {
    pub docs: Vec<ExtractedDoc>,
    pub providers_used: Vec<String>,
    pub fallback_chain: String,
    pub attempts: Vec<AttemptRecord>,
    pub debug_decisions: Option<Vec<DebugDecision>>,
    pub duration_ms: u64,
}

pub struct ContentExecutor {
    catalog: CatalogStore,
    selector: Arc<Selector>,
    providers: HashMap<String, Arc<dyn ContentProvider>>,
    rate_limit: RateLimitState,
    usage: UsageTracker,
    secrets_dir: PathBuf,
    stats: Arc<StatsTracker>,
}

impl ContentExecutor {
    pub fn new(
        catalog: CatalogStore,
        selector: Arc<Selector>,
        providers: HashMap<String, Arc<dyn ContentProvider>>,
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

    pub async fn fetch(
        &self,
        params: ContentParams,
    ) -> Result<ContentExecutionResult, crate::error::AppError> {
        let start = Instant::now();

        if params.urls.is_empty() {
            return Ok(ContentExecutionResult {
                docs: vec![],
                providers_used: vec![],
                fallback_chain: String::new(),
                attempts: vec![],
                debug_decisions: params.debug.then_some(vec![]),
                duration_ms: 0,
            });
        }

        // Candidate pool: group members (or all active keys), filtered to keys
        // whose provider actually implements ContentProvider.
        let catalog = self.catalog.read().await;
        let mut pool = catalog.candidates(params.group_slug.as_deref());
        drop(catalog);
        pool.retain(|c| self.providers.contains_key(&c.provider.slug));

        if pool.is_empty() {
            return Err(crate::error::AppError::service_unavailable(
                "No content-extraction provider candidates available",
            ));
        }

        // Strict priority tiers when a group set member_priority (same rule as
        // the search executor); otherwise one implicit tier.
        let tiers: Vec<Vec<Candidate>> = if pool.iter().any(|c| c.member_priority.is_some()) {
            let mut by_priority: BTreeMap<i64, Vec<Candidate>> = BTreeMap::new();
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

        let req = SelectRequest::default();

        let mut excluded: Vec<String> = Vec::new();
        let mut unresolved: Vec<String> = params.urls.clone();
        let mut resolved: HashMap<String, ExtractedDoc> = HashMap::new();
        let mut last_reason: HashMap<String, String> = HashMap::new();
        let mut providers_used: Vec<String> = Vec::new();
        let mut chain_parts: Vec<String> = Vec::new();
        let mut attempt_records: Vec<AttemptRecord> = Vec::new();
        let mut all_decisions: Vec<DebugDecision> = Vec::new();
        let mut tier_idx: usize = 0;
        let mut real_attempts: usize = 0;

        loop {
            if unresolved.is_empty() || real_attempts > params.max_fallbacks {
                break;
            }

            let outcome = self
                .selector
                .select(&tiers[tier_idx], &req, &excluded, params.debug);
            if params.debug {
                all_decisions.extend(outcome.decisions.clone());
            }

            let Some(candidate) = outcome.candidate else {
                for d in &outcome.decisions {
                    let reason = d.reason.clone().unwrap_or_else(|| d.outcome.clone());
                    chain_parts.push(format!("{}:skipped:{reason}", d.provider));
                }
                tier_idx += 1;
                if tier_idx >= tiers.len() {
                    break;
                }
                continue;
            };

            real_attempts += 1;
            let slug = candidate.provider.slug.clone();
            // Never re-select this key for the URLs still unresolved after it runs.
            excluded.push(candidate.api_key.id.clone());

            let Some(provider) = self.providers.get(&slug).cloned() else {
                continue;
            };

            let api_key = match resolve_key(&candidate.api_key.key_ref, &self.secrets_dir) {
                Ok(k) => k,
                Err(_) => {
                    chain_parts.push(format!("{slug}:auth"));
                    attempt_records.push(AttemptRecord {
                        provider: slug.clone(),
                        success: false,
                        error: Some("auth".to_string()),
                        duration_ms: 0,
                    });
                    continue;
                }
            };

            let attempt_start = Instant::now();
            let batch: Vec<String> = std::mem::take(&mut unresolved);
            let max_batch = provider.max_batch().max(1);
            let chunks: Vec<Vec<String>> = batch.chunks(max_batch).map(|c| c.to_vec()).collect();

            let mut got_any = false;
            let mut hard_err: Option<ProviderError> = None;

            for (ci, chunk) in chunks.iter().enumerate() {
                self.usage.reserve(&candidate.api_key.id);
                let q = ContentQuery {
                    urls: chunk,
                    format: &params.format,
                    objective: params.objective.as_deref(),
                    api_key: &api_key,
                };
                let res = tokio::time::timeout(
                    Duration::from_millis(params.timeout_ms),
                    provider.fetch(q),
                )
                .await;
                self.usage.complete(&candidate.api_key.id);

                let outcome = match res {
                    Ok(inner) => inner,
                    Err(_) => Err(ProviderError::Timeout),
                };
                match outcome {
                    Ok(output) => {
                        for d in output.docs {
                            got_any = true;
                            resolved.insert(
                                d.url.clone(),
                                ExtractedDoc {
                                    url: d.url,
                                    title: d.title,
                                    published_date: d.published_date,
                                    content: Some(d.content),
                                    source: slug.clone(),
                                    error: None,
                                },
                            );
                        }
                        for (url, reason) in output.failed {
                            last_reason.insert(url.clone(), reason);
                            unresolved.push(url);
                        }
                    }
                    Err(e) => {
                        let reason = e.error_type_str().to_string();
                        for c in &chunks[ci..] {
                            for u in c {
                                last_reason.insert(u.clone(), reason.clone());
                                unresolved.push(u.clone());
                            }
                        }
                        hard_err = Some(e);
                        break;
                    }
                }
            }

            let storage = Arc::clone(self.catalog.storage());
            let kid = candidate.api_key.id.clone();
            tokio::spawn(async move {
                let _ = storage.touch_api_key(&kid).await;
            });

            let dur = attempt_start.elapsed().as_millis() as u64;
            if got_any && !providers_used.contains(&slug) {
                providers_used.push(slug.clone());
            }

            if let Some(e) = hard_err {
                let et = e.error_type_str();
                chain_parts.push(format!("{slug}:{et}"));
                let etype = match et {
                    "rpm" => ErrorType::Rpm,
                    "auth" => ErrorType::Auth,
                    "timeout" => ErrorType::Timeout,
                    "empty" => ErrorType::Empty,
                    _ => ErrorType::Error,
                };
                if et == "auth" {
                    warn!(
                        key_id = candidate.api_key.id,
                        "content provider auth error - key disabled 300s"
                    );
                }
                self.rate_limit.report_error(&candidate.api_key.id, etype);
                let storage = Arc::clone(self.catalog.storage());
                let kid2 = candidate.api_key.id.clone();
                let ets = et.to_string();
                tokio::spawn(async move {
                    let _ = storage.record_rate_event(&kid2, &ets).await;
                });
                self.stats.record_search(&slug, true, dur);
                attempt_records.push(AttemptRecord {
                    provider: slug.clone(),
                    success: got_any,
                    error: Some(et.to_string()),
                    duration_ms: dur,
                });
            } else {
                chain_parts.push(format!("{slug}:ok"));
                self.stats.record_search(&slug, !got_any, dur);
                attempt_records.push(AttemptRecord {
                    provider: slug.clone(),
                    success: got_any,
                    error: (!got_any).then(|| "empty".to_string()),
                    duration_ms: dur,
                });
            }

            // Drop anything now resolved, and dedupe what's left.
            unresolved.retain(|u| !resolved.contains_key(u));
            let mut seen = HashSet::new();
            unresolved.retain(|u| seen.insert(u.clone()));

            debug!(
                provider = slug,
                got_any,
                remaining = unresolved.len(),
                "content attempt complete"
            );
        }

        if !unresolved.is_empty() {
            warn!(
                remaining = unresolved.len(),
                chain = chain_parts.join(","),
                "content chain exhausted with URLs still unresolved"
            );
        }

        let docs = assemble_docs(&params.urls, &mut resolved, &last_reason);

        Ok(ContentExecutionResult {
            docs,
            providers_used,
            fallback_chain: chain_parts.join(","),
            attempts: attempt_records,
            debug_decisions: params.debug.then_some(all_decisions),
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// Re-emit one doc per requested URL, in request order, deduped. A URL with no
/// resolved content becomes an error doc carrying the last failure reason.
fn assemble_docs(
    requested: &[String],
    resolved: &mut HashMap<String, ExtractedDoc>,
    last_reason: &HashMap<String, String>,
) -> Vec<ExtractedDoc> {
    let mut out = Vec::with_capacity(requested.len());
    let mut seen = HashSet::new();
    for u in requested {
        if !seen.insert(u.clone()) {
            continue;
        }
        if let Some(doc) = resolved.remove(u) {
            out.push(doc);
        } else {
            out.push(ExtractedDoc {
                url: u.clone(),
                title: None,
                published_date: None,
                content: None,
                source: String::new(),
                error: Some(
                    last_reason
                        .get(u)
                        .cloned()
                        .unwrap_or_else(|| "no extractor could fetch this URL".to_string()),
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use providers::{ContentDoc, ContentOutput};
    use proviz_core::{
        language_profile::ProfileMatcher,
        models::{ApiKey, Provider},
        storage::StorageBackend,
    };
    use std::collections::HashMap as Map;

    struct FakeProvider {
        slug: &'static str,
        /// URLs this provider can extract; anything else goes to `failed`.
        can: Vec<&'static str>,
        /// If set, every call returns this hard error.
        hard_err: Option<fn() -> ProviderError>,
    }

    #[async_trait]
    impl ContentProvider for FakeProvider {
        fn slug(&self) -> &str {
            self.slug
        }
        fn max_batch(&self) -> usize {
            5
        }
        async fn fetch(&self, q: ContentQuery<'_>) -> Result<ContentOutput, ProviderError> {
            if let Some(mk) = self.hard_err {
                return Err(mk());
            }
            let mut docs = Vec::new();
            let mut failed = Vec::new();
            for u in q.urls {
                if self.can.contains(&u.as_str()) {
                    docs.push(ContentDoc {
                        url: u.clone(),
                        title: Some("t".to_string()),
                        published_date: None,
                        content: FullContent {
                            text: format!("body of {u} via {}", self.slug),
                            format: "markdown".to_string(),
                            length: 10,
                        },
                    });
                } else {
                    failed.push((u.clone(), "not supported".to_string()));
                }
            }
            Ok(ContentOutput { docs, failed })
        }
    }

    async fn storage_with(providers: &[(&str, &str)]) -> Arc<dyn StorageBackend> {
        let s = storage_sqlite::Storage::open_in_memory().unwrap();
        for (slug, key_ref) in providers {
            let p = s
                .create_provider(Provider {
                    id: uuid::Uuid::new_v4().to_string(),
                    slug: slug.to_string(),
                    name: slug.to_string(),
                    base_url: None,
                    is_active: true,
                    priority: 0,
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
        Arc::new(s)
    }

    fn executor(
        catalog: CatalogStore,
        providers: HashMap<String, Arc<dyn ContentProvider>>,
    ) -> ContentExecutor {
        let rl = RateLimitState::default();
        let usage = UsageTracker::default();
        let selector = Arc::new(Selector::new(
            rl.clone(),
            usage.clone(),
            ProfileMatcher::new(vec![]),
        ));
        ContentExecutor::new(
            catalog,
            selector,
            providers,
            rl,
            usage,
            PathBuf::from("/nonexistent"),
            Arc::new(StatsTracker::new()),
        )
    }

    fn params(urls: &[&str]) -> ContentParams {
        ContentParams {
            urls: urls.iter().map(|s| s.to_string()).collect(),
            format: "markdown".to_string(),
            objective: None,
            group_slug: None,
            timeout_ms: 2000,
            max_fallbacks: 3,
            debug: false,
        }
    }

    #[tokio::test]
    async fn per_url_fallback_only_retries_unresolved() {
        std::env::set_var("CE_KEY_A", "x");
        std::env::set_var("CE_KEY_B", "y");
        let storage = storage_with(&[("prov-a", "CE_KEY_A"), ("prov-b", "CE_KEY_B")]).await;
        let catalog = CatalogStore::new(storage).await.unwrap();

        let mut map: HashMap<String, Arc<dyn ContentProvider>> = HashMap::new();
        map.insert(
            "prov-a".to_string(),
            Arc::new(FakeProvider {
                slug: "prov-a",
                can: vec!["https://a.test/1"],
                hard_err: None,
            }),
        );
        map.insert(
            "prov-b".to_string(),
            Arc::new(FakeProvider {
                slug: "prov-b",
                can: vec!["https://a.test/2"],
                hard_err: None,
            }),
        );
        let ex = executor(catalog, map);

        let res = ex
            .fetch(params(&[
                "https://a.test/1",
                "https://a.test/2",
                "https://a.test/3",
            ]))
            .await
            .unwrap();

        let by_url: Map<_, _> = res.docs.iter().map(|d| (d.url.as_str(), d)).collect();
        assert_eq!(by_url["https://a.test/1"].source, "prov-a");
        assert_eq!(by_url["https://a.test/2"].source, "prov-b");
        assert!(by_url["https://a.test/3"].content.is_none());
        assert!(by_url["https://a.test/3"].error.is_some());
        // both providers contributed
        assert_eq!(res.providers_used.len(), 2);
    }

    #[tokio::test]
    async fn hard_error_cools_provider_and_advances_chain() {
        std::env::set_var("CE_KEY_C", "x");
        std::env::set_var("CE_KEY_D", "y");
        let storage = storage_with(&[("prov-c", "CE_KEY_C"), ("prov-d", "CE_KEY_D")]).await;
        let catalog = CatalogStore::new(storage).await.unwrap();

        let mut map: HashMap<String, Arc<dyn ContentProvider>> = HashMap::new();
        map.insert(
            "prov-c".to_string(),
            Arc::new(FakeProvider {
                slug: "prov-c",
                can: vec![],
                hard_err: Some(|| ProviderError::RateLimit),
            }),
        );
        map.insert(
            "prov-d".to_string(),
            Arc::new(FakeProvider {
                slug: "prov-d",
                can: vec!["https://x.test/1"],
                hard_err: None,
            }),
        );
        let ex = executor(catalog, map);

        let res = ex.fetch(params(&["https://x.test/1"])).await.unwrap();
        assert_eq!(res.docs[0].source, "prov-d");
        assert!(res.fallback_chain.contains("prov-c:rpm"));
        assert!(res.fallback_chain.contains("prov-d:ok"));
    }
}
