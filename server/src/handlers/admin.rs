use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
use proviz_core::models::{ApiKey, Group, Provider};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{app::AppState, error::AppError};

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

pub async fn require_admin_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let Some(token) = state.config.admin_token.as_deref() else {
        return Err(AppError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Admin API disabled: ADMIN_TOKEN not configured".to_string(),
            code: "admin_disabled".to_string(),
        });
    };

    let provided = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if provided != Some(token) {
        return Err(AppError::unauthorized());
    }

    Ok(next.run(request).await)
}

// ---------------------------------------------------------------------------
// /admin/reload
// ---------------------------------------------------------------------------

pub async fn handle_reload(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// /admin/providers
// ---------------------------------------------------------------------------

pub async fn handle_list_providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<Provider>>, AppError> {
    let catalog = state.catalog.read().await;
    Ok(Json(catalog.providers.clone()))
}

#[derive(Deserialize)]
pub struct CreateProviderRequest {
    pub slug: String,
    pub name: String,
    pub base_url: Option<String>,
    pub priority: Option<i64>,
    pub coverage_scores: Option<HashMap<String, f64>>,
    pub notes: Option<String>,
}

pub async fn handle_create_provider(
    State(state): State<AppState>,
    Json(req): Json<CreateProviderRequest>,
) -> Result<Json<Provider>, AppError> {
    let provider = Provider {
        id: Uuid::new_v4().to_string(),
        slug: req.slug,
        name: req.name,
        base_url: req.base_url,
        is_active: true,
        priority: req.priority.unwrap_or(0),
        avg_latency_ms: None,
        coverage_scores: req.coverage_scores.unwrap_or_default(),
        notes: req.notes,
        created_at: String::new(),
    };
    let created = state.storage.create_provider(provider).await?;
    state.catalog.reload().await?;
    Ok(Json(created))
}

#[derive(Deserialize)]
pub struct UpdateProviderRequest {
    pub priority: Option<i64>,
    pub is_active: Option<bool>,
    pub coverage_scores: Option<HashMap<String, f64>>,
    pub notes: Option<Option<String>>,
}

pub async fn handle_update_provider(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<UpdateProviderRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .storage
        .update_provider_fields(
            &slug,
            req.priority,
            req.is_active,
            req.coverage_scores,
            req.notes,
        )
        .await?;
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// /admin/providers/:slug/keys
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ApiKeyPublic {
    pub id: String,
    pub provider_id: String,
    pub label: String,
    pub key_ref: String,
    pub is_active: bool,
    pub rps_limit: Option<f64>,
    pub rpm_limit: Option<i64>,
    pub rpd_limit: Option<i64>,
    pub last_used_at: Option<String>,
    pub created_at: String,
}

impl From<ApiKey> for ApiKeyPublic {
    fn from(k: ApiKey) -> Self {
        Self {
            id: k.id,
            provider_id: k.provider_id,
            label: k.label,
            key_ref: k.key_ref,
            is_active: k.is_active,
            rps_limit: k.rps_limit,
            rpm_limit: k.rpm_limit,
            rpd_limit: k.rpd_limit,
            last_used_at: k.last_used_at,
            created_at: k.created_at,
        }
    }
}

pub async fn handle_list_keys(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<ApiKeyPublic>>, AppError> {
    let provider = state.storage.get_provider_by_slug(&slug).await?;
    let keys = state.storage.list_keys_for_provider(&provider.id).await?;
    Ok(Json(keys.into_iter().map(ApiKeyPublic::from).collect()))
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub label: String,
    pub key_ref: String,
    pub rps_limit: Option<f64>,
    pub rpm_limit: Option<i64>,
    pub rpd_limit: Option<i64>,
}

pub async fn handle_create_key(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<CreateKeyRequest>,
) -> Result<Json<ApiKeyPublic>, AppError> {
    let provider = state.storage.get_provider_by_slug(&slug).await?;
    let key = ApiKey {
        id: Uuid::new_v4().to_string(),
        provider_id: provider.id,
        label: req.label,
        key_ref: req.key_ref,
        is_active: true,
        rps_limit: req.rps_limit,
        rpm_limit: req.rpm_limit,
        rpd_limit: req.rpd_limit,
        last_used_at: None,
        created_at: String::new(),
    };
    let created = state.storage.create_api_key(key).await?;
    state.catalog.reload().await?;
    Ok(Json(ApiKeyPublic::from(created)))
}

// ---------------------------------------------------------------------------
// /admin/keys/:id
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UpdateKeyRequest {
    pub label: Option<String>,
    pub is_active: Option<bool>,
    pub key_ref: Option<String>,
    pub rpm_limit: Option<Option<i64>>,
    pub rpd_limit: Option<Option<i64>>,
}

pub async fn handle_update_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateKeyRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .storage
        .update_api_key_fields(
            &id,
            req.label,
            req.is_active,
            req.key_ref,
            req.rpm_limit,
            req.rpd_limit,
        )
        .await?;
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn handle_delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.storage.soft_delete_api_key(&id).await?;
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Serialize)]
pub struct ResolveResponse {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<Vec<String>>,
}

pub async fn handle_resolve_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ResolveResponse>, AppError> {
    let key = state.storage.get_api_key(&id).await?;
    let (ok, checked) = proviz_core::key_resolver::check_key(&key.key_ref, &state.config.secrets_dir);
    Ok(Json(ResolveResponse {
        status: if ok { "ok" } else { "missing" },
        checked,
    }))
}

// ---------------------------------------------------------------------------
// /admin/groups
// ---------------------------------------------------------------------------

pub async fn handle_list_groups(
    State(state): State<AppState>,
) -> Result<Json<Vec<Group>>, AppError> {
    let catalog = state.catalog.read().await;
    Ok(Json(catalog.groups.clone()))
}

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
}

pub async fn handle_create_group(
    State(state): State<AppState>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<Json<Group>, AppError> {
    let group = Group {
        id: Uuid::new_v4().to_string(),
        slug: req.slug,
        name: req.name,
        description: req.description,
        is_active: true,
        created_at: String::new(),
    };
    let created = state.storage.create_group(group).await?;
    state.catalog.reload().await?;
    Ok(Json(created))
}

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub api_key_id: String,
    pub priority: Option<i64>,
}

pub async fn handle_add_group_member(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let catalog = state.catalog.read().await;
    let group = catalog
        .groups
        .iter()
        .find(|g| g.slug == slug)
        .ok_or_else(|| AppError::not_found(format!("Group '{slug}' not found")))?
        .clone();
    drop(catalog);

    state
        .storage
        .add_group_member(&group.id, &req.api_key_id, req.priority.unwrap_or(0))
        .await?;
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn handle_remove_group_member(
    State(state): State<AppState>,
    Path((slug, key_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let catalog = state.catalog.read().await;
    let group = catalog
        .groups
        .iter()
        .find(|g| g.slug == slug)
        .ok_or_else(|| AppError::not_found(format!("Group '{slug}' not found")))?
        .clone();
    drop(catalog);

    state
        .storage
        .remove_group_member(&group.id, &key_id)
        .await?;
    state.catalog.reload().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}
