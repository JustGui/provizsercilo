use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{app::AppState, error::AppError};

#[derive(Deserialize)]
pub struct ReportRequest {
    pub api_key_id: String,
    pub outcome: String,
    pub error_type: Option<String>,
    pub remaining_requests: Option<i64>,
    pub limit_requests: Option<i64>,
    #[serde(default)]
    pub sync_limits: bool,
}

#[derive(Serialize)]
pub struct ReportResponse {
    pub ok: bool,
}

pub async fn handle_report(
    State(state): State<AppState>,
    Json(req): Json<ReportRequest>,
) -> Result<Json<ReportResponse>, AppError> {
    // Sync rate limit windows with provider-reported remaining counts
    if req.sync_limits {
        if let (Some(remaining), Some(limit)) = (req.remaining_requests, req.limit_requests) {
            state.usage.sync_rpm(&req.api_key_id, remaining, limit);
        }
    }

    // Apply error cooldown if outcome indicates failure
    if req.outcome == "rate_limit" || req.outcome == "error" {
        if let Some(et) = &req.error_type {
            use proviz_core::rate_limit::ErrorType;
            let error_type = match et.as_str() {
                "rpm" => Some(ErrorType::Rpm),
                "rpd" => Some(ErrorType::Rpd),
                "rps" => Some(ErrorType::Rps),
                "auth" => Some(ErrorType::Auth),
                "timeout" => Some(ErrorType::Timeout),
                "empty" => Some(ErrorType::Empty),
                _ => Some(ErrorType::Error),
            };
            if let Some(et) = error_type {
                state.rate_limit.report_error(&req.api_key_id, et);
                let _ = state
                    .storage
                    .record_rate_event(&req.api_key_id, et.as_str())
                    .await;
            }
        }
    }

    Ok(Json(ReportResponse { ok: true }))
}
