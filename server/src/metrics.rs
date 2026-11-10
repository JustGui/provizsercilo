//! Prometheus counters for upstream provider calls, served as text at `GET /metrics`.
//!
//! Reasons use the same vocabulary as proviz-elekto (`credits_exhausted`, `quota`,
//! `rate_limited`, `auth`, `bad_request`, `timeout`, `server`) so one alert rule matches both
//! gateways. Labels are provider slugs from the catalog and a fixed `kind`; no query text,
//! provider body or credential ever becomes a label.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};

use proviz_core::rate_limit::{ErrorType, ParkedKey};

/// Upstream call kind: a search or a content fetch.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Search,
    Contents,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Contents => "contents",
        }
    }
}

/// Map sercilo's cooldown classes onto the shared failure-reason vocabulary.
pub fn reason(et: ErrorType) -> &'static str {
    match et {
        ErrorType::Billing => "credits_exhausted",
        ErrorType::Rpd => "quota",
        ErrorType::Rps | ErrorType::Rpm => "rate_limited",
        ErrorType::Auth => "auth",
        ErrorType::Request => "bad_request",
        ErrorType::Timeout => "timeout",
        ErrorType::Empty => "empty",
        ErrorType::Error => "server",
    }
}

const MAX_SERIES: usize = 2000;

#[derive(Default)]
struct Counters {
    /// (provider, kind, outcome) -> count; outcome is ok | empty | error.
    calls: BTreeMap<(String, &'static str, &'static str), u64>,
    /// (provider, kind, reason) -> count.
    failures: BTreeMap<(String, &'static str, &'static str), u64>,
}

static COUNTERS: LazyLock<Mutex<Counters>> = LazyLock::new(Default::default);

fn bump(
    map: &mut BTreeMap<(String, &'static str, &'static str), u64>,
    key: (String, &'static str, &'static str),
) {
    if let Some(n) = map.get_mut(&key) {
        *n += 1;
    } else if map.len() < MAX_SERIES {
        map.insert(key, 1);
    }
}

fn label(v: &str) -> String {
    let s: String = v
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() {
        "none".into()
    } else {
        s
    }
}

/// An upstream call that returned results (`empty = false`) or none (`empty = true`).
pub fn record_ok(provider: &str, kind: Kind, empty: bool) {
    let mut c = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = if empty { "empty" } else { "ok" };
    bump(&mut c.calls, (label(provider), kind.as_str(), outcome));
}

/// A failed upstream call. An empty answer is not a failure: it counts as `outcome="empty"`.
pub fn record_error(provider: &str, kind: Kind, et: ErrorType) {
    if et == ErrorType::Empty {
        return record_ok(provider, kind, true);
    }
    let mut c = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let provider = label(provider);
    bump(&mut c.calls, (provider.clone(), kind.as_str(), "error"));
    bump(&mut c.failures, (provider, kind.as_str(), reason(et)));
}

/// Prometheus text format. `parked` is the current parked-key list with each key's provider
/// slug, rendered as the `sercilo_keys_parked{provider,reason}` gauge.
pub fn render(parked: &[(String, ParkedKey)]) -> String {
    let mut out = String::new();
    let c = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(
        out,
        "# HELP sercilo_provider_calls_total Upstream provider calls by outcome (ok|empty|error)."
    );
    let _ = writeln!(out, "# TYPE sercilo_provider_calls_total counter");
    for ((p, k, o), n) in &c.calls {
        let _ = writeln!(
            out,
            "sercilo_provider_calls_total{{provider=\"{p}\",kind=\"{k}\",outcome=\"{o}\"}} {n}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP sercilo_provider_failures_total Failed upstream provider calls by failure reason."
    );
    let _ = writeln!(out, "# TYPE sercilo_provider_failures_total counter");
    for ((p, k, r), n) in &c.failures {
        let _ = writeln!(
            out,
            "sercilo_provider_failures_total{{provider=\"{p}\",kind=\"{k}\",reason=\"{r}\"}} {n}"
        );
    }
    drop(c);

    let mut gauges: BTreeMap<(String, &'static str), u64> = BTreeMap::new();
    for (provider, key) in parked {
        *gauges
            .entry((label(provider), reason(key.reason)))
            .or_default() += 1;
    }
    let _ = writeln!(out, "# HELP sercilo_keys_parked Keys currently in cooldown or parked pending a probe, by reason.");
    let _ = writeln!(out, "# TYPE sercilo_keys_parked gauge");
    for ((p, r), n) in &gauges {
        let _ = writeln!(
            out,
            "sercilo_keys_parked{{provider=\"{p}\",reason=\"{r}\"}} {n}"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_renders_with_shared_reasons() {
        record_error("metrics-test-exa", Kind::Search, ErrorType::Billing);
        record_error("metrics-test-exa", Kind::Search, ErrorType::Empty);
        record_ok("metrics-test-exa", Kind::Search, false);
        record_error("metrics-test-exa", Kind::Contents, ErrorType::Rpd);
        let parked = vec![(
            "metrics-test-exa".to_string(),
            ParkedKey {
                key_id: "k1".into(),
                reason: ErrorType::Billing,
                since: String::new(),
                retry_after_secs: 10,
                probe_failures: 0,
                probing: false,
            },
        )];
        let text = render(&parked);
        for line in [
            "sercilo_provider_failures_total{provider=\"metrics-test-exa\",kind=\"search\",reason=\"credits_exhausted\"} 1",
            "sercilo_provider_calls_total{provider=\"metrics-test-exa\",kind=\"search\",outcome=\"error\"} 1",
            "sercilo_provider_calls_total{provider=\"metrics-test-exa\",kind=\"search\",outcome=\"empty\"} 1",
            "sercilo_provider_calls_total{provider=\"metrics-test-exa\",kind=\"search\",outcome=\"ok\"} 1",
            "sercilo_provider_failures_total{provider=\"metrics-test-exa\",kind=\"contents\",reason=\"quota\"} 1",
            "sercilo_keys_parked{provider=\"metrics-test-exa\",reason=\"credits_exhausted\"} 1",
        ] {
            assert!(text.contains(line), "missing {line}\n{text}");
        }
        assert!(!text.contains("reason=\"empty\""));
    }

    #[test]
    fn labels_are_bounded() {
        assert_eq!(label("ddg-yandex"), "ddg-yandex");
        assert_eq!(label("a\"b\nc"), "a_b_c");
        assert_eq!(label(""), "none");
    }
}
