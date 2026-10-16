//! Process-local, atomic per-key reservation clocks. Multiple replicas require shared state.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

use proviz_core::models::ApiKey;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Aux,
    #[default]
    #[serde(other)]
    Main,
}

#[derive(Clone)]
pub struct Pacer {
    keys: Arc<Mutex<HashMap<String, Arc<Mutex<KeyQueue>>>>>,
    safety: f64,
    aux_fraction: f64,
}

struct KeyQueue {
    next: Instant,
    metrics: QueueMetrics,
}

#[derive(Clone, Default, Serialize)]
pub struct QueueMetrics {
    pub queue_depth: u64,
    pub next_slot_delay_ms: u64,
    pub queued: u64,
    pub wait_ms: u64,
    pub overflow: u64,
}

/// Dropping a cancelled waiter clears depth, but does not rewind the clock: its
/// unused slot expires naturally, so a cancellation never creates a permanent gap.
pub struct Slot {
    key: Arc<Mutex<KeyQueue>>,
    pub at: Instant,
    queued: bool,
}

impl Slot {
    pub async fn wait(self) {
        tokio::time::sleep_until(self.at).await;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.queued {
            self.key.lock().unwrap().metrics.queue_depth -= 1;
        }
    }
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new(0.95, 0.3)
    }
}

impl Pacer {
    pub fn new(safety: f64, aux_fraction: f64) -> Self {
        assert!(safety.is_finite() && safety > 0.0 && safety <= 1.0);
        assert!(aux_fraction.is_finite() && (0.0..1.0).contains(&aux_fraction));
        Self {
            keys: Default::default(),
            safety,
            aux_fraction,
        }
    }

    pub fn metrics(&self, key_id: &str) -> QueueMetrics {
        let keys = self.keys.lock().unwrap();
        let Some(key) = keys.get(key_id) else {
            return QueueMetrics::default();
        };
        let key = key.lock().unwrap();
        QueueMetrics {
            next_slot_delay_ms: key
                .next
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
            ..key.metrics.clone()
        }
    }

    /// Aux can reserve only within its shorter horizon. Even a continuous aux
    /// flood cannot fill the main horizon; admitted slots keep their ordering.
    /// None means pacing is disabled; Err means budget_exhausted without a reservation.
    pub fn reserve(
        &self,
        key: &ApiKey,
        priority: Priority,
        deadline: Instant,
    ) -> Result<Option<Slot>, ()> {
        let Some(rate) = key.pacing_rate() else {
            return Ok(None);
        };
        let now = Instant::now();
        let state = self
            .keys
            .lock()
            .unwrap()
            .entry(key.id.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(KeyQueue {
                    next: now,
                    metrics: Default::default(),
                }))
            })
            .clone();
        let mut queue = state.lock().unwrap();
        let at = queue.next.max(now);
        let wait = at - now;
        let fraction = match priority {
            Priority::Main => 1.0,
            Priority::Aux => self.aux_fraction,
        };
        let cap = Duration::from_secs_f64(key.max_queue_ms.unwrap_or(0) as f64 / 1000.0 * fraction);
        // Leave 50 ms for the provider / response; never reserve outside the request budget.
        if wait > cap
            || at
                .checked_add(Duration::from_millis(50))
                .is_none_or(|end| end >= deadline)
        {
            queue.metrics.overflow += 1;
            tracing::info!(
                key_id = key.id,
                ?priority,
                wait_ms = wait.as_millis() as u64,
                overflow = queue.metrics.overflow,
                "pacing budget_exhausted"
            );
            return Err(());
        }
        // Floor sub-nanosecond precision so repeated rounding cannot push an
        // exact cap boundary a few nanoseconds outside its configured window.
        let interval = Duration::try_from_secs_f64(1.0 / (rate * self.safety))
            .ok()
            .map(|d| d.saturating_sub(Duration::from_nanos(1)))
            .filter(|d| !d.is_zero())
            .ok_or(())?;
        queue.next = at.checked_add(interval).ok_or(())?;
        let queued = !wait.is_zero();
        if queued {
            queue.metrics.queue_depth += 1;
            queue.metrics.queued += 1;
            queue.metrics.wait_ms += wait.as_millis() as u64;
            tracing::info!(
                key_id = key.id,
                ?priority,
                wait_ms = wait.as_millis() as u64,
                queued = queue.metrics.queued,
                queue_depth = queue.metrics.queue_depth,
                "request queued"
            );
        }
        drop(queue);
        Ok(Some(Slot {
            key: state,
            at,
            queued,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ApiKey {
        serde_json::from_value(serde_json::json!({
            "id":"k", "provider_id":"p", "label":"test", "key_ref":"TEST_KEY",
            "is_active":true, "rps_limit":20.0, "max_queue_ms":3000,
            "created_at":""
        }))
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn safety_rate_and_strict_cap_bound_a_burst() {
        let pacer = Pacer::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        let key = key();
        let slots: Vec<_> = (0..120)
            .filter_map(|_| pacer.reserve(&key, Priority::Main, deadline).ok().flatten())
            .collect();
        // 19/s gives floor(3*19)+1 immediate-or-queued slots, not 60.
        assert_eq!(slots.len(), 58);
        let metrics = pacer.metrics("k");
        assert_eq!(metrics.queue_depth, 57);
        assert_eq!(metrics.queued, 57);
        assert_eq!(metrics.overflow, 62);
        assert!(metrics.wait_ms > 80_000);
        assert_eq!(metrics.next_slot_delay_ms, 3052);
        drop(slots);
        assert_eq!(pacer.metrics("k").queue_depth, 0);
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(pacer.metrics("k").next_slot_delay_ms, 0);
        assert_eq!(
            pacer
                .reserve(&key, Priority::Main, deadline)
                .unwrap()
                .unwrap()
                .at,
            Instant::now()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sixty_concurrent_requests_are_spaced_at_the_effective_rate() {
        let pacer = Pacer::default();
        let mut key = key();
        key.max_queue_ms = Some(3200); // 59/19 seconds needs 3106 ms.
        let start = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..60 {
            let pacer = pacer.clone();
            let key = key.clone();
            tasks.spawn(async move {
                pacer
                    .reserve(&key, Priority::Main, start + Duration::from_secs(10))
                    .unwrap()
                    .unwrap()
                    .wait()
                    .await;
                Instant::now() - start
            });
        }
        let mut times = vec![];
        while let Some(result) = tasks.join_next().await {
            times.push(result.unwrap());
        }
        times.sort();
        assert_eq!(times.len(), 60);
        assert!(times
            .windows(2)
            .all(|w| w[1] - w[0] >= Duration::from_millis(51)));
        assert!(times[59] >= Duration::from_millis(3105));
        assert!(times[59] < Duration::from_millis(3110));
        assert_eq!(pacer.metrics("k").overflow, 0);
        assert_eq!(pacer.metrics("k").queue_depth, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn aux_flood_leaves_capacity_for_main() {
        let pacer = Pacer::default();
        let key = key();
        let deadline = Instant::now() + Duration::from_secs(10);
        let aux: Vec<_> = (0..120)
            .filter_map(|_| pacer.reserve(&key, Priority::Aux, deadline).ok().flatten())
            .collect();
        assert_eq!(aux.len(), 18); // through 894 ms; next aux slot exceeds 900 ms.
        let main: Vec<_> = (0..120)
            .filter_map(|_| pacer.reserve(&key, Priority::Main, deadline).ok().flatten())
            .collect();
        assert_eq!(main.len(), 40);
        assert!(pacer.reserve(&key, Priority::Aux, deadline).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_rejection_does_not_reserve_or_sleep() {
        let pacer = Pacer::default();
        let key = key();
        let start = Instant::now();
        let first = pacer
            .reserve(&key, Priority::Main, start + Duration::from_secs(1))
            .unwrap();
        assert!(pacer
            .reserve(&key, Priority::Main, start + Duration::from_millis(100))
            .is_err());
        assert_eq!(Instant::now(), start);
        let next = pacer
            .reserve(&key, Priority::Main, start + Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(next.at - start < Duration::from_millis(53));
        drop((first, next));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_wait_clears_depth_and_unused_slot_expires() {
        let pacer = Pacer::default();
        let key = key();
        let deadline = Instant::now() + Duration::from_secs(10);
        drop(pacer.reserve(&key, Priority::Main, deadline).unwrap());
        let slot = pacer
            .reserve(&key, Priority::Main, deadline)
            .unwrap()
            .unwrap();
        let task = tokio::spawn(slot.wait());
        tokio::task::yield_now().await;
        assert_eq!(pacer.metrics("k").queue_depth, 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(pacer.metrics("k").queue_depth, 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(pacer.metrics("k").next_slot_delay_ms, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn rpm_only_and_combined_limits_use_the_slowest_rate() {
        let mut key = key();
        key.rpm_limit = Some(60);
        let pacer = Pacer::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        drop(pacer.reserve(&key, Priority::Main, deadline).unwrap());
        let combined = pacer
            .reserve(&key, Priority::Main, deadline)
            .unwrap()
            .unwrap();
        assert!(combined.at - Instant::now() >= Duration::from_millis(1052));
        key.rps_limit = None;
        assert_eq!(key.pacing_rate(), Some(1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn null_zero_or_no_rate_preserves_legacy_selection() {
        let mut key = key();
        let pacer = Pacer::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        for cap in [None, Some(0), Some(-1)] {
            key.max_queue_ms = cap;
            assert!(pacer
                .reserve(&key, Priority::Main, deadline)
                .unwrap()
                .is_none());
        }
        key.max_queue_ms = Some(3000);
        key.rps_limit = None;
        assert!(pacer
            .reserve(&key, Priority::Main, deadline)
            .unwrap()
            .is_none());
        assert_eq!(pacer.metrics("k").queued, 0);
    }
}
