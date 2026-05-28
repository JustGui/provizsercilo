use axum::{extract::State, Json};
use proviz_core::rate_limit::KeyState;
use serde::Serialize;

use crate::{app::AppState, error::AppError};

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub providers: Vec<ProviderHealth>,
}

#[derive(Serialize)]
pub struct ProviderHealth {
    pub slug: String,
    pub is_active: bool,
    pub keys: Vec<KeyHealth>,
}

#[derive(Serialize)]
pub struct KeyHealth {
    pub id: String,
    pub label: String,
    pub state: &'static str,
    pub cooldown_remaining_ms: u64,
    pub rpm_headroom: f64,
}

pub async fn handle_health(
    State(state): State<AppState>,
) -> Result<Json<HealthResponse>, AppError> {
    let catalog = state.catalog.read().await;

    let mut providers_health: Vec<ProviderHealth> = Vec::new();

    for provider in &catalog.providers {
        let keys = catalog
            .api_keys
            .iter()
            .filter(|k| k.provider_id == provider.id)
            .map(|k| {
                let key_state = state.rate_limit.key_state(&k.id);
                let (state_str, cooldown_ms) = match &key_state {
                    KeyState::Ok => ("ok", 0),
                    KeyState::Cooldown(ms) => ("cooldown", *ms),
                    KeyState::Disabled => ("disabled", 0),
                };
                KeyHealth {
                    id: k.id.clone(),
                    label: k.label.clone(),
                    state: state_str,
                    cooldown_remaining_ms: cooldown_ms,
                    rpm_headroom: state.usage.rpm_headroom(&k.id, k.rpm_limit),
                }
            })
            .collect();

        providers_health.push(ProviderHealth {
            slug: provider.slug.clone(),
            is_active: provider.is_active,
            keys,
        });
    }

    Ok(Json(HealthResponse {
        status: "ok",
        providers: providers_health,
    }))
}
