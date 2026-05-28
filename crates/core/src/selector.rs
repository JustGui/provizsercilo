use serde::Serialize;

use crate::{
    language_profile::ProfileMatcher,
    models::Candidate,
    rate_limit::{RateLimitState, UsageTracker},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SelectRequest {
    pub language: Option<String>,
    pub country: Option<String>,
    pub exclude_key_ids: Vec<String>,
    pub exclude_provider_slugs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoreComponents {
    pub fast_headroom: f64,
    pub slow_headroom: f64,
    pub priority: f64,
    pub coverage: f64,
    pub latency: f64,
    pub traffic_balance: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    ProviderInactive,
    KeyInactive,
    RateLimitCooldown,
    ExcludedByRequest,
    ExcludedByProfile,
}

#[derive(Debug, Clone, Serialize)]
pub struct DebugDecision {
    pub provider: String,
    pub key_label: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_components: Option<ScoreComponents>,
}

// ---------------------------------------------------------------------------
// Selector
// ---------------------------------------------------------------------------

pub struct Selector {
    pub rate_limit: RateLimitState,
    pub usage: UsageTracker,
    pub profiles: ProfileMatcher,
}

impl Selector {
    pub fn new(rate_limit: RateLimitState, usage: UsageTracker, profiles: ProfileMatcher) -> Self {
        Self {
            rate_limit,
            usage,
            profiles,
        }
    }

    /// Select the best candidate from `pool`, respecting exclusions from `req`
    /// and the `extra_excludes` list (accumulated during fallback).
    ///
    /// Returns the winning Candidate plus optional debug trace.
    pub fn select(
        &self,
        pool: &[Candidate],
        req: &SelectRequest,
        extra_excludes: &[String],
        debug: bool,
    ) -> Option<(Candidate, Vec<DebugDecision>)> {
        let lang = req.language.as_deref();
        let cty = req.country.as_deref();
        let profile = self.profiles.find(lang, cty);

        let excluded_providers: std::collections::HashSet<&str> = profile
            .iter()
            .flat_map(|p| p.exclude.iter().map(|s| s.as_str()))
            .chain(req.exclude_provider_slugs.iter().map(|s| s.as_str()))
            .collect();

        let mut decisions: Vec<DebugDecision> = Vec::new();
        let mut scored: Vec<(Candidate, f64, ScoreComponents)> = Vec::new();

        // Pass 1 + 2: hard filters and collect eligible candidates with raw stats.
        let mut raw: Vec<CandidateStats> = Vec::new();
        for c in pool {
            // Hard filters
            if !c.provider.is_active {
                if debug {
                    decisions.push(skip_decision(c, "provider_inactive", None, None));
                }
                continue;
            }
            if !c.api_key.is_active {
                if debug {
                    decisions.push(skip_decision(c, "key_inactive", None, None));
                }
                continue;
            }
            if extra_excludes.contains(&c.api_key.id) || req.exclude_key_ids.contains(&c.api_key.id)
            {
                if debug {
                    decisions.push(skip_decision(c, "excluded_by_request", None, None));
                }
                continue;
            }
            if self.rate_limit.is_limited(&c.api_key.id) {
                let remaining = self.rate_limit.cooldown_remaining_ms(&c.api_key.id);
                if debug {
                    decisions.push(skip_decision(c, "rpm_cooldown", None, Some(remaining)));
                }
                continue;
            }
            if excluded_providers.contains(c.provider.slug.as_str()) {
                if debug {
                    decisions.push(skip_decision(
                        c,
                        "profile_excluded",
                        Some(format!(
                            "language={} excluded by profile",
                            lang.unwrap_or("*")
                        )),
                        None,
                    ));
                }
                continue;
            }

            // Gather stats for scoring
            let fast_headroom = self
                .usage
                .rpm_headroom(&c.api_key.id, c.api_key.rpm_limit)
                .min(self.usage.rps_headroom(&c.api_key.id, c.api_key.rps_limit));
            let slow_headroom = self.usage.rpd_headroom(&c.api_key.id, c.api_key.rpd_limit);
            let five_min = self.usage.five_min_count(&c.api_key.id);

            // Coverage: from provider's coverage_scores map, boosted by profile position.
            let base_coverage = lookup_coverage_score(&c.provider.coverage_scores, lang, cty);
            let profile_boost = profile
                .map(|p| ProfileMatcher::coverage_boost(p, &c.provider.slug))
                .unwrap_or(0.0);
            let coverage = (base_coverage + profile_boost).min(1.0);

            raw.push(CandidateStats {
                candidate: c.clone(),
                fast_headroom,
                slow_headroom,
                coverage,
                five_min,
            });
        }

        if raw.is_empty() {
            return None;
        }

        // Normalisation helpers
        let max_priority = raw
            .iter()
            .map(|r| r.candidate.effective_priority())
            .max()
            .unwrap_or(0) as f64;
        let min_priority = raw
            .iter()
            .map(|r| r.candidate.effective_priority())
            .min()
            .unwrap_or(0) as f64;
        let max_latency = raw
            .iter()
            .filter_map(|r| r.candidate.provider.avg_latency_ms.map(|l| l as f64))
            .fold(0.0_f64, f64::max);
        let max_5m = raw.iter().map(|r| r.five_min).max().unwrap_or(0) as f64;

        // Pass 3: score
        for r in &raw {
            let priority_score = norm_inv(
                r.candidate.effective_priority() as f64,
                min_priority,
                max_priority,
            );
            let latency_score = r
                .candidate
                .provider
                .avg_latency_ms
                .map(|l| norm_inv(l as f64, 0.0, max_latency.max(1.0)))
                .unwrap_or(0.5);
            let traffic_balance = if max_5m > 0.0 {
                1.0 - (r.five_min as f64 / max_5m)
            } else {
                1.0
            };

            let components = ScoreComponents {
                fast_headroom: r.fast_headroom,
                slow_headroom: r.slow_headroom,
                priority: priority_score,
                coverage: r.coverage,
                latency: latency_score,
                traffic_balance,
            };

            let score = 0.25 * components.fast_headroom
                + 0.20 * components.slow_headroom
                + 0.20 * components.priority
                + 0.15 * components.coverage
                + 0.10 * components.latency
                + 0.10 * components.traffic_balance;

            scored.push((r.candidate.clone(), score, components));
        }

        // Pick winner
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let (winner, score, components) = scored.remove(0);

        if debug {
            // Add skipped candidates first
            for (c, s, comp) in &scored {
                decisions.push(DebugDecision {
                    provider: c.provider.slug.clone(),
                    key_label: c.api_key.label.clone(),
                    outcome: "evaluated".to_string(),
                    reason: None,
                    detail: None,
                    cooldown_remaining_ms: None,
                    score: Some(*s),
                    score_components: Some(comp.clone()),
                });
            }
            decisions.push(DebugDecision {
                provider: winner.provider.slug.clone(),
                key_label: winner.api_key.label.clone(),
                outcome: "selected".to_string(),
                reason: None,
                detail: None,
                cooldown_remaining_ms: None,
                score: Some(score),
                score_components: Some(components),
            });
        }

        Some((winner, decisions))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct CandidateStats {
    candidate: Candidate,
    fast_headroom: f64,
    slow_headroom: f64,
    coverage: f64,
    five_min: usize,
}

fn norm_inv(val: f64, min: f64, max: f64) -> f64 {
    if (max - min).abs() < f64::EPSILON {
        return 0.5;
    }
    let norm = (val - min) / (max - min);
    1.0 - norm
}

/// Look up coverage score for a (language, country) pair.
/// Key tried in order: "fr_fr" > "fr" > default 0.5.
fn lookup_coverage_score(
    scores: &std::collections::HashMap<String, f64>,
    lang: Option<&str>,
    country: Option<&str>,
) -> f64 {
    if let (Some(l), Some(c)) = (lang, country) {
        let full_key = format!("{}_{}", l, c);
        if let Some(&v) = scores.get(&full_key) {
            return v;
        }
    }
    if let Some(l) = lang {
        if let Some(&v) = scores.get(l) {
            return v;
        }
    }
    0.5
}

fn skip_decision(
    c: &Candidate,
    reason: &str,
    detail: Option<String>,
    cooldown_ms: Option<u64>,
) -> DebugDecision {
    DebugDecision {
        provider: c.provider.slug.clone(),
        key_label: c.api_key.label.clone(),
        outcome: "skipped".to_string(),
        reason: Some(reason.to_string()),
        detail,
        cooldown_remaining_ms: cooldown_ms,
        score: None,
        score_components: None,
    }
}
