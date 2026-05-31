use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchOutput, SearchProvider};

pub struct TavilyProvider {
    client: reqwest::Client,
}

impl Default for TavilyProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

#[derive(Serialize)]
struct TavilyRequest<'a> {
    api_key: &'a str,
    query: &'a str,
    max_results: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_depth: Option<&'static str>,
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    url: String,
    title: String,
    content: Option<String>,
    published_date: Option<String>,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn slug(&self) -> &str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        _language: Option<&str>,
        _country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let body = TavilyRequest {
            api_key,
            query,
            max_results: n,
            search_depth: Some("basic"),
        };

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimit);
        }
        if status == 401 || status == 403 {
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .results
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title,
                snippet: r.content.unwrap_or_default(),
                rank: i,
                published_date: r.published_date,
                language: None,
            })
            .collect();

        debug!(provider = "tavily", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
