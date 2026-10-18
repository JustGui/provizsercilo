use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use providers::{ContentProvider, SearchProvider};
use proviz_core::{
    language_profile::ProfileMatcher,
    rate_limit::{RateLimitState, UsageTracker},
    selector::Selector,
    storage::StorageBackend,
};
use tower_http::trace::TraceLayer;

use crate::{
    catalog::CatalogStore, config::Config, content_executor::ContentExecutor, executor::Executor,
    handlers, stats::StatsTracker,
};

#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<Executor>,
    pub probes: Arc<crate::probes::ProbeRunner>,
    pub content_executor: Arc<ContentExecutor>,
    pub catalog: CatalogStore,
    pub storage: Arc<dyn StorageBackend>,
    pub cache: Arc<cache::QueryCache>,
    pub doc_cache: Arc<cache::DocCache>,
    pub config: Arc<Config>,
    pub stats: Arc<StatsTracker>,
    pub rate_limit: RateLimitState,
    pub usage: UsageTracker,
    pub no_cache_providers: Arc<HashSet<String>>,
}

pub fn build_providers(
    deepseek_localizations: HashMap<String, std::net::IpAddr>,
) -> HashMap<String, Arc<dyn SearchProvider>> {
    let mut map: HashMap<String, Arc<dyn SearchProvider>> = HashMap::new();
    map.insert(
        "brave".to_string(),
        Arc::new(providers::brave::BraveProvider::default()),
    );
    map.insert(
        "tavily".to_string(),
        Arc::new(providers::tavily::TavilyProvider::default()),
    );
    map.insert(
        "mojeek".to_string(),
        Arc::new(providers::mojeek::MojeekProvider::default()),
    );
    map.insert(
        "serper".to_string(),
        Arc::new(providers::serper::SerperProvider::default()),
    );
    map.insert(
        "searxng".to_string(),
        Arc::new(providers::searxng::SearxngProvider::default()),
    );
    map.insert(
        "exa".to_string(),
        Arc::new(providers::exa::ExaProvider::default()),
    );
    map.insert(
        "staan".to_string(),
        Arc::new(providers::staan::StaanProvider::default()),
    );
    map.insert(
        "decodo".to_string(),
        Arc::new(providers::decodo::DecodoProvider::default()),
    );
    map.insert(
        "you-com".to_string(),
        Arc::new(providers::you_com::YouComProvider::default()),
    );
    map.insert(
        "parallel".to_string(),
        Arc::new(providers::parallel::ParallelProvider::default()),
    );
    // DDG bridge — one adapter per backend so each has its own cooldown in proviz-sercilo.
    // The fan-out "ddg" adapter is intentionally removed; proviz owns the fallback logic.
    for backend in &[
        "yandex",
        "mojeek",
        "startpage",
        "yahoo",
        "google",
        "duckduckgo",
        "brave",
    ] {
        map.insert(
            format!("ddg-{backend}"),
            Arc::new(providers::ddg_bridge::DdgBridgeProvider::new_backend(
                *backend,
            )),
        );
    }
    map.insert(
        "deepseek".to_string(),
        Arc::new(
            providers::deepseek::DeepSeekProvider::default()
                .with_country_localizations(deepseek_localizations),
        ),
    );
    map
}

/// The page-extraction pool: providers implementing `ContentProvider`. Keyed by
/// the same slug as `build_providers` so a candidate row selects the right one.
pub fn build_content_providers() -> HashMap<String, Arc<dyn ContentProvider>> {
    let mut map: HashMap<String, Arc<dyn ContentProvider>> = HashMap::new();
    map.insert(
        "you-com".to_string(),
        Arc::new(providers::you_com::YouComProvider::default()),
    );
    map.insert(
        "parallel".to_string(),
        Arc::new(providers::parallel::ParallelProvider::default()),
    );
    map.insert(
        "deepseek".to_string(),
        Arc::new(providers::deepseek::DeepSeekProvider::default()),
    );
    map
}

pub async fn build_app(config: Config) -> anyhow::Result<(Router, AppState)> {
    let deepseek_localizations =
        providers::deepseek::parse_country_localizations(&config.deepseek_localization_ips)
            .map_err(anyhow::Error::msg)?;
    let storage: Arc<dyn StorageBackend> = if let Some(ref url) = config.database_url {
        tracing::info!(url = %url, "connecting to PostgreSQL");
        Arc::new(storage_postgres::PgStorage::connect(url).await?)
    } else {
        tracing::info!(path = %config.database_path.display(), "opening SQLite database");
        Arc::new(storage_sqlite::Storage::open(&config.database_path)?)
    };

    let catalog = CatalogStore::new(Arc::clone(&storage)).await?;

    let no_cache_providers: Arc<HashSet<String>> = {
        let cat = catalog.read().await;
        Arc::new(
            cat.providers
                .iter()
                .filter(|p| p.no_cache)
                .map(|p| p.slug.clone())
                .collect(),
        )
    };

    let cache = Arc::new(cache::QueryCache::new());
    let doc_cache = Arc::new(cache::DocCache::new());
    let stats = Arc::new(StatsTracker::new());
    let rate_limit =
        RateLimitState::with_transient_threshold(config.transient_failures_before_cooldown)
            .with_billing_cooldown(config.billing_cooldown_secs)
            .with_auth_cooldown(
                config.auth_recent_success_secs,
                config.auth_repeat_window_secs,
                config.auth_first_cooldown_secs,
            );
    let usage = UsageTracker::default();

    let profiles_content = std::fs::read_to_string(&config.profiles_path)
        .unwrap_or_else(|_| include_str!("../../profiles.toml").to_string());

    let profiles = ProfileMatcher::load_toml(&profiles_content).unwrap_or_else(|e| {
        tracing::warn!("Failed to parse profiles.toml: {e} - using empty profile set");
        ProfileMatcher::new(vec![])
    });

    let selector = Arc::new(Selector::new(rate_limit.clone(), usage.clone(), profiles));

    let providers = build_providers(deepseek_localizations);
    let config = Arc::new(config);

    let executor = Arc::new(
        Executor::new(
            catalog.clone(),
            Arc::clone(&selector),
            providers,
            rate_limit.clone(),
            usage.clone(),
            config.secrets_dir.clone(),
            Arc::clone(&stats),
        )
        .with_pacing(config.pacing_safety, config.aux_queue_fraction),
    );

    let content_executor = Arc::new(ContentExecutor::new(
        catalog.clone(),
        Arc::clone(&selector),
        build_content_providers(),
        rate_limit.clone(),
        usage.clone(),
        config.secrets_dir.clone(),
        Arc::clone(&stats),
    ));

    let probes = Arc::new(crate::probes::ProbeRunner::new(
        Arc::clone(&executor),
        catalog.clone(),
        rate_limit.clone(),
        Arc::clone(&config),
    ));

    let state = AppState {
        probes: Arc::clone(&probes),
        executor,
        content_executor,
        catalog,
        storage,
        cache,
        doc_cache,
        config: Arc::clone(&config),
        stats,
        rate_limit,
        usage,
        no_cache_providers,
    };

    let admin_router = build_admin_router(state.clone());

    let router = Router::new()
        .route("/search", post(handlers::search::handle_search))
        .route("/contents", post(handlers::contents::handle_contents))
        .route("/report", post(handlers::report::handle_report))
        .route("/health", get(handlers::health::handle_health))
        .route("/stats", get(handlers::stats::handle_stats))
        .nest("/admin", admin_router)
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // A Weak reference lets the worker stop once the server state is dropped.
    crate::probes::spawn_worker(Arc::downgrade(&probes), config.probe_interval_secs);
    Ok((router, state))
}

fn build_admin_router(app_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/reload", post(handlers::admin::handle_reload))
        .route("/providers", get(handlers::admin::handle_list_providers))
        .route("/providers", post(handlers::admin::handle_create_provider))
        .route(
            "/providers/:slug",
            patch(handlers::admin::handle_update_provider),
        )
        .route(
            "/providers/:slug/keys",
            get(handlers::admin::handle_list_keys),
        )
        .route(
            "/providers/:slug/keys",
            post(handlers::admin::handle_create_key),
        )
        .route("/keys/parked", get(handlers::admin::handle_parked_keys))
        .route("/keys/:id/probe", post(handlers::admin::handle_probe_key))
        .route("/keys/:id", patch(handlers::admin::handle_update_key))
        .route("/keys/:id", delete(handlers::admin::handle_delete_key))
        .route(
            "/keys/:id/resolve",
            get(handlers::admin::handle_resolve_key),
        )
        .route("/costs", get(handlers::admin::handle_cost_stats))
        .route("/groups", get(handlers::admin::handle_list_groups))
        .route("/groups", post(handlers::admin::handle_create_group))
        .route("/groups/:slug", put(handlers::admin::handle_upsert_group))
        .route(
            "/groups/:slug",
            delete(handlers::admin::handle_delete_group),
        )
        .route(
            "/groups/:slug/members",
            post(handlers::admin::handle_add_group_member),
        )
        .route(
            "/groups/:slug/members",
            delete(handlers::admin::handle_clear_group_members),
        )
        .route(
            "/groups/:slug/members/:key_id",
            delete(handlers::admin::handle_remove_group_member),
        )
        .layer(middleware::from_fn_with_state(
            app_state,
            handlers::admin::require_admin_token,
        ))
}

#[cfg(test)]
mod admin_probe_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn malformed_localization_config_fails_before_opening_storage() {
        let mut config = Config::from_env();
        config.deepseek_localization_ips = r#"{"fr":"not-an-ip"}"#.into();
        let result = build_app(config).await;
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("DEEPSEEK_LOCALIZATION_IPS"));
    }

    #[tokio::test]
    async fn parked_key_and_probe_routes_require_admin_authentication() {
        let mut config = Config::from_env();
        config.database_url = None;
        config.database_path = ":memory:".into();
        config.admin_token = Some("admin-test-only".into());
        let (router, state) = build_app(config).await.unwrap();
        state
            .rate_limit
            .report_error("test-key-id", proviz_core::rate_limit::ErrorType::Billing);
        for (method, path) in [
            ("GET", "/admin/keys/parked"),
            ("POST", "/admin/keys/test-key-id/probe"),
        ] {
            for token in [None, Some("wrong-token")] {
                let mut request = Request::builder().method(method).uri(path);
                if let Some(token) = token {
                    request = request.header("Authorization", format!("Bearer {token}"));
                }
                let response = router
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/keys/parked")
                    .header("Authorization", "Bearer admin-test-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body[0]["key_id"], "test-key-id");
        assert_eq!(body[0]["reason"], "billing");
        assert!(body[0]["since"].as_str().is_some());
        assert!(body[0].get("key_ref").is_none());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/keys/test-key-id/probe")
                    .header("Authorization", "Bearer admin-test-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
