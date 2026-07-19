use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
};

pub struct ExaProvider {
    client: reqwest::Client,
}

impl Default for ExaProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

#[derive(Serialize)]
struct ExaRequest<'a> {
    query: &'a str,
    #[serde(rename = "type")]
    search_type: &'static str,
    #[serde(rename = "numResults")]
    num_results: usize,
    contents: ExaContents,
}

#[derive(Serialize)]
struct ExaContents {
    highlights: bool,
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    url: String,
    title: Option<String>,
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
    highlights: Option<Vec<String>>,
}

#[async_trait]
impl SearchProvider for ExaProvider {
    fn slug(&self) -> &str {
        "exa"
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        _language: Option<&str>,
        _country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let body = ExaRequest {
            query,
            search_type: "auto",
            num_results: n,
            contents: ExaContents { highlights: true },
        };

        let resp = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            debug!(provider = "exa", status, "rate-limited");
            return Err(ProviderError::RateLimit);
        }
        if status == 401 || status == 403 {
            debug!(provider = "exa", status, "auth/blocked");
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            debug!(provider = "exa", status, body = %msg, "http error");
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: ExaResponse = resp
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
                title: r.title.unwrap_or_default(),
                snippet: r.highlights.unwrap_or_default().join(" … "),
                rank: i,
                published_date: r.published_date,
                language: None,
            })
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "exa", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
