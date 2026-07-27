use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
};

pub struct DecodoProvider {
    client: reqwest::Client,
}

impl Default for DecodoProvider {
    fn default() -> Self {
        Self {
            client: build_client(10), // Fast Search API targets sub-second responses
        }
    }
}

#[derive(Serialize)]
struct DecodoRequest<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    gl: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locale: Option<&'a str>,
}

#[derive(Deserialize)]
struct DecodoResponse {
    #[serde(default)]
    organic: Vec<DecodoResult>,
}

#[derive(Deserialize)]
struct DecodoResult {
    link: String,
    title: String,
    description: Option<String>,
    rank: usize,
}

#[async_trait]
impl SearchProvider for DecodoProvider {
    fn slug(&self) -> &str {
        "decodo"
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        // key_ref holds the base64 Basic-auth token value (no "Basic " prefix).
        let body = DecodoRequest {
            query: q.query,
            gl: q.country,
            locale: q.language,
        };

        let resp = self
            .client
            .post("https://fastsearch.decodo.com/v0/search")
            .header("Accept", "application/json")
            .header("Authorization", format!("Basic {}", q.api_key))
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();

        if status == 429 {
            debug!(provider = "decodo", status, "rate-limited");
            return Err(ProviderError::RateLimit);
        }
        if status == 401 || status == 403 {
            debug!(provider = "decodo", status, "auth/blocked");
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            debug!(provider = "decodo", status, body = %msg, "http error");
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: DecodoResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .organic
            .into_iter()
            .map(|r| SearchResult {
                domain: extract_domain(&r.link),
                url: r.link,
                title: r.title,
                snippet: r.description.unwrap_or_default(),
                rank: r.rank.saturating_sub(1),
                published_date: None,
                language: None,
                full_content: None,
                extra_snippets: None,
            })
            .take(q.n)
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "decodo", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
