/// DDG Bridge provider - calls the Python ddgs-bridge HTTP service.
///
/// The "api_key" for this provider is the bridge base URL (e.g. "http://localhost:8001").
/// This follows the same key rotation model as other providers: each bridge instance
/// is one entry in api_keys, with key_ref resolving to its URL.
///
/// In fan-out mode (no backend specified), the bridge tries all backends sequentially
/// and returns `backend_used` in the response. The executor then sees e.g. "ddg-yandex"
/// in the fallback chain instead of the generic "ddg".
use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchOutput, SearchProvider};

pub struct DdgBridgeProvider {
    client: reqwest::Client,
    backend: Option<String>,
}

impl DdgBridgeProvider {
    pub fn new_fanout() -> Self {
        Self {
            client: build_client(20),
            backend: None,
        }
    }

    pub fn new_backend(backend: impl Into<String>) -> Self {
        Self {
            client: build_client(20),
            backend: Some(backend.into()),
        }
    }
}

impl Default for DdgBridgeProvider {
    fn default() -> Self {
        Self::new_fanout()
    }
}

#[derive(Deserialize)]
struct BridgeResponse {
    results: Vec<BridgeResult>,
    backend_used: Option<String>,
}

#[derive(Deserialize)]
struct BridgeResult {
    url: Option<String>,
    title: Option<String>,
    snippet: Option<String>,
}

#[async_trait]
impl SearchProvider for DdgBridgeProvider {
    fn slug(&self) -> &str {
        match self.backend.as_deref() {
            None => "ddg",
            Some(b) => b,
        }
    }

    fn requires_api_key(&self) -> bool {
        true // key_ref holds the bridge URL
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let base = api_key.trim_end_matches('/');
        let mut req = self
            .client
            .get(format!("{base}/search"))
            .query(&[("q", query), ("n", &n.to_string())]);

        if let Some(lang) = language {
            req = req.query(&[("language", lang)]);
        }
        if let Some(c) = country {
            req = req.query(&[("country", c)]);
        }
        if let Some(backend) = &self.backend {
            req = req.query(&[("backend", backend.as_str())]);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();

        if status == 429 {
            return Err(ProviderError::RateLimit);
        }
        if status == 403 {
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: BridgeResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let url = r.url?;
                Some(SearchResult {
                    domain: extract_domain(&url),
                    url,
                    title: r.title.unwrap_or_default(),
                    snippet: r.snippet.unwrap_or_default(),
                    rank: i,
                    published_date: None,
                    language: None,
                })
            })
            .collect();

        if results.is_empty() {
            return Err(ProviderError::Empty);
        }

        // In fan-out mode the bridge reports which backend actually returned results.
        // Surface it as "ddg-<backend>" so the executor logs e.g. "ddg-yandex:ok".
        let effective_slug = if self.backend.is_none() {
            body.backend_used.map(|b| format!("ddg-{b}"))
        } else {
            None
        };

        let slug = effective_slug.as_deref().unwrap_or(self.slug());
        debug!(provider = slug, n = results.len(), "search complete");

        Ok(SearchOutput {
            results,
            effective_slug,
        })
    }
}
