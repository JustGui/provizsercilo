pub mod brave;
pub mod ddg_bridge;
pub mod mojeek;
pub mod searxng;
pub mod serper;
pub mod tavily;

use async_trait::async_trait;
use proviz_core::models::SearchResult;
use thiserror::Error;

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

/// Common interface for all search engine adapters.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn slug(&self) -> &str;

    /// True for providers whose key_ref holds an actual API key.
    /// False for providers where key_ref holds a service URL (SearXNG, DDG bridge).
    fn requires_api_key(&self) -> bool {
        true
    }

    /// `api_key` is the resolved key value, or the service URL for URL-based providers (SearXNG, DDG bridge).
    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        api_key: &str,
    ) -> Result<Vec<SearchResult>, ProviderError>;
}

/// Extract the canonical domain from a URL string.
pub fn extract_domain(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default()
}

/// Build a shared reqwest client with sensible defaults and rustls.
pub fn build_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("ProvizSercilo/0.1 (search-router)")
        .build()
        .expect("Failed to build reqwest client")
}
