use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    Rps,
    Rpm,
    Rpd,
    Auth,
    Billing,
    Request,
    Timeout,
    Empty,
    Error,
}

impl ErrorType {
    pub fn cooldown_secs(self) -> u64 {
        match self {
            Self::Rps => 5,
            Self::Rpm => 60,
            Self::Rpd => 3600,
            Self::Auth => 300,
            Self::Billing => 21600,
            Self::Request => 0,
            Self::Timeout => 30,
            Self::Empty => 0,
            Self::Error => 60,
        }
    }

    /// Only repeated transport/provider failures count against key health.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Timeout | Self::Error)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rps => "rps",
            Self::Rpm => "rpm",
            Self::Rpd => "rpd",
            Self::Auth => "auth",
            Self::Billing => "billing",
            Self::Request => "request",
            Self::Timeout => "timeout",
            Self::Empty => "empty",
            Self::Error => "error",
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimitState - reactive cooldowns
// ---------------------------------------------------------------------------

/// Consecutive transient failures of one key before it is cooled down. A single slow or
/// failed answer used to black out the key for 60 s; with one key per provider that sent
/// every search down the cascade to the paid fallbacks (Staan -> Exa, 2026-09-25).
pub const DEFAULT_TRANSIENT_FAILURES_BEFORE_COOLDOWN: u32 = 3;

/// A run of transient failures only counts while they keep coming: one older than this
/// starts a new run.
const TRANSIENT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct Cooldown {
    until: Instant,
    reason: ErrorType,
    since: String,
    failures: u32,
    probing: bool,
    generation: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParkedKey {
    pub key_id: String,
    pub reason: ErrorType,
    pub since: String,
    pub retry_after_secs: u64,
    pub probe_failures: u32,
    pub probing: bool,
}

#[derive(Debug, Clone)]
pub struct RateLimitState {
    cooldowns: Arc<DashMap<String, Cooldown>>,
    transient: Arc<DashMap<String, (u32, Instant)>>,
    transient_threshold: u32,
    billing_cooldown_secs: u64,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::with_transient_threshold(DEFAULT_TRANSIENT_FAILURES_BEFORE_COOLDOWN)
    }
}

impl RateLimitState {
    pub fn with_transient_threshold(threshold: u32) -> Self {
        Self {
            cooldowns: Arc::new(DashMap::new()),
            transient: Arc::new(DashMap::new()),
            transient_threshold: threshold.max(1),
            billing_cooldown_secs: ErrorType::Billing.cooldown_secs(),
        }
    }

    pub fn with_billing_cooldown(mut self, seconds: u64) -> Self {
        self.billing_cooldown_secs = seconds.max(1);
        self
    }

    /// Expiry makes a probe due; only a successful probe releases the key.
    pub fn is_limited(&self, api_key_id: &str) -> bool {
        self.cooldowns.contains_key(api_key_id)
    }

    pub fn report_error(&self, api_key_id: &str, error_type: ErrorType) -> bool {
        if matches!(error_type, ErrorType::Empty | ErrorType::Request) {
            // An empty answer is healthy, and ends any run of failures. An invalid
            // request says nothing about key health and leaves the count alone.
            if error_type == ErrorType::Empty {
                self.report_success(api_key_id);
            }
            return false;
        }
        if error_type.is_transient() && self.transient_threshold > 1 {
            let now = Instant::now();
            let mut entry = self
                .transient
                .entry(api_key_id.to_string())
                .or_insert((0, now));
            if now.duration_since(entry.1) > TRANSIENT_WINDOW {
                entry.0 = 0;
            }
            entry.0 += 1;
            entry.1 = now;
            if entry.0 < self.transient_threshold {
                return false;
            }
            entry.0 = 0;
        }
        let seconds = if error_type == ErrorType::Billing {
            self.billing_cooldown_secs
        } else {
            error_type.cooldown_secs()
        };
        let mut entry = self
            .cooldowns
            .entry(api_key_id.to_string())
            .or_insert_with(|| Cooldown {
                until: Instant::now(),
                reason: error_type,
                since: chrono::Utc::now().to_rfc3339(),
                failures: 0,
                probing: false,
                generation: 0,
            });
        entry.until = Instant::now() + Duration::from_secs(seconds);
        entry.reason = error_type;
        entry.generation += 1; // invalidates a probe racing a new user-call failure
        entry.probing = false;
        true
    }

    /// A late successful user request must never release an already parked key.
    pub fn report_success(&self, api_key_id: &str) {
        self.transient.remove(api_key_id);
    }

    pub fn cooldown_remaining_ms(&self, api_key_id: &str) -> u64 {
        self.cooldowns
            .get(api_key_id)
            .map(|e| {
                e.until
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64
            })
            .unwrap_or(0)
    }

    pub fn cooldown_reason(&self, api_key_id: &str) -> Option<ErrorType> {
        self.cooldowns.get(api_key_id).map(|e| e.reason)
    }

    pub fn parked_keys(&self) -> Vec<ParkedKey> {
        self.cooldowns
            .iter()
            .map(|e| ParkedKey {
                key_id: e.key().clone(),
                reason: e.reason,
                since: e.since.clone(),
                retry_after_secs: e
                    .until
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .saturating_add(1),
                probe_failures: e.failures,
                probing: e.probing,
            })
            .collect()
    }

    pub fn due_probes(&self) -> Vec<String> {
        let now = Instant::now();
        self.cooldowns
            .iter()
            .filter(|e| !e.probing && e.until <= now)
            .map(|e| e.key().clone())
            .collect()
    }

    /// Atomically claim one probe. Manual probes may run before the due time,
    /// but share the same per-key exclusion as the background worker.
    pub fn begin_probe(&self, key_id: &str, force: bool) -> Option<u64> {
        let mut e = self
            .cooldowns
            .entry(key_id.to_string())
            .or_insert_with(|| Cooldown {
                until: Instant::now(),
                reason: ErrorType::Error,
                since: chrono::Utc::now().to_rfc3339(),
                failures: 0,
                probing: false,
                generation: 0,
            });
        if e.probing || (!force && e.until > Instant::now()) {
            return None;
        }
        e.probing = true;
        e.generation += 1;
        Some(e.generation)
    }

    pub fn finish_probe(
        &self,
        key_id: &str,
        generation: u64,
        success: bool,
        backoff: u64,
        max_backoff: u64,
    ) {
        let mut e = match self.cooldowns.get_mut(key_id) {
            Some(e) if e.generation == generation && e.probing => e,
            _ => return,
        };
        if success {
            // Remove under the entry lock, so a concurrent new failure cannot be lost.
            drop(e);
            self.cooldowns
                .remove_if(key_id, |_, e| e.generation == generation && e.probing);
            self.transient.remove(key_id);
        } else {
            e.failures = e.failures.saturating_add(1);
            let delay = backoff
                .max(1)
                .saturating_mul(1u64 << e.failures.saturating_sub(1).min(20))
                .min(max_backoff.max(1));
            e.until = Instant::now() + Duration::from_secs(delay);
            e.probing = false;
        }
    }

    pub fn key_state(&self, api_key_id: &str) -> KeyState {
        if self.is_limited(api_key_id) {
            KeyState::Cooldown(self.cooldown_remaining_ms(api_key_id))
        } else {
            KeyState::Ok
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyState {
    Ok,
    Cooldown(u64), // remaining ms
    Disabled,
}

// ---------------------------------------------------------------------------
// UsageTracker - proactive sliding-window rate budgets
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct KeyUsage {
    rpm_events: Mutex<VecDeque<Instant>>,
    rpd_events: Mutex<VecDeque<Instant>>,
    rps_events: Mutex<VecDeque<Instant>>,
    five_min_events: Mutex<VecDeque<Instant>>,
    in_flight: AtomicI64,
}

impl Default for KeyUsage {
    fn default() -> Self {
        Self {
            rpm_events: Mutex::new(VecDeque::new()),
            rpd_events: Mutex::new(VecDeque::new()),
            rps_events: Mutex::new(VecDeque::new()),
            five_min_events: Mutex::new(VecDeque::new()),
            in_flight: AtomicI64::new(0),
        }
    }
}

impl KeyUsage {
    fn count_within(events: &mut VecDeque<Instant>, window: Duration) -> usize {
        let cutoff = Instant::now() - window;
        while events.front().is_some_and(|t| *t < cutoff) {
            events.pop_front();
        }
        events.len()
    }

    fn rpm_count(&self) -> usize {
        let mut g = self.rpm_events.lock().unwrap();
        Self::count_within(&mut g, Duration::from_secs(60))
    }

    fn rpd_count(&self) -> usize {
        let mut g = self.rpd_events.lock().unwrap();
        Self::count_within(&mut g, Duration::from_secs(86400))
    }

    fn rps_count(&self) -> usize {
        let mut g = self.rps_events.lock().unwrap();
        Self::count_within(&mut g, Duration::from_secs(1))
    }

    fn five_min_count(&self) -> usize {
        let mut g = self.five_min_events.lock().unwrap();
        Self::count_within(&mut g, Duration::from_secs(300))
    }

    fn record(&self) {
        let now = Instant::now();
        self.rpm_events.lock().unwrap().push_back(now);
        self.rpd_events.lock().unwrap().push_back(now);
        self.rps_events.lock().unwrap().push_back(now);
        self.five_min_events.lock().unwrap().push_back(now);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn complete(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    windows: Arc<DashMap<String, Arc<KeyUsage>>>,
}

impl UsageTracker {
    fn get_or_create(&self, key_id: &str) -> Arc<KeyUsage> {
        if let Some(entry) = self.windows.get(key_id) {
            return Arc::clone(&entry);
        }
        let usage = Arc::new(KeyUsage::default());
        self.windows.insert(key_id.to_string(), Arc::clone(&usage));
        usage
    }

    /// Record a reservation (call before making the provider request).
    pub fn reserve(&self, key_id: &str) {
        self.get_or_create(key_id).record();
    }

    /// Signal completion (call after provider returns, success or failure).
    pub fn complete(&self, key_id: &str) {
        if let Some(entry) = self.windows.get(key_id) {
            entry.complete();
        }
    }

    /// RPM headroom: 1.0 = unlimited / full budget, 0.0 = at limit.
    pub fn rpm_headroom(&self, key_id: &str, limit: Option<i64>) -> f64 {
        let Some(limit) = limit else { return 1.0 };
        if limit <= 0 {
            return 1.0;
        }
        let used = self.get_or_create(key_id).rpm_count() as f64;
        ((limit as f64 - used) / limit as f64).clamp(0.0, 1.0)
    }

    /// RPD headroom: 1.0 = unlimited / full budget.
    pub fn rpd_headroom(&self, key_id: &str, limit: Option<i64>) -> f64 {
        let Some(limit) = limit else { return 1.0 };
        if limit <= 0 {
            return 1.0;
        }
        let used = self.get_or_create(key_id).rpd_count() as f64;
        ((limit as f64 - used) / limit as f64).clamp(0.0, 1.0)
    }

    /// RPS headroom.
    pub fn rps_headroom(&self, key_id: &str, limit: Option<f64>) -> f64 {
        let Some(limit) = limit else { return 1.0 };
        if limit <= 0.0 {
            return 1.0;
        }
        let used = self.get_or_create(key_id).rps_count() as f64;
        ((limit - used) / limit).clamp(0.0, 1.0)
    }

    /// True when a limit configured on the key (rps / rpm / rpd) is fully used. The
    /// selector skips such a key instead of sending a request the provider will answer
    /// with a 429 (which then costs a 60 s cooldown) - and `rpd_limit` becomes a hard
    /// daily cap, e.g. on an expensive last-resort provider. Counted in memory: a restart
    /// starts the day's count over.
    pub fn budget_exhausted(
        &self,
        key_id: &str,
        rps_limit: Option<f64>,
        rpm_limit: Option<i64>,
        rpd_limit: Option<i64>,
    ) -> bool {
        self.rps_headroom(key_id, rps_limit) <= 0.0
            || self.rpm_headroom(key_id, rpm_limit) <= 0.0
            || self.rpd_headroom(key_id, rpd_limit) <= 0.0
    }

    /// Five-minute request count for traffic-balance scoring.
    pub fn five_min_count(&self, key_id: &str) -> usize {
        self.get_or_create(key_id).five_min_count()
    }

    /// Anchor RPM budget to provider's reported remaining count.
    pub fn sync_rpm(&self, key_id: &str, remaining: i64, limit: i64) {
        if limit <= 0 {
            return;
        }
        let usage = self.get_or_create(key_id);
        let mut events = usage.rpm_events.lock().unwrap();
        // Trim events so the count matches `limit - remaining`.
        let target_used = (limit - remaining).max(0) as usize;
        while events.len() > target_used {
            events.pop_front();
        }
    }
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    #[test]
    fn empty_answers_and_invalid_requests_never_cool_keys() {
        let state = RateLimitState::with_transient_threshold(1);
        for _ in 0..10 {
            assert!(!state.report_error("key", ErrorType::Empty));
            assert!(!state.report_error("key", ErrorType::Request));
        }
        assert!(!state.is_limited("key"));
        let state = RateLimitState::default();
        state.report_error("key", ErrorType::Timeout);
        state.report_error("key", ErrorType::Timeout);
        state.report_error("key", ErrorType::Empty);
        assert!(!state.report_error("key", ErrorType::Timeout));
    }

    #[test]
    fn repeated_transient_failures_still_cool_down() {
        let state = RateLimitState::default();
        assert!(!state.report_error("key", ErrorType::Timeout));
        assert!(!state.report_error("key", ErrorType::Error));
        assert!(state.report_error("key", ErrorType::Timeout));
        assert!(state.is_limited("key"));
    }

    #[test]
    fn parked_keys_require_a_successful_probe_and_failures_back_off() {
        for reason in [ErrorType::Billing, ErrorType::Auth, ErrorType::Rpm] {
            let state = RateLimitState::default().with_billing_cooldown(42);
            assert!(state.report_error("key", reason));
            if reason == ErrorType::Billing {
                assert!((41000..=42000).contains(&state.cooldown_remaining_ms("key")));
            }
            let since = state.parked_keys()[0].since.clone();
            state.cooldowns.get_mut("key").unwrap().until = Instant::now();
            assert!(state.is_limited("key"));
            assert_eq!(state.due_probes(), vec!["key"]);
            let token = state.begin_probe("key", false).unwrap();
            assert_eq!(state.begin_probe("key", true), None);
            assert!(state.due_probes().is_empty());
            state.report_success("key");
            assert!(state.is_limited("key"));
            state.finish_probe("key", token, false, 10, 60);
            assert!(state.is_limited("key"));
            assert_eq!(state.parked_keys()[0].probe_failures, 1);
            assert_eq!(state.parked_keys()[0].since, since);
            assert!(state.cooldown_remaining_ms("key") > 9000);
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, false, 10, 60);
            assert!(state.cooldown_remaining_ms("key") > 19000);
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, true, 10, 60);
            assert!(!state.is_limited("key"));
        }
    }

    #[test]
    fn stale_probe_cannot_release_a_new_failure() {
        let state = RateLimitState::default();
        state.report_error("key", ErrorType::Auth);
        let token = state.begin_probe("key", true).unwrap();
        state.report_error("key", ErrorType::Billing);
        state.finish_probe("key", token, true, 10, 60);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Billing));
    }
}
