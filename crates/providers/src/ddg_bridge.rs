/// DDG Bridge provider - calls the Python ddgs-bridge HTTP service.
///
/// The "api_key" for this provider is the bridge base URL (e.g. "http://localhost:8001").
/// This follows the same key rotation model as other providers: each bridge instance
/// is one entry in api_keys, with key_ref resolving to its URL.
use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchProvider};

pub struct DdgBridgeProvider {
    client: reqwest::Client,
}

impl Default for DdgBridgeProvider {
    fn default() -> Self {
        Self {
            client: build_client(20),
        }
    }
}

#[derive(Deserialize)]
struct BridgeResponse {
    results: Vec<BridgeResult>,
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
        "ddg"
    }

    /// For this provider, api_key is the bridge base URL.
    fn requires_api_key(&self) -> bool {
        true // key_ref holds the bridge URL - resolution still required
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        _country: Option<&str>,
        api_key: &str,
    ) -> Result<Vec<SearchResult>, ProviderError> {
        // api_key here is the bridge base URL.
        let base = api_key.trim_end_matches('/');
        let mut req = self
            .client
            .get(format!("{base}/search"))
            .query(&[("q", query), ("n", &n.to_string())]);

        if let Some(lang) = language {
            req = req.query(&[("language", lang)]);
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

        debug!(provider = "ddg", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(results)
    }
}
