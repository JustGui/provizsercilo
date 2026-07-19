pub mod brave;
pub mod ddg_bridge;
pub mod exa;
pub mod mojeek;
pub mod searxng;
pub mod serper;
pub mod staan;
pub mod tavily;

use async_trait::async_trait;
use proviz_core::models::SearchResult;
use thiserror::Error;

/// Output of a successful search. `effective_slug` is set by meta-providers
/// (e.g. DDG bridge fan-out) to surface which sub-backend actually returned
/// results — e.g. "ddg-yandex" — so the executor can use it in the fallback chain.
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub effective_slug: Option<String>,
}

impl SearchOutput {
    pub fn new(results: Vec<SearchResult>) -> Self {
        Self {
            results,
            effective_slug: None,
        }
    }

    pub fn with_slug(results: Vec<SearchResult>, slug: impl Into<String>) -> Self {
        Self {
            results,
            effective_slug: Some(slug.into()),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("Rate limited (429)")]
    RateLimit,
    #[error("IP blocked or forbidden (403/401)")]
    Blocked,
    #[error("Request timed out")]
    Timeout,
    #[error("No results returned")]
    Empty,
    #[error("HTTP error {status}: {message}")]
    Http { status: u16, message: String },
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Request error: {0}")]
    Request(#[from] reqwest::Error),
}

impl ProviderError {
    /// Map this error to the event_type string used in rate_events and cooldowns.
    pub fn error_type_str(&self) -> &'static str {
        match self {
            Self::RateLimit => "rpm",
            Self::Blocked => "auth",
            Self::Timeout => "timeout",
            Self::Empty => "empty",
            _ => "error",
        }
    }

    pub fn is_ip_related(&self) -> bool {
        matches!(self, Self::RateLimit | Self::Blocked)
    }
}

/// Parameters for one search call. `api_key` is the resolved key value, or the
/// service URL for URL-based providers (SearXNG, DDG bridge). The enrichment
/// fields (`extra_snippets`, `full_content`, `max_snippets`, `min_score`,
/// `include_domains`, `exclude_domains`) are hints - a provider that doesn't
/// support one just ignores it; see `SearchProvider::supports_*`.
pub struct SearchQuery<'a> {
    pub query: &'a str,
    pub n: usize,
    pub language: Option<&'a str>,
    pub country: Option<&'a str>,
    pub api_key: &'a str,
    pub extra_snippets: bool,
    /// Requested body format hint: "markdown" | "html" | "text".
    pub full_content: Option<&'a str>,
    pub max_snippets: Option<usize>,
    pub min_score: Option<f64>,
    pub include_domains: &'a [String],
    pub exclude_domains: &'a [String],
}

/// Common interface for all search engine adapters.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn slug(&self) -> &str;

    /// True for providers whose key_ref holds an actual API key.
    /// False for providers where key_ref holds a service URL (SearXNG, DDG bridge).
    fn requires_api_key(&self) -> bool {
        true
    }

    /// True if this provider can fill `SearchResult::full_content` on request.
    fn supports_full_content(&self) -> bool {
        false
    }

    /// True if this provider can fill `SearchResult::extra_snippets` on request.
    fn supports_extra_snippets(&self) -> bool {
        false
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError>;
}

/// Extract the canonical domain from a URL string.
pub fn extract_domain(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_default()
}

/// Drop results with relative URLs or unresolvable domains.
pub fn sanitize_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    results
        .into_iter()
        .filter(|r| {
            !r.domain.is_empty() && (r.url.starts_with("http://") || r.url.starts_with("https://"))
        })
        .collect()
}

/// Build a shared reqwest client with sensible defaults and rustls.
pub fn build_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("ProvizSercilo/0.1 (search-router)")
        .build()
        .expect("Failed to build reqwest client")
}
