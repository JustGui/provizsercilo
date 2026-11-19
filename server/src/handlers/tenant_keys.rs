//! `/admin/tenant-keys/:provider[/:tenant]`: provider keys sercilo creates itself,
//! one per caller tenant (`team:<id>`, `client:<id>`), through the vendor's key API.
//!
//! The vendor shows a new key's value once, so it is stored sealed in `key_ref`
//! (`enc:v1:`, see `key_resolver::seal_key`) - the one exception to "key values
//! are never stored in the DB". Rate limits, pacing and pricing are copied from
//! the provider's shared key, so every tenant gets the same per-key budget.
//! A tenant key is used only by `/search` calls that name its tenant
//! (`Catalog::candidates_for`). Only Staan is supported.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use providers::staan_keys::{KeyAdminError, StaanKeys, DEFAULT_BASE};
use proviz_core::{key_resolver, models::ApiKey};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{app::AppState, error::AppError, handlers::search::valid_tenants};

/// Env var / secrets file of the Staan console key (the only key allowed on /v1/keys).
pub const STAAN_CONSOLE_KEY_REF: &str = "STAAN_CONSOLE_KEY";
const REVOKED_REF: &str = "revoked";

/// Serialises create/delete in this process; the partial unique index on
/// (provider_id, tenant) covers several replicas.
fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Serialize)]
pub struct TenantKeyPublic {
    pub tenant: String,
    pub key_id: String,
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

impl From<&ApiKey> for TenantKeyPublic {
    fn from(k: &ApiKey) -> Self {
        Self {
            tenant: k.tenant.clone().unwrap_or_default(),
            key_id: k.id.clone(),
            label: k.label.clone(),
            created_at: k.created_at.clone(),
            last_used_at: k.last_used_at.clone(),
        }
    }
}

#[derive(Deserialize)]
pub struct EnsureRequest {
    /// Shown in the vendor console / usage API, e.g. "Faktolumo - TF1 / Desk".
    pub name: String,
}

#[derive(Serialize)]
pub struct EnsureResponse {
    #[serde(flatten)]
    pub key: TenantKeyPublic,
    pub created: bool,
}

fn bad_request(message: &str, code: &str) -> AppError {
    AppError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
        code: code.into(),
        fallback_chain: None,
        attempts: None,
        retry_after_secs: None,
    }
}

fn vendor_error(e: KeyAdminError) -> AppError {
    let (status, code) = match e {
        KeyAdminError::LimitReached => (StatusCode::CONFLICT, "tenant_key_limit"),
        KeyAdminError::Auth(_) => (StatusCode::BAD_GATEWAY, "vendor_auth"),
        KeyAdminError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "vendor_rate_limited"),
        _ => (StatusCode::BAD_GATEWAY, "vendor_error"),
    };
    AppError {
        status,
        message: e.to_string(),
        code: code.into(),
        fallback_chain: None,
        attempts: None,
        retry_after_secs: None,
    }
}

fn check_provider(slug: &str) -> Result<(), AppError> {
    if slug == "staan" {
        Ok(())
    } else {
        Err(bad_request(
            "tenant keys are supported for staan only",
            "unsupported_provider",
        ))
    }
}

fn check_tenant(tenant: &str) -> Result<(), AppError> {
    if valid_tenants(&[tenant.to_string()]) {
        Ok(())
    } else {
        Err(bad_request(
            "tenant must be team:<id> or client:<id>",
            "invalid_tenant",
        ))
    }
}

fn client(state: &AppState) -> Result<(StaanKeys, String), AppError> {
    let console = key_resolver::resolve_key(STAAN_CONSOLE_KEY_REF, &state.config.secrets_dir)
        .map_err(|_| {
            AppError::service_unavailable(format!("{STAAN_CONSOLE_KEY_REF} not configured"))
        })?;
    let base = std::env::var("STAAN_API_BASE").unwrap_or_else(|_| DEFAULT_BASE.into());
    Ok((StaanKeys::new(base), console))
}

async fn tenant_keys(state: &AppState, provider_id: &str) -> Result<Vec<ApiKey>, AppError> {
    Ok(state
        .storage
        .list_keys_for_provider(provider_id)
        .await?
        .into_iter()
        .filter(|k| k.is_active && k.tenant.is_some())
        .collect())
}

pub async fn handle_list_tenant_keys(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<TenantKeyPublic>>, AppError> {
    check_provider(&slug)?;
    let provider = state.storage.get_provider_by_slug(&slug).await?;
    let keys = tenant_keys(&state, &provider.id).await?;
    Ok(Json(keys.iter().map(TenantKeyPublic::from).collect()))
}

/// Idempotent: returns the tenant's active key, creating one at the vendor if none.
pub async fn handle_ensure_tenant_key(
    State(state): State<AppState>,
    Path((slug, tenant)): Path<(String, String)>,
    Json(req): Json<EnsureRequest>,
) -> Result<(StatusCode, Json<EnsureResponse>), AppError> {
    check_provider(&slug)?;
    check_tenant(&tenant)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(bad_request("name must not be blank", "invalid_name"));
    }
    let _guard = lock().lock().await;
    let provider = state.storage.get_provider_by_slug(&slug).await?;
    let existing = |keys: &[ApiKey]| {
        keys.iter()
            .find(|k| k.tenant.as_deref() == Some(&tenant))
            .map(TenantKeyPublic::from)
    };
    if let Some(key) = existing(&tenant_keys(&state, &provider.id).await?) {
        return Ok((
            StatusCode::OK,
            Json(EnsureResponse {
                key,
                created: false,
            }),
        ));
    }

    // Budgets and pricing follow the shared key: Staan limits every key alike.
    let all = state.storage.list_keys_for_provider(&provider.id).await?;
    let template = all.iter().find(|k| k.is_active && k.tenant.is_none());
    let (vendor, console) = client(&state)?;
    let created = vendor.create(&console, name).await.map_err(vendor_error)?;
    let sealed = match key_resolver::seal_key(&created.key, &state.config.secrets_dir) {
        Ok(s) => s,
        Err(e) => {
            let _ = vendor.delete(&console, &created.id).await;
            return Err(AppError::service_unavailable(e.to_string()));
        }
    };
    let key = ApiKey {
        id: Uuid::new_v4().to_string(),
        provider_id: provider.id.clone(),
        label: format!("{slug}-{tenant}"),
        key_ref: sealed,
        is_active: true,
        rps_limit: template.and_then(|t| t.rps_limit).or(Some(20.0)),
        rpm_limit: template.and_then(|t| t.rpm_limit),
        rpd_limit: template.and_then(|t| t.rpd_limit),
        max_queue_ms: template.and_then(|t| t.max_queue_ms).or(Some(3000)),
        last_used_at: None,
        created_at: String::new(),
        cost_per_mille: template.and_then(|t| t.cost_per_mille),
        currency: template.and_then(|t| t.currency.clone()),
        tenant: Some(tenant.clone()),
        external_id: Some(created.id.clone()),
    };
    if let Err(e) = state.storage.create_api_key(key).await {
        // Another replica won the race (unique index): drop ours, return theirs.
        let _ = vendor.delete(&console, &created.id).await;
        if let Some(key) = existing(&tenant_keys(&state, &provider.id).await?) {
            return Ok((
                StatusCode::OK,
                Json(EnsureResponse {
                    key,
                    created: false,
                }),
            ));
        }
        return Err(e.into());
    }
    state.catalog.reload().await?;
    let keys = tenant_keys(&state, &provider.id).await?;
    let key =
        existing(&keys).ok_or_else(|| AppError::internal("tenant key vanished after insert"))?;
    info!(provider = %slug, tenant = %tenant, key_id = %key.key_id, "tenant key created");
    Ok((
        StatusCode::CREATED,
        Json(EnsureResponse { key, created: true }),
    ))
}

/// Deletes the tenant's keys at the vendor, then deactivates them and wipes the
/// sealed value. Idempotent; a tenant with no key is a no-op.
pub async fn handle_delete_tenant_key(
    State(state): State<AppState>,
    Path((slug, tenant)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    check_provider(&slug)?;
    check_tenant(&tenant)?;
    let _guard = lock().lock().await;
    let provider = state.storage.get_provider_by_slug(&slug).await?;
    let keys: Vec<ApiKey> = tenant_keys(&state, &provider.id)
        .await?
        .into_iter()
        .filter(|k| k.tenant.as_deref() == Some(&tenant))
        .collect();
    if keys.is_empty() {
        return Ok(Json(serde_json::json!({"ok": true, "deleted": 0})));
    }
    let (vendor, console) = client(&state)?;
    for key in &keys {
        if let Some(external) = &key.external_id {
            vendor.delete(&console, external).await.map_err(|e| {
                warn!(tenant = %tenant, key_id = %key.id, "vendor key delete failed");
                vendor_error(e)
            })?;
        }
        state
            .storage
            .update_api_key_fields(
                &key.id,
                None,
                Some(false),
                Some(REVOKED_REF.into()),
                None,
                None,
                None,
                None,
                None,
            )
            .await?;
    }
    state.catalog.reload().await?;
    info!(provider = %slug, tenant = %tenant, n = keys.len(), "tenant key deleted");
    Ok(Json(serde_json::json!({"ok": true, "deleted": keys.len()})))
}

#[cfg(test)]
mod tests {
    use crate::{app::build_app, config::Config};
    use axum::{
        body::{to_bytes, Body},
        extract::{Path, State},
        http::{HeaderMap, Request, StatusCode},
        routing::{delete, post},
        Json, Router,
    };
    use base64::Engine;
    use proviz_core::{
        key_resolver,
        models::{ApiKey, Provider},
    };
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    type Calls = Arc<Mutex<Vec<String>>>;

    async fn mock_staan() -> (String, Calls) {
        let calls: Calls = Arc::default();
        let app = Router::new()
            .route(
                "/v1/keys",
                post(|State(c): State<Calls>, h: HeaderMap, Json(b): Json<serde_json::Value>| async move {
                    assert_eq!(h["authorization"], "Bearer console-secret");
                    let mut c = c.lock().unwrap();
                    c.push(format!("POST {}", b["name"].as_str().unwrap()));
                    let n = c.len();
                    (StatusCode::CREATED, Json(serde_json::json!({"id": format!("ext-{n}"), "key": format!("stn_secret_{n}"), "name": b["name"]})))
                }),
            )
            .route(
                "/v1/keys/:id",
                delete(|State(c): State<Calls>, Path(id): Path<String>| async move {
                    c.lock().unwrap().push(format!("DELETE {id}"));
                    Json(serde_json::json!({"deleted": true, "id": id}))
                }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), calls)
    }

    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", "Bearer admin-test-only")
            .header("content-type", "application/json")
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn ensure_is_idempotent_routes_the_tenant_and_delete_revokes() {
        let (base, calls) = mock_staan().await;
        std::env::set_var("STAAN_API_BASE", &base);
        let secrets = std::env::temp_dir().join(format!("sercilo-tenant-{}", std::process::id()));
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join(super::STAAN_CONSOLE_KEY_REF), "console-secret").unwrap();
        std::fs::write(
            secrets.join(key_resolver::ENCRYPTION_KEY_REF),
            base64::engine::general_purpose::STANDARD.encode([3u8; 32]),
        )
        .unwrap();

        let mut config = Config::from_env();
        config.database_url = None;
        config.database_path = ":memory:".into();
        config.admin_token = Some("admin-test-only".into());
        config.secrets_dir = secrets.clone();
        let (router, state) = build_app(config).await.unwrap();
        let provider = state
            .storage
            .create_provider(Provider {
                id: "p-staan".into(),
                slug: "staan".into(),
                name: "Staan".into(),
                base_url: None,
                is_active: true,
                priority: 1,
                avg_latency_ms: None,
                coverage_scores: Default::default(),
                notes: None,
                created_at: String::new(),
                no_cache: false,
            })
            .await
            .unwrap();
        let shared: ApiKey = serde_json::from_value(serde_json::json!({
            "id": "staan-1", "provider_id": provider.id, "label": "staan-1", "key_ref": "STAAN_KEY_1",
            "is_active": true, "rps_limit": 20.0, "max_queue_ms": 3000, "cost_per_mille": 2.0,
            "currency": "EUR", "created_at": ""
        }))
        .unwrap();
        state.storage.create_api_key(shared).await.unwrap();
        state.catalog.reload().await.unwrap();

        let uri = "/admin/tenant-keys/staan/team:abc";
        let (s, body) = call(
            &router,
            "PUT",
            uri,
            Some(serde_json::json!({"name": "Faktolumo - TF1 / Desk"})),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        assert_eq!(body["created"], true);
        let (s, body) = call(
            &router,
            "PUT",
            uri,
            Some(serde_json::json!({"name": "again"})),
        )
        .await;
        assert_eq!(
            (s, body["created"].clone()),
            (StatusCode::OK, serde_json::json!(false))
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["POST Faktolumo - TF1 / Desk".to_string()]
        );

        let pool = state
            .catalog
            .read()
            .await
            .candidates_for(None, &["team:abc".into()]);
        assert_eq!(pool.len(), 1);
        let key = &pool[0].api_key;
        assert_eq!(
            (key.rps_limit, key.max_queue_ms, key.cost_per_mille),
            (Some(20.0), Some(3000), Some(2.0))
        );
        assert_eq!(
            key_resolver::resolve_key(&key.key_ref, &secrets).unwrap(),
            "stn_secret_1"
        );
        assert!(state
            .catalog
            .read()
            .await
            .candidates(None)
            .iter()
            .all(|c| c.api_key.id == "staan-1"));

        let (s, list) = call(&router, "GET", "/admin/tenant-keys/staan", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert!(!list.to_string().contains("stn_secret") && !list.to_string().contains("enc:v1"));
        let (_, keys) = call(&router, "GET", "/admin/providers/staan/keys", None).await;
        assert!(
            !keys.to_string().contains("stn_secret")
                && keys.to_string().contains("enc:v1:<sealed>")
        );

        let (s, body) = call(&router, "DELETE", uri, None).await;
        assert_eq!(
            (s, body["deleted"].clone()),
            (StatusCode::OK, serde_json::json!(1))
        );
        assert_eq!(calls.lock().unwrap().last().unwrap(), "DELETE ext-1");
        let pool = state
            .catalog
            .read()
            .await
            .candidates_for(None, &["team:abc".into()]);
        assert_eq!(pool[0].api_key.id, "staan-1", "back on the shared key");
        let (s, _) = call(&router, "DELETE", uri, None).await;
        assert_eq!(s, StatusCode::OK);

        let (s, _) = call(
            &router,
            "PUT",
            "/admin/tenant-keys/staan/user:x",
            Some(serde_json::json!({"name": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = call(
            &router,
            "PUT",
            "/admin/tenant-keys/brave/team:abc",
            Some(serde_json::json!({"name": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(&secrets).unwrap();
    }
}
