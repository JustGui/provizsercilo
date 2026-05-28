/// SearXNG provider — calls a self-hosted SearXNG instance's JSON API.
///
/// The "api_key" for this provider is the SearXNG instance base URL
/// (e.g. "http://localhost:8080" or "https://searx.example.com").
/// Multiple instances can be registered as separate api_key entries,
/// and the selector will rotate between them naturally.
use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchProvider};

pub struct SearxngProvider {
    client: reqwest::Client,
}

impl Default for SearxngProvider {
    fn default() -> Self {
        Self {
            client: build_client(20),
        }
    }
}

#[derive(Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    url: String,
    title: String,
    content: Option<String>,
    publishedDate: Option<String>,
    language: Option<String>,
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    fn slug(&self) -> &str {
        "searxng"
    }

    /// api_key is the SearXNG instance base URL.
    fn requires_api_key(&self) -> bool {
        true // key_ref holds the instance URL
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        _country: Option<&str>,
        api_key: &str,
    ) -> Result<Vec<SearchResult>, ProviderError> {
        let base = api_key.trim_end_matches('/');
        let mut req = self.client.get(format!("{base}/search")).query(&[
            ("q", query),
            ("format", "json"),
            ("categories", "general"),
            ("pageno", "1"),
        ]);

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

        let body: SearxngResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .results
            .into_iter()
            .take(n)
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title,
                snippet: r.content.unwrap_or_default(),
                rank: i,
                published_date: r.publishedDate,
                language: r.language,
            })
            .collect();

        debug!(provider = "searxng", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(results)
    }
}
