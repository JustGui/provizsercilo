use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use providers::SearchProvider;
use proviz_core::{
    language_profile::ProfileMatcher,
    rate_limit::{RateLimitState, UsageTracker},
    selector::Selector,
    storage::StorageBackend,
};
use tower_http::trace::TraceLayer;

use crate::{
    catalog::CatalogStore, config::Config, executor::Executor, handlers, stats::StatsTracker,
};

#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<Executor>,
    pub catalog: CatalogStore,
    pub storage: Arc<dyn StorageBackend>,
    pub cache: Arc<cache::QueryCache>,
    pub config: Arc<Config>,
    pub stats: Arc<StatsTracker>,
    pub rate_limit: RateLimitState,
    pub usage: UsageTracker,
    pub no_cache_providers: Arc<HashSet<String>>,
}

pub fn build_providers() -> HashMap<String, Arc<dyn SearchProvider>> {
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
        "ddg".to_string(),
        Arc::new(providers::ddg_bridge::DdgBridgeProvider::new_fanout()),
    );
    map.insert(
        "ddg-duckduckgo".to_string(),
        Arc::new(providers::ddg_bridge::DdgBridgeProvider::new_backend(
            "duckduckgo",
        )),
    );
    map.insert(
        "ddg-yahoo".to_string(),
        Arc::new(providers::ddg_bridge::DdgBridgeProvider::new_backend(
            "yahoo",
        )),
    );
    map.insert(
        "ddg-brave".to_string(),
        Arc::new(providers::ddg_bridge::DdgBridgeProvider::new_backend(
            "brave",
        )),
    );
    map.insert(
        "searxng".to_string(),
        Arc::new(providers::searxng::SearxngProvider::default()),
    );
    map
}

pub async fn build_app(config: Config) -> anyhow::Result<(Router, AppState)> {
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
    let stats = Arc::new(StatsTracker::new());
    let rate_limit = RateLimitState::default();
    let usage = UsageTracker::default();

    let profiles_content = std::fs::read_to_string(&config.profiles_path)
        .unwrap_or_else(|_| include_str!("../../profiles.toml").to_string());

    let profiles = ProfileMatcher::load_toml(&profiles_content).unwrap_or_else(|e| {
        tracing::warn!("Failed to parse profiles.toml: {e} - using empty profile set");
        ProfileMatcher::new(vec![])
    });

    let selector = Arc::new(Selector::new(rate_limit.clone(), usage.clone(), profiles));

    let providers = build_providers();
    let config = Arc::new(config);

    let executor = Arc::new(Executor::new(
        catalog.clone(),
        Arc::clone(&selector),
        providers,
        rate_limit.clone(),
        usage.clone(),
        config.secrets_dir.clone(),
        Arc::clone(&stats),
    ));

    let state = AppState {
        executor,
        catalog,
        storage,
        cache,
        config: Arc::clone(&config),
        stats,
        rate_limit,
        usage,
        no_cache_providers,
    };

    let admin_router = build_admin_router(state.clone());

    let router = Router::new()
        .route("/search", post(handlers::search::handle_search))
        .route("/report", post(handlers::report::handle_report))
        .route("/health", get(handlers::health::handle_health))
        .route("/stats", get(handlers::stats::handle_stats))
        .nest("/admin", admin_router)
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

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
        .route("/keys/:id", patch(handlers::admin::handle_update_key))
        .route("/keys/:id", delete(handlers::admin::handle_delete_key))
        .route(
            "/keys/:id/resolve",
            get(handlers::admin::handle_resolve_key),
        )
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
