use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchOutput, SearchProvider};

pub struct SerperProvider {
    client: reqwest::Client,
}

impl Default for SerperProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

#[derive(Serialize)]
struct SerperRequest<'a> {
    q: &'a str,
    num: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    gl: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hl: Option<&'a str>,
}

#[derive(Deserialize)]
struct SerperResponse {
    organic: Option<Vec<SerperResult>>,
}

#[derive(Deserialize)]
struct SerperResult {
    link: String,
    title: String,
    snippet: Option<String>,
    date: Option<String>,
}

#[async_trait]
impl SearchProvider for SerperProvider {
    fn slug(&self) -> &str {
        "serper"
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let body = SerperRequest {
            q: query,
            num: n,
            gl: country,
            hl: language,
        };

        let resp = self
            .client
            .post("https://google.serper.dev/search")
            .header("X-API-KEY", api_key)
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

        let body: SerperResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .organic
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.link),
                url: r.link,
                title: r.title,
                snippet: r.snippet.unwrap_or_default(),
                rank: i,
                published_date: r.date,
                language: None,
            })
            .collect();

        debug!(provider = "serper", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
