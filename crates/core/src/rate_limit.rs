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
            Self::Timeout => 30,
            Self::Empty => 10,
            Self::Error => 60,
        }
    }

    /// A failure that says nothing reliable about the key itself: one slow answer, one
    /// 5xx, one query with no results. Only a run of them should take the key out of
    /// rotation (see `RateLimitState::report_error`). 429 / 401 / 403 are explicit
    /// provider signals and still cool the key down at once.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Timeout | Self::Empty | Self::Error)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rps => "rps",
            Self::Rpm => "rpm",
            Self::Rpd => "rpd",
            Self::Auth => "auth",
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
pub struct RateLimitState {
    cooldowns: Arc<DashMap<String, (Instant, ErrorType)>>,
    /// Per key: (consecutive transient failures, time of the last one).
    transient: Arc<DashMap<String, (u32, Instant)>>,
    transient_threshold: u32,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::with_transient_threshold(DEFAULT_TRANSIENT_FAILURES_BEFORE_COOLDOWN)
    }
}

impl RateLimitState {
    /// `threshold` consecutive transient failures cool a key down; 0 or 1 = the old
    /// behaviour (the first one does).
    pub fn with_transient_threshold(threshold: u32) -> Self {
        Self {
            cooldowns: Arc::new(DashMap::new()),
            transient: Arc::new(DashMap::new()),
            transient_threshold: threshold.max(1),
        }
    }

    pub fn is_limited(&self, api_key_id: &str) -> bool {
        if let Some(entry) = self.cooldowns.get(api_key_id) {
            return Instant::now() < entry.0;
        }
        false
    }

    /// Record a failure. Returns true when the key was put on cooldown.
    pub fn report_error(&self, api_key_id: &str, error_type: ErrorType) -> bool {
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
        let unblocked_at = Instant::now() + Duration::from_secs(error_type.cooldown_secs());
        self.cooldowns
            .insert(api_key_id.to_string(), (unblocked_at, error_type));
        true
    }

    /// A successful call ends any run of transient failures.
    pub fn report_success(&self, api_key_id: &str) {
        self.transient.remove(api_key_id);
    }

    pub fn cooldown_remaining_ms(&self, api_key_id: &str) -> u64 {
        if let Some(entry) = self.cooldowns.get(api_key_id) {
            let now = Instant::now();
            if now < entry.0 {
                return (entry.0 - now).as_millis() as u64;
            }
        }
        0
    }

    pub fn key_state(&self, api_key_id: &str) -> KeyState {
        if let Some(entry) = self.cooldowns.get(api_key_id) {
            if Instant::now() < entry.0 {
                return KeyState::Cooldown(self.cooldown_remaining_ms(api_key_id));
            }
        }
        KeyState::Ok
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
