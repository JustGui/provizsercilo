use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
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
    #[serde(rename = "startPublishedDate", skip_serializing_if = "Option::is_none")]
    start_published_date: Option<String>,
    #[serde(rename = "endPublishedDate", skip_serializing_if = "Option::is_none")]
    end_published_date: Option<String>,
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

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let (from, to) = q
            .freshness
            .map(|f| f.range(crate::operators::today()))
            .unwrap_or((None, None));
        let body = ExaRequest {
            query: q.query,
            search_type: "auto",
            num_results: q.n,
            contents: ExaContents { highlights: true },
            start_published_date: from.map(|d| format!("{d}T00:00:00.000Z")),
            end_published_date: to.map(|d| format!("{d}T23:59:59.999Z")),
        };

        let resp = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", q.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(ProviderError::Http {
                status,
                message: resp.text().await.unwrap_or_default(),
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
                full_content: None,
                extra_snippets: None,
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
