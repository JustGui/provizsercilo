pub mod brave;
pub mod ddg_bridge;
pub mod decodo;
pub mod deepseek;
pub mod exa;
pub mod mojeek;
pub mod operators;
pub mod parallel;
pub mod searxng;
pub mod serper;
pub mod staan;
pub mod tavily;
pub mod you_com;

use async_trait::async_trait;
use proviz_core::models::{FullContent, SearchResult};
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
    /// Classify the HTTP status *and* body before discarding any provider detail.
    pub fn error_type(&self) -> proviz_core::rate_limit::ErrorType {
        use proviz_core::rate_limit::ErrorType as E;
        match self {
            Self::RateLimit => E::Rpm,
            Self::Blocked => E::Auth,
            Self::Timeout => E::Timeout,
            Self::Empty => E::Empty,
            Self::Request(e) if e.is_timeout() => E::Timeout,
            Self::Http { status, message } => {
                let body = message.to_ascii_lowercase();
                let billing = [
                    "not enough credit",
                    "credit balance",
                    "credits exhausted",
                    "insufficient credit",
                    "quota exceeded",
                    "quota_exceeded",
                    "quota_limited",
                    "pay-as-you-go limit",
                    "payment_required",
                    "payment required",
                    "billing",
                    "credit limit",
                    "out of credits",
                    "insufficient_quota",
                ]
                .iter()
                .any(|pattern| body.contains(pattern));
                if matches!(status, 402 | 433) || (matches!(status, 400 | 403) && billing) {
                    E::Billing
                } else if matches!(status, 401 | 403)
                    || (*status == 400
                        && [
                            "invalid api key",
                            "invalid_api_key",
                            "invalid key",
                            "invalid credential",
                            "unauthorized",
                        ]
                        .iter()
                        .any(|p| body.contains(p)))
                {
                    E::Auth
                } else if *status == 422
                    || (*status == 400
                        && [
                            "invalid",
                            "validation",
                            "missing required",
                            "unsupported parameter",
                            "must be",
                            "is required",
                        ]
                        .iter()
                        .any(|p| body.contains(p)))
                {
                    E::Request
                } else {
                    match status {
                        401 | 403 => E::Auth,
                        429 => E::Rpm,
                        408 | 504 => E::Timeout,
                        _ => E::Error,
                    }
                }
            }
            _ => E::Error,
        }
    }

    /// Log only the numeric status for auth/billing HTTP failures, never the
    /// provider body or resolved credentials. Shared by user calls and probes.
    pub fn log_auth_billing_status(&self) {
        if let Self::Http { status, .. } = self {
            if matches!(
                self.error_type(),
                proviz_core::rate_limit::ErrorType::Auth
                    | proviz_core::rate_limit::ErrorType::Billing
            ) {
                tracing::warn!(
                    provider_http_status = *status,
                    "provider auth/billing failure"
                );
            }
        }
    }

    pub fn error_type_str(&self) -> &'static str {
        self.error_type().as_str()
    }

    pub fn is_ip_related(&self) -> bool {
        matches!(
            self.error_type(),
            proviz_core::rate_limit::ErrorType::Rpm | proviz_core::rate_limit::ErrorType::Auth
        )
    }
}

/// Parameters for one search call. `api_key` is the resolved key value, or the
/// service URL for URL-based providers (SearXNG, DDG bridge). The enrichment
/// fields (`extra_snippets`, `full_content`, `max_snippets`, `min_score`) are hints -
/// a provider that doesn't support one just ignores it; see `SearchProvider::supports_*`.
///
/// `include_domains` / `exclude_domains` reach the adapter only when it
/// `supports_include_domains` (hosts only); for every other provider the executor has
/// already rewritten them into `site:` operators in `query` and filters the answer
/// (see `operators`). `freshness` is mapped by each adapter that has a date parameter.
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
    pub freshness: Option<&'a operators::Freshness>,
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

    /// True if the provider API restricts results to `include_domains` itself. False
    /// (the free engines) → the executor writes `site:` operators into the query.
    fn supports_include_domains(&self) -> bool {
        false
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError>;
}

// ---------------------------------------------------------------------------
// Page content extraction (the /contents endpoint's provider pool)
// ---------------------------------------------------------------------------

/// One page-extraction call. `urls` is a batch (the executor already chunked it
/// to `ContentProvider::max_batch`). `api_key` is the resolved key value.
pub struct ContentQuery<'a> {
    pub urls: &'a [String],
    /// Requested body format hint: "markdown" | "html" | "text".
    pub format: &'a str,
    /// Optional focus hint passed through to providers that support it (Parallel).
    pub objective: Option<&'a str>,
    pub api_key: &'a str,
}

/// A successfully extracted page.
pub struct ContentDoc {
    pub url: String,
    pub title: Option<String>,
    pub published_date: Option<String>,
    pub content: FullContent,
}

/// Output of one `ContentProvider::fetch` call. `docs` are the pages that came
/// back with a body; `failed` lists `(url, reason)` for pages this provider
/// could not extract — those are *soft* per-URL misses, the executor retries
/// them against the next provider without cooling this one down.
pub struct ContentOutput {
    pub docs: Vec<ContentDoc>,
    pub failed: Vec<(String, String)>,
}

/// Common interface for page-content extraction adapters. A provider may
/// implement both this and `SearchProvider`.
#[async_trait]
pub trait ContentProvider: Send + Sync {
    fn slug(&self) -> &str;

    /// Max URLs accepted in a single upstream call. The executor slices larger
    /// request lists into chunks of this size.
    fn max_batch(&self) -> usize {
        1
    }

    async fn fetch(&self, q: ContentQuery<'_>) -> Result<ContentOutput, ProviderError>;
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

#[cfg(test)]
mod error_tests {
    use super::*;
    use proviz_core::rate_limit::ErrorType as E;

    #[test]
    fn production_http_failures_are_classified_before_fallback() {
        for (status, body, expected) in [
            (
                400,
                r#"{"message":"Not enough credits","statusCode":400}"#,
                E::Billing,
            ),
            (
                433,
                r#"{"detail":{"error":"This request exceeds the pay-as-you-go limit. You can increase ..."}}"#,
                E::Billing,
            ),
            (
                402,
                r#"{"error":"payment_required","message":"Your prepaid credit balance has been depleted..."}"#,
                E::Billing,
            ),
            (
                400,
                r#"{"message":["include_domains contains an invalid domain"],"error":"Bad Request","statusCode":400}"#,
                E::Request,
            ),
            (403, r#"{"error":"quota exceeded"}"#, E::Billing),
            (403, r#"{"error":"QUOTA_LIMITED"}"#, E::Billing),
            (403, "invalid API key", E::Auth),
            (400, "invalid API key", E::Auth),
            (401, "unauthorized", E::Auth),
            (429, "rate limited", E::Rpm),
            (422, "invalid query parameter", E::Request),
            (400, "invalid value for count", E::Request),
            (500, "internal error", E::Error),
            (504, "gateway timeout", E::Timeout),
        ] {
            assert_eq!(
                ProviderError::Http {
                    status,
                    message: body.into()
                }
                .error_type(),
                expected
            );
        }
        assert_eq!(ProviderError::Empty.error_type(), E::Empty);
        assert_eq!(
            ProviderError::Parse("brave: invalid JSON".into()).error_type(),
            E::Error
        );
    }
}
