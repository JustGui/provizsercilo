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

pub async fn handle_stats(
    State(state): State<AppState>,
) -> Result<Json<StatsResponse>, AppError> {
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
