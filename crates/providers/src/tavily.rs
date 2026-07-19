use async_trait::async_trait;
use proviz_core::models::{FullContent, SearchResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
};

pub struct TavilyProvider {
    client: reqwest::Client,
}

impl Default for TavilyProvider {
    fn default() -> Self {
        Self {
            client: build_client(20), // "advanced" search_depth is slower than "basic"
        }
    }
}

#[derive(Serialize)]
struct TavilyRequest<'a> {
    api_key: &'a str,
    query: &'a str,
    max_results: usize,
    search_depth: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks_per_source: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_raw_content: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    include_domains: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclude_domains: &'a [String],
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
    raw_content: Option<String>,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn slug(&self) -> &str {
        "tavily"
    }

    fn supports_full_content(&self) -> bool {
        true
    }

    // Tavily's `content` field is a single NLP summary (basic) or a concatenation
    // of `<chunk> [...] <chunk>` (advanced) with only one page-level `score`, not
    // a per-chunk score - doesn't fit the discrete {chunk,score}[] shape we need.
    // full_content (via raw_content) is the clean fit; extra_snippets stays staan-only.

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        // Tavily only emits "markdown" or "text" - "html" isn't offered, fall back
        // to markdown and let the stamped `format` field tell the caller what it got.
        let raw_format = q.full_content.map(|fmt| match fmt {
            "text" => "text",
            _ => "markdown",
        });
        // "advanced" costs 2 credits vs 1 for "basic" (see tavily_search.md) - only
        // pay for it when the caller actually asked for enriched content.
        let search_depth = if raw_format.is_some() {
            "advanced"
        } else {
            "basic"
        };
        let body = TavilyRequest {
            api_key: q.api_key,
            query: q.query,
            max_results: q.n,
            search_depth,
            chunks_per_source: raw_format.is_some().then_some(3),
            include_raw_content: raw_format,
            include_domains: q.include_domains,
            exclude_domains: q.exclude_domains,
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
            .map(|(i, r)| {
                let full_content = r.raw_content.filter(|t| !t.is_empty()).map(|text| {
                    let length = text.len();
                    FullContent {
                        text,
                        format: raw_format.unwrap_or("").to_string(),
                        length,
                    }
                });
                SearchResult {
                    domain: extract_domain(&r.url),
                    url: r.url,
                    title: r.title,
                    snippet: r.content.unwrap_or_default(),
                    rank: i,
                    published_date: r.published_date,
                    language: None,
                    full_content,
                    extra_snippets: None,
                }
            })
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "tavily", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
