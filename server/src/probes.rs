use std::sync::{Arc, Weak};
use std::time::Duration;

use proviz_core::rate_limit::{ErrorType, RateLimitState};
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::{catalog::CatalogStore, config::Config, error::AppError, executor::Executor};

pub struct ProbeRunner {
    executor: Arc<Executor>,
    catalog: CatalogStore,
    rate_limit: RateLimitState,
    config: Arc<Config>,
    permits: Arc<Semaphore>,
}

#[derive(Debug, Serialize)]
pub struct ProbeOutcome {
    pub key_id: String,
    pub success: bool,
    pub error_type: Option<&'static str>,
}

impl ProbeRunner {
    pub fn new(
        executor: Arc<Executor>,
        catalog: CatalogStore,
        rate_limit: RateLimitState,
        config: Arc<Config>,
    ) -> Self {
        Self {
            executor,
            catalog,
            rate_limit,
            permits: Arc::new(Semaphore::new(config.probe_concurrency.max(1))),
            config,
        }
    }

    pub async fn run(&self, id: &str, force: bool) -> Result<ProbeOutcome, AppError> {
        // Both manual and background probes respect operator-disabled providers/keys.
        let active = self
            .catalog
            .read()
            .await
            .candidates(None)
            .iter()
            .any(|c| c.api_key.id == id);
        if !active {
            return Err(AppError::not_found("Active key not found"));
        }
        let _permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| AppError::probe_busy())?;
        let generation = self
            .rate_limit
            .begin_probe(id, force)
            .ok_or_else(AppError::probe_busy)?;
        // If a request/task is cancelled, Drop releases the claim with backoff.
        struct Claim<'a> {
            runner: &'a ProbeRunner,
            id: &'a str,
            generation: u64,
            finished: bool,
        }
        impl Drop for Claim<'_> {
            fn drop(&mut self) {
                if !self.finished {
                    self.runner.rate_limit.finish_probe(
                        self.id,
                        self.generation,
                        Err(ErrorType::Timeout),
                        self.runner.config.probe_backoff_secs,
                        self.runner.config.probe_max_backoff_secs,
                    );
                }
            }
        }
        let mut claim = Claim {
            runner: self,
            id,
            generation,
            finished: false,
        };
        let error = self
            .executor
            .probe_key(id, &self.config.probe_query, self.config.probe_timeout_ms)
            .await
            .err()
            .map(|e| e.error_type());
        self.rate_limit.finish_probe(
            id,
            generation,
            error.map_or(Ok(()), Err),
            self.config.probe_backoff_secs,
            self.config.probe_max_backoff_secs,
        );
        claim.finished = true;
        // No provider response or request text is logged, including on decode errors.
        if let Some(reason) = error {
            tracing::warn!(
                key_id = id,
                error_type = reason.as_str(),
                "key probe failed; remains parked"
            );
        } else {
            tracing::info!(key_id = id, "key probe succeeded");
        }
        Ok(ProbeOutcome {
            key_id: id.into(),
            success: error.is_none(),
            error_type: error.map(|e| e.as_str()),
        })
    }

    pub async fn run_due(self: &Arc<Self>) {
        // JoinSet bounds the tasks themselves as well as concurrent upstream calls.
        let mut tasks = tokio::task::JoinSet::new();
        for id in self.rate_limit.due_probes() {
            while tasks.len() >= self.config.probe_concurrency.max(1) {
                let _ = tasks.join_next().await;
            }
            let runner = Arc::clone(self);
            tasks.spawn(async move {
                let _ = runner.run(&id, false).await;
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

pub fn spawn_worker(runner: Weak<ProbeRunner>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(runner) = runner.upgrade() else {
                break;
            };
            runner.run_due().await;
        }
    });
}
