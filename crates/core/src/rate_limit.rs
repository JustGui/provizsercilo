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
    probe_generation: u64,
    requires_probe: bool,
    auth_episode: bool,
    transient_cooldowns: u32,
    probe_timeouts: u32,
}

impl Cooldown {
    fn new(reason: ErrorType) -> Self {
        Self {
            until: Instant::now(),
            reason,
            since: chrono::Utc::now().to_rfc3339(),
            failures: 0,
            probing: false,
            generation: 0,
            probe_generation: 0,
            requires_probe: false,
            auth_episode: false,
            transient_cooldowns: 0,
            probe_timeouts: 0,
        }
    }

    fn is_limited(&self) -> bool {
        self.requires_probe || self.until > Instant::now()
    }
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

#[derive(Debug, Default, Clone)]
struct AuthHistory {
    last_user_success: Option<Instant>,
    last_auth_error: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct RateLimitState {
    cooldowns: Arc<DashMap<String, Cooldown>>,
    transient: Arc<DashMap<String, (u32, Instant)>>,
    auth_history: Arc<DashMap<String, AuthHistory>>,
    auth_recent_success_secs: u64,
    auth_repeat_window_secs: u64,
    auth_first_cooldown_secs: u64,
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
            auth_history: Arc::new(DashMap::new()),
            auth_recent_success_secs: 600,
            auth_repeat_window_secs: 600,
            auth_first_cooldown_secs: 30,
            transient_threshold: threshold.max(1),
            billing_cooldown_secs: ErrorType::Billing.cooldown_secs(),
        }
    }

    pub fn with_billing_cooldown(mut self, seconds: u64) -> Self {
        self.billing_cooldown_secs = seconds.max(1);
        self
    }

    pub fn with_auth_cooldown(
        mut self,
        recent_success_secs: u64,
        repeat_window_secs: u64,
        first_cooldown_secs: u64,
    ) -> Self {
        self.auth_recent_success_secs = recent_success_secs.max(1);
        self.auth_repeat_window_secs = repeat_window_secs.max(1);
        self.auth_first_cooldown_secs = first_cooldown_secs.max(1);
        self
    }

    // Called while holding the cooldown entry: record only the first auth error
    // of an episode, which ends when a successful probe removes the cooldown.
    fn auth_cooldown_secs(&self, api_key_id: &str) -> u64 {
        let mut history = self.auth_history.entry(api_key_id.to_string()).or_default();
        let now = Instant::now();
        let recently_healthy = history.last_user_success.is_some_and(|t| {
            now.duration_since(t) <= Duration::from_secs(self.auth_recent_success_secs)
        });
        let repeated = history.last_auth_error.is_some_and(|t| {
            now.duration_since(t) <= Duration::from_secs(self.auth_repeat_window_secs)
        });
        history.last_auth_error = Some(now);
        if recently_healthy && !repeated {
            self.auth_first_cooldown_secs
        } else {
            ErrorType::Auth.cooldown_secs()
        }
    }

    /// Provider throttles and a first transient cooldown release on expiry.
    /// Dead or repeatedly failing keys require a successful probe instead.
    pub fn is_limited(&self, api_key_id: &str) -> bool {
        self.cooldowns
            .get(api_key_id)
            .is_some_and(|e| e.is_limited())
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
        let mut entry = self
            .cooldowns
            .entry(api_key_id.to_string())
            .or_insert_with(|| Cooldown::new(error_type));
        if error_type == ErrorType::Auth && entry.auth_episode {
            // In-flight failures belong to the current episode, even after its
            // deadline passes or billing takes precedence over the auth reason.
            // Still invalidate a probe racing this new failure.
            entry.generation += 1;
            return true;
        }
        let seconds = if error_type == ErrorType::Billing {
            self.billing_cooldown_secs
        } else if error_type == ErrorType::Auth {
            entry.auth_episode = true;
            self.auth_cooldown_secs(api_key_id)
        } else {
            error_type.cooldown_secs()
        };
        if !entry.is_limited() {
            // Count cooldown episodes, not concurrent failures in the same one.
            // Keep the history after expiry until a user call or probe succeeds.
            if error_type.is_transient() {
                entry.transient_cooldowns = entry.transient_cooldowns.saturating_add(1);
            }
            entry.since = chrono::Utc::now().to_rfc3339();
            entry.reason = error_type;
            entry.failures = 0;
            entry.probe_timeouts = 0;
        }
        entry.requires_probe |= matches!(error_type, ErrorType::Billing | ErrorType::Auth)
            || (error_type.is_transient() && entry.transient_cooldowns >= 2);
        // Concurrent in-flight calls may fail after the key was already parked.
        // A weaker/shorter failure must not undo billing or auth parking.
        entry.until = entry
            .until
            .max(Instant::now() + Duration::from_secs(seconds));
        if error_type == ErrorType::Billing
            || (entry.reason != ErrorType::Billing
                && (entry.reason != ErrorType::Auth || error_type == ErrorType::Auth)
                && (!entry.requires_probe
                    || !matches!(error_type, ErrorType::Rps | ErrorType::Rpm | ErrorType::Rpd)))
        {
            entry.reason = error_type;
        }
        entry.generation += 1; // invalidates a probe racing a new user-call failure
        true
    }

    /// A late successful user request must never release an already parked key.
    pub fn report_success(&self, api_key_id: &str) {
        self.auth_history
            .entry(api_key_id.to_string())
            .or_default()
            .last_user_success = Some(Instant::now());
        self.transient.remove(api_key_id);
        if let Some(mut e) = self.cooldowns.get_mut(api_key_id) {
            e.transient_cooldowns = 0;
        }
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
        self.cooldowns
            .get(api_key_id)
            .filter(|e| e.is_limited())
            .map(|e| e.reason)
    }

    pub fn parked_keys(&self) -> Vec<ParkedKey> {
        self.cooldowns
            .iter()
            .filter(|e| e.is_limited())
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
            .filter(|e| e.requires_probe && !e.probing && e.until <= now)
            .map(|e| e.key().clone())
            .collect()
    }

    /// Atomically claim one probe. Manual probes may run before the due time,
    /// but share the same per-key exclusion as the background worker.
    pub fn begin_probe(&self, key_id: &str, force: bool) -> Option<u64> {
        let mut e = if force {
            self.cooldowns
                .entry(key_id.to_string())
                .or_insert_with(|| Cooldown::new(ErrorType::Error))
        } else {
            self.cooldowns.get_mut(key_id)?
        };
        if e.probing || (!force && (!e.requires_probe || e.until > Instant::now())) {
            return None;
        }
        e.probing = true;
        e.generation += 1;
        e.probe_generation = e.generation;
        Some(e.generation)
    }

    pub fn finish_probe(
        &self,
        key_id: &str,
        generation: u64,
        result: Result<(), ErrorType>,
        backoff: u64,
        max_backoff: u64,
        max_transient_backoff: u64,
    ) {
        let mut e = match self.cooldowns.get_mut(key_id) {
            Some(e) if e.probe_generation == generation && e.probing => e,
            _ => return,
        };
        if e.generation != generation {
            e.probing = false;
            return; // A newer user-call failure keeps its own reason and deadline.
        }
        if result == Err(ErrorType::Auth) && !e.auth_episode {
            e.auth_episode = true;
            self.auth_history
                .entry(key_id.to_string())
                .or_default()
                .last_auth_error = Some(Instant::now());
        }
        if result.is_ok() {
            // Conditional removal preserves any concurrent new failure.
            drop(e);
            self.cooldowns
                .remove_if(key_id, |_, e| e.generation == generation && e.probing);
            self.transient.remove(key_id);
        } else {
            if result == Err(ErrorType::Timeout) {
                e.probe_timeouts = e.probe_timeouts.saturating_add(1);
                if e.probe_timeouts < 2 {
                    // One slow probe must not increase backoff. Retry on the next scan.
                    e.probing = false;
                    return;
                }
            } else {
                e.probe_timeouts = 0;
            }
            if let Err(reason @ (ErrorType::Billing | ErrorType::Auth)) = result {
                e.reason = reason;
                e.requires_probe = true;
            }
            e.failures = e.failures.saturating_add(1);
            let cap = if matches!(e.reason, ErrorType::Billing | ErrorType::Auth) {
                max_backoff
            } else {
                max_transient_backoff
            };
            let delay = backoff
                .max(1)
                .saturating_mul(1u64 << e.failures.saturating_sub(1).min(20))
                .min(cap.max(1));
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

    fn expire(state: &RateLimitState, key: &str) {
        state.cooldowns.get_mut(key).unwrap().until = Instant::now();
    }

    fn assert_delay(state: &RateLimitState, seconds: u64) {
        assert!(((seconds * 1000 - 1000)..=seconds * 1000)
            .contains(&state.cooldown_remaining_ms("key")));
    }

    #[test]
    fn first_auth_on_recently_healthy_key_requires_probe_after_short_cooldown() {
        let state = RateLimitState::default();
        state.report_success("key");
        assert!(state.report_error("key", ErrorType::Auth));
        assert_delay(&state, 30);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Auth));
        assert!(state.due_probes().is_empty());
        assert_eq!(state.begin_probe("key", false), None);
        // Even a late successful user call cannot release the short parking.
        state.report_success("key");
        expire(&state, "key");
        assert!(state.is_limited("key"));
        assert_eq!(state.due_probes(), vec!["key"]);
        let token = state.begin_probe("key", false).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        assert!(!state.is_limited("key"));
        // Probe and user successes preserve the repeat-auth history.
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 300);
    }

    #[test]
    fn concurrent_auth_errors_in_one_episode_keep_short_cooldown() {
        let state = RateLimitState::default();
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        let deadline = state.cooldowns.get("key").unwrap().until;
        let first_error = state.auth_history.get("key").unwrap().last_auth_error;
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| state.report_error("key", ErrorType::Auth));
            }
        });
        assert_delay(&state, 30);
        assert_eq!(state.cooldowns.get("key").unwrap().until, deadline);
        // Expiry and a late success do not end a probe-gated episode.
        expire(&state, "key");
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        assert_eq!(state.cooldown_remaining_ms("key"), 0);
        assert!(state.is_limited("key"));
        assert_eq!(
            state.auth_history.get("key").unwrap().last_auth_error,
            first_error
        );
    }

    #[test]
    fn auth_after_release_within_repeat_window_uses_full_cooldown() {
        let state = RateLimitState::default();
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        assert!(!state.is_limited("key"));
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 300);
    }

    #[test]
    fn auth_after_release_outside_repeat_window_uses_short_cooldown() {
        let state = RateLimitState::default();
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        state.auth_history.get_mut("key").unwrap().last_auth_error =
            Some(Instant::now() - Duration::from_secs(601));
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 30);
    }

    #[test]
    fn auth_without_recent_user_success_uses_full_cooldown() {
        for old_success in [false, true] {
            let state = RateLimitState::default();
            if old_success {
                state
                    .auth_history
                    .entry("key".into())
                    .or_default()
                    .last_user_success = Some(Instant::now() - Duration::from_secs(601));
            } else {
                let token = state.begin_probe("key", true).unwrap();
                state.finish_probe("key", token, Ok(()), 60, 21600, 600);
            }
            state.report_error("key", ErrorType::Auth);
            assert_delay(&state, 300);
            expire(&state, "key");
            assert!(state.is_limited("key"));
            assert_eq!(state.due_probes(), vec!["key"]);
        }
    }

    #[test]
    fn auth_windows_and_first_cooldown_are_configurable_and_per_key() {
        let state = RateLimitState::default().with_auth_cooldown(10, 20, 7);
        state.report_success("key");
        state.report_error("other", ErrorType::Auth);
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 7);
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        state.auth_history.get_mut("key").unwrap().last_auth_error =
            Some(Instant::now() - Duration::from_secs(21));
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 7);
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        {
            let mut history = state.auth_history.get_mut("key").unwrap();
            history.last_auth_error = None;
            history.last_user_success = Some(Instant::now() - Duration::from_secs(11));
        }
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 300);
    }

    #[test]
    fn auth_probe_failure_preserves_repeat_history() {
        let state = RateLimitState::default();
        state.report_success("key");
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Auth), 60, 21600, 600);
        let first_error = state.auth_history.get("key").unwrap().last_auth_error;
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Auth), 60, 21600, 600);
        assert_eq!(
            state.auth_history.get("key").unwrap().last_auth_error,
            first_error
        );
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        state.report_error("key", ErrorType::Auth);
        assert_delay(&state, 300);
    }

    #[test]
    fn billing_reason_does_not_start_a_second_auth_episode() {
        let state = RateLimitState::default().with_billing_cooldown(42);
        state.report_success("key");
        state.report_error("key", ErrorType::Auth);
        let first_error = state.auth_history.get("key").unwrap().last_auth_error;
        state.report_error("key", ErrorType::Billing);
        let deadline = state.cooldowns.get("key").unwrap().until;
        state.report_error("key", ErrorType::Auth);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Billing));
        assert_eq!(state.cooldowns.get("key").unwrap().until, deadline);
        assert_eq!(
            state.auth_history.get("key").unwrap().last_auth_error,
            first_error
        );
    }

    #[test]
    fn recent_success_does_not_shorten_billing_parking() {
        let state = RateLimitState::default().with_billing_cooldown(42);
        state.report_success("key");
        state.report_error("key", ErrorType::Billing);
        assert_delay(&state, 42);
        state.report_error("key", ErrorType::Auth);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Billing));
        assert_delay(&state, 42);
        expire(&state, "key");
        assert!(state.is_limited("key"));
        assert_eq!(state.due_probes(), vec!["key"]);
    }

    #[test]
    fn rate_limits_and_first_transient_cooldowns_release_without_probes() {
        for reason in [
            ErrorType::Rps,
            ErrorType::Rpm,
            ErrorType::Rpd,
            ErrorType::Timeout,
            ErrorType::Error,
        ] {
            let state = RateLimitState::with_transient_threshold(1);
            assert!(state.report_error("key", reason));
            assert!(state.is_limited("key"));
            expire(&state, "key");
            assert!(!state.is_limited("key"), "{reason:?}");
            assert_eq!(state.key_state("key"), KeyState::Ok);
            assert_eq!(state.cooldown_reason("key"), None);
            assert!(state.parked_keys().is_empty());
            assert!(state.due_probes().is_empty());
            assert_eq!(state.begin_probe("key", false), None);
        }
    }

    #[test]
    fn only_repeated_transient_cooldown_episodes_require_probes() {
        let state = RateLimitState::default();
        for _ in 0..6 {
            state.report_error("key", ErrorType::Timeout);
        }
        // Concurrent errors during the first cooldown do not create a second episode.
        expire(&state, "key");
        assert!(!state.is_limited("key"));
        for _ in 0..3 {
            state.report_error("key", ErrorType::Error);
        }
        expire(&state, "key");
        assert!(state.is_limited("key"));
        assert_eq!(state.due_probes(), vec!["key"]);
        let token = state.begin_probe("key", false).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        assert!(!state.is_limited("key"));
        for _ in 0..3 {
            state.report_error("key", ErrorType::Timeout);
        }
        expire(&state, "key");
        assert!(!state.is_limited("key"));
    }

    #[test]
    fn success_resets_cooldown_history() {
        for empty_answer in [false, true] {
            let state = RateLimitState::with_transient_threshold(1);
            state.report_error("key", ErrorType::Timeout);
            expire(&state, "key");
            if empty_answer {
                state.report_error("key", ErrorType::Empty);
            } else {
                state.report_success("key");
            }
            state.report_error("key", ErrorType::Error);
            expire(&state, "key");
            assert!(!state.is_limited("key"));
        }
    }

    #[test]
    fn repeated_throttles_never_require_probes() {
        let state = RateLimitState::default();
        for reason in [
            ErrorType::Rps,
            ErrorType::Rpm,
            ErrorType::Rpd,
            ErrorType::Rps,
        ] {
            state.report_error("key", reason);
            expire(&state, "key");
            assert!(!state.is_limited("key"));
            assert!(state.due_probes().is_empty());
        }
    }

    #[test]
    fn transient_probe_backoff_caps_at_ten_minutes_until_auth_or_billing_is_detected() {
        for hard_failure in [ErrorType::Billing, ErrorType::Auth] {
            let state = RateLimitState::with_transient_threshold(1);
            state.report_error("key", ErrorType::Timeout);
            expire(&state, "key");
            state.report_error("key", ErrorType::Error);
            for delay in [60, 120, 240, 480, 600, 600, 600, 600, 600, 600] {
                expire(&state, "key");
                let token = state.begin_probe("key", false).unwrap();
                state.finish_probe("key", token, Err(ErrorType::Error), 60, 21600, 600);
                assert!(((delay * 1000 - 1000)..=delay * 1000)
                    .contains(&state.cooldown_remaining_ms("key")));
            }
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, Err(hard_failure), 60, 21600, 600);
            assert_eq!(state.cooldown_reason("key"), Some(hard_failure));
            assert!((21_599_000..=21_600_000).contains(&state.cooldown_remaining_ms("key")));
        }
    }

    #[test]
    fn probe_timeouts_only_back_off_after_two_consecutive_timeouts() {
        let state = RateLimitState::default();
        state.report_error("key", ErrorType::Billing);
        expire(&state, "key");
        let token = state.begin_probe("key", false).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Timeout), 60, 21600, 600);
        assert_eq!(state.parked_keys()[0].probe_failures, 0);
        assert_eq!(state.cooldown_remaining_ms("key"), 0);
        assert_eq!(state.due_probes(), vec!["key"]);
        let token = state.begin_probe("key", false).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Timeout), 60, 21600, 600);
        assert_eq!(state.parked_keys()[0].probe_failures, 1);
        assert!((59000..=60000).contains(&state.cooldown_remaining_ms("key")));
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Error), 60, 21600, 600);
        let deadline = state.cooldowns.get("key").unwrap().until;
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Err(ErrorType::Timeout), 60, 21600, 600);
        assert_eq!(state.parked_keys()[0].probe_failures, 2);
        assert_eq!(state.cooldowns.get("key").unwrap().until, deadline);
        assert!(state.is_limited("key"));
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 60, 21600, 600);
        assert!(!state.is_limited("key"));
    }

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
        for reason in [ErrorType::Billing, ErrorType::Auth] {
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
            state.finish_probe("key", token, Err(reason), 10, 60, 30);
            assert!(state.is_limited("key"));
            assert_eq!(state.parked_keys()[0].probe_failures, 1);
            assert_eq!(state.parked_keys()[0].since, since);
            assert!(state.cooldown_remaining_ms("key") > 9000);
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, Err(reason), 10, 60, 30);
            assert!(state.cooldown_remaining_ms("key") > 19000);
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, Ok(()), 10, 60, 30);
            assert!(!state.is_limited("key"));
        }
    }

    #[test]
    fn stale_probe_cannot_release_a_new_failure() {
        let state = RateLimitState::default();
        state.report_error("key", ErrorType::Auth);
        let token = state.begin_probe("key", true).unwrap();
        state.report_error("key", ErrorType::Billing);
        assert_eq!(state.begin_probe("key", true), None);
        state.finish_probe("key", token, Ok(()), 10, 60, 30);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Billing));
        assert!(!state.parked_keys()[0].probing);
        assert!(state.begin_probe("key", true).is_some());
    }
    #[test]
    fn later_in_flight_errors_cannot_shorten_billing_parking() {
        let state = RateLimitState::with_transient_threshold(1);
        state.report_error("key", ErrorType::Billing);
        state.report_error("key", ErrorType::Auth);
        state.report_error("key", ErrorType::Timeout);
        assert_eq!(state.cooldown_reason("key"), Some(ErrorType::Billing));
        assert!(state.cooldown_remaining_ms("key") > 21_599_000);
    }

    #[test]
    fn probe_backoff_is_capped_and_stale_due_work_does_not_repark_healthy_keys() {
        let state = RateLimitState::default();
        assert!(state.begin_probe("healthy", false).is_none());
        assert!(!state.is_limited("healthy"));
        state.report_error("key", ErrorType::Auth);
        for _ in 0..10 {
            let token = state.begin_probe("key", true).unwrap();
            state.finish_probe("key", token, Err(ErrorType::Auth), 10, 30, 20);
        }
        assert!((29000..=30000).contains(&state.cooldown_remaining_ms("key")));
        let token = state.begin_probe("key", true).unwrap();
        state.finish_probe("key", token, Ok(()), 10, 30, 20);
        assert!(state.begin_probe("key", false).is_none());
        assert!(!state.is_limited("key"));
    }
}
