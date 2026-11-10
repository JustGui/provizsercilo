use axum::{extract::State, Json};
use serde::Serialize;
use std::collections::HashMap;

use crate::{app::AppState, error::AppError};

#[derive(Serialize)]
pub struct StatsResponse {
    pub window_5m: WindowStats,
    pub by_provider: HashMap<String, ProviderStatsOut>,
}

#[derive(Serialize)]
pub struct WindowStats {
    pub searches: i64,
    pub errors: i64,
}

#[derive(Serialize)]
pub struct ProviderStatsOut {
    pub searches: i64,
    pub errors: i64,
    pub avg_latency_ms: Option<i64>,
}

/// Prometheus scrape endpoint: provider call/failure counters and parked keys by provider.
pub async fn handle_metrics(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let parked = state.rate_limit.parked_keys();
    let named: Vec<(String, proviz_core::rate_limit::ParkedKey)> = {
        let catalog = state.catalog.read().await;
        parked
            .into_iter()
            .map(|key| {
                let provider = catalog
                    .api_keys
                    .iter()
                    .find(|k| k.id == key.key_id)
                    .and_then(|k| catalog.providers.iter().find(|p| p.id == k.provider_id))
                    .map(|p| p.slug.clone())
                    .unwrap_or_else(|| "unknown".into());
                (provider, key)
            })
            .collect()
    };
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::render(&named),
    )
}

pub async fn handle_stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, AppError> {
    let window = state.stats.window_stats(300);
    let by_prov = state.stats.by_provider_stats(3600); // 1h window for per-provider

    let by_provider: HashMap<String, ProviderStatsOut> = by_prov
        .into_iter()
        .map(|p| {
            (
                p.slug,
                ProviderStatsOut {
                    searches: p.searches,
                    errors: p.errors,
                    avg_latency_ms: p.avg_latency_ms,
                },
            )
        })
        .collect();

    Ok(Json(StatsResponse {
        window_5m: WindowStats {
            searches: window.searches,
            errors: window.errors,
        },
        by_provider,
    }))
}
