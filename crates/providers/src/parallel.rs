use async_trait::async_trait;
use proviz_core::models::{FullContent, SearchResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ContentDoc, ContentOutput, ContentProvider,
    ContentQuery, ProviderError, SearchOutput, SearchProvider, SearchQuery,
};

/// Parallel's Search API (`/v1/search`) + Extract API (`/v1/extract`).
/// Both authenticate with the `x-api-key` header and share one key.
pub struct ParallelProvider {
    client: reqwest::Client,
}

impl Default for ParallelProvider {
    fn default() -> Self {
        Self {
            client: build_client(30),
        }
    }
}

const SEARCH_URL: &str = "https://api.parallel.ai/v1/search";
const EXTRACT_URL: &str = "https://api.parallel.ai/v1/extract";

/// No documented hard cap on `/v1/extract`; stay conservative and chunk.
const PARALLEL_MAX_BATCH: usize = 10;

// --- search ----------------------------------------------------------------

#[derive(Serialize)]
struct SearchRequest<'a> {
    objective: &'a str,
    search_queries: [&'a str; 1],
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    url: String,
    #[serde(default)]
    title: String,
    publish_date: Option<String>,
    #[serde(default)]
    excerpts: Vec<String>,
}

fn parse_search_response(body: &str, n: usize) -> Result<Vec<SearchResult>, ProviderError> {
    let resp: SearchResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;

    let results: Vec<SearchResult> = resp
        .results
        .into_iter()
        .enumerate()
        .map(|(i, r)| SearchResult {
            domain: extract_domain(&r.url),
            url: r.url,
            title: r.title,
            snippet: r.excerpts.join(" … "),
            rank: i,
            published_date: r.publish_date,
            language: None,
            full_content: None,
            extra_snippets: None,
        })
        .collect();

    let mut results = sanitize_results(results);
    if n > 0 && results.len() > n {
        results.truncate(n);
    }
    Ok(results)
}

// --- extract --------------------------------------------------------------

#[derive(Serialize)]
struct ExtractRequest<'a> {
    urls: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    objective: Option<&'a str>,
    advanced_settings: AdvancedSettings,
}

/// `full_content` is off by default and lives under `advanced_settings` — set it
/// to `true` to get the whole page body as markdown (not just excerpts).
#[derive(Serialize)]
struct AdvancedSettings {
    full_content: bool,
}

#[derive(Deserialize)]
struct ExtractResponse {
    #[serde(default)]
    results: Vec<ExtractItem>,
}

#[derive(Deserialize)]
struct ExtractItem {
    url: String,
    title: Option<String>,
    publish_date: Option<String>,
    #[serde(default)]
    excerpts: Vec<String>,
    full_content: Option<String>,
}

fn parse_extract_response(
    body: &str,
    requested: &[String],
) -> Result<ContentOutput, ProviderError> {
    let resp: ExtractResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;

    let mut docs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in resp.results {
        let text = item
            .full_content
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| item.excerpts.join("\n\n"));
        if text.is_empty() {
            continue;
        }
        seen.insert(item.url.clone());
        let length = text.len();
        docs.push(ContentDoc {
            url: item.url,
            title: item.title,
            published_date: item.publish_date,
            content: FullContent {
                text,
                // Parallel only emits markdown.
                format: "markdown".to_string(),
                length,
            },
        });
    }

    let failed = requested
        .iter()
        .filter(|u| !seen.contains(*u))
        .map(|u| (u.clone(), "no content returned".to_string()))
        .collect();

    Ok(ContentOutput { docs, failed })
}

fn map_status(status: u16) -> Option<ProviderError> {
    match status {
        429 => Some(ProviderError::RateLimit),
        401 | 403 => Some(ProviderError::Blocked),
        s if !(200..300).contains(&s) => Some(ProviderError::Http {
            status: s,
            message: String::new(),
        }),
        _ => None,
    }
}

#[async_trait]
impl SearchProvider for ParallelProvider {
    fn slug(&self) -> &str {
        "parallel"
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let body = SearchRequest {
            objective: q.query,
            search_queries: [q.query],
        };

        let resp = self
            .client
            .post(SEARCH_URL)
            .header("x-api-key", q.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if let Some(mut err) = map_status(status) {
            if let ProviderError::Http { message, .. } = &mut err {
                *message = resp.text().await.unwrap_or_default();
            }
            debug!(provider = "parallel", status, "search error");
            return Err(err);
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        let results = parse_search_response(&text, q.n)?;

        debug!(provider = "parallel", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}

#[async_trait]
impl ContentProvider for ParallelProvider {
    fn slug(&self) -> &str {
        "parallel"
    }

    fn max_batch(&self) -> usize {
        PARALLEL_MAX_BATCH
    }

    async fn fetch(&self, q: ContentQuery<'_>) -> Result<ContentOutput, ProviderError> {
        let body = ExtractRequest {
            urls: q.urls,
            objective: q.objective,
            advanced_settings: AdvancedSettings { full_content: true },
        };

        let resp = self
            .client
            .post(EXTRACT_URL)
            .header("x-api-key", q.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if let Some(mut err) = map_status(status) {
            if let ProviderError::Http { message, .. } = &mut err {
                *message = resp.text().await.unwrap_or_default();
            }
            debug!(provider = "parallel", status, "extract error");
            return Err(err);
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        parse_extract_response(&text, q.urls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_results() {
        let body = r#"{"results":[
            {"url":"https://a.test/1","title":"A","publish_date":"2024-02-01","excerpts":["e1","e2"]},
            {"url":"ftp://nope","title":"bad","excerpts":[]}
        ]}"#;
        let results = parse_search_response(body, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "e1 … e2");
        assert_eq!(results[0].published_date.as_deref(), Some("2024-02-01"));
    }

    #[test]
    fn extract_prefers_full_content_and_flags_missing() {
        let requested = vec![
            "https://a.test/1".to_string(),
            "https://b.test/2".to_string(),
        ];
        let body = r##"{"results":[
            {"url":"https://a.test/1","title":"A","publish_date":null,
             "excerpts":["snip"],"full_content":"# full body"}
        ]}"##;
        let out = parse_extract_response(body, &requested).unwrap();
        assert_eq!(out.docs.len(), 1);
        assert_eq!(out.docs[0].content.text, "# full body");
        assert_eq!(out.docs[0].content.format, "markdown");
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].0, "https://b.test/2");
    }

    #[test]
    fn extract_falls_back_to_excerpts() {
        let requested = vec!["https://a.test/1".to_string()];
        let body = r#"{"results":[
            {"url":"https://a.test/1","excerpts":["part one","part two"],"full_content":null}
        ]}"#;
        let out = parse_extract_response(body, &requested).unwrap();
        assert_eq!(out.docs[0].content.text, "part one\n\npart two");
    }
}
