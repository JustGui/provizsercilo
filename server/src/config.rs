use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// PostgreSQL URL (postgres://…). When set, takes precedence over database_path.
    pub database_url: Option<String>,
    pub database_path: PathBuf,
    pub profiles_path: PathBuf,
    pub secrets_dir: PathBuf,
    pub admin_token: Option<String>,
    pub log_level: String,
    pub log_format: LogFormat,
    pub cache_ttl_secs: u64,
    /// TTL for the URL-keyed doc cache (full_content/extra_snippets), separate
    /// from the per-query SERP cache above - a page's content stays valid far
    /// longer than a ranking, and it's reused across different queries that
    /// happen to surface the same URL. Mirrors rtfc's own 6h URL-content cache.
    pub doc_cache_ttl_secs: u64,
    pub max_fallbacks: usize,
    /// Consecutive transient failures (timeout / 5xx) before a key is cooled down.
    pub transient_failures_before_cooldown: u32,
    pub billing_cooldown_secs: u64,
    pub probe_query: String,
    pub probe_interval_secs: u64,
    pub probe_concurrency: usize,
    pub probe_timeout_ms: u64,
    pub probe_backoff_secs: u64,
    pub probe_max_backoff_secs: u64,
    pub probe_max_backoff_transient_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogFormat {
    Json,
    Pretty,
}

impl Config {
    pub fn from_env() -> Self {
        let port = std::env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8090);

        let database_url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| u.starts_with("postgres"));

        let database_path = std::env::var("DATABASE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./proviz.db"));

        let profiles_path = std::env::var("PROFILES_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./profiles.toml"));

        let secrets_dir = std::env::var("SECRETS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/run/secrets"));

        let admin_token = std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty());

        let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "INFO".to_string());

        let log_format = match std::env::var("LOG_FORMAT").as_deref() {
            Ok("pretty") => LogFormat::Pretty,
            _ => LogFormat::Json,
        };

        let cache_ttl_secs = std::env::var("CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);

        let doc_cache_ttl_secs = std::env::var("DOC_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6 * 3600);

        let max_fallbacks = std::env::var("MAX_FALLBACKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let transient_failures_before_cooldown =
            std::env::var("TRANSIENT_FAILURES_BEFORE_COOLDOWN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(proviz_core::rate_limit::DEFAULT_TRANSIENT_FAILURES_BEFORE_COOLDOWN);

        Self {
            port,
            database_url,
            database_path,
            profiles_path,
            secrets_dir,
            admin_token,
            log_level,
            log_format,
            cache_ttl_secs,
            doc_cache_ttl_secs,
            max_fallbacks,
            transient_failures_before_cooldown,
            billing_cooldown_secs: env_number("BILLING_COOLDOWN_SECS", 21600),
            probe_query: std::env::var("PROBE_QUERY")
                .ok()
                .filter(|q| !q.trim().is_empty())
                .unwrap_or_else(|| "test".into()),
            probe_interval_secs: env_number("PROBE_INTERVAL_SECS", 30),
            probe_concurrency: env_number("PROBE_CONCURRENCY", 4) as usize,
            probe_timeout_ms: env_number("PROBE_TIMEOUT_MS", 5000),
            probe_backoff_secs: env_number("PROBE_BACKOFF_SECS", 60),
            probe_max_backoff_secs: env_number("PROBE_MAX_BACKOFF_SECS", 21600),
            probe_max_backoff_transient_secs: env_number("PROBE_MAX_BACKOFF_TRANSIENT_SECS", 600),
        }
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .max(1)
}
