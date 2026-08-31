use async_trait::async_trait;
use proviz_core::models::{FullContent, SearchResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ContentDoc, ContentOutput, ContentProvider,
    ContentQuery, ProviderError, SearchOutput, SearchProvider, SearchQuery,
};

/// You.com Web Search API (`/v1/search`) + Contents API (`/v1/contents`).
/// Both authenticate with the `X-API-Key` header and share one key.
pub struct YouComProvider {
    client: reqwest::Client,
}

impl Default for YouComProvider {
    fn default() -> Self {
        Self {
            // Page crawls (full_page extraction / /contents) budget up to
            // crawl_timeout=60s upstream; give the socket room beyond that.
            client: build_client(30),
        }
    }
}

const SEARCH_URL: &str = "https://ydc-index.io/v1/search";
const CONTENTS_URL: &str = "https://ydc-index.io/v1/contents";

/// You.com's `/v1/contents` caps a single call at 10 URLs.
const YOU_COM_MAX_BATCH: usize = 10;

// --- search request/response -------------------------------------------------

#[derive(Serialize)]
struct SearchRequest<'a> {
    query: &'a str,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    include_domains: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclude_domains: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    extraction: Option<Extraction>,
}

#[derive(Serialize)]
struct Extraction {
    extraction_mode: &'static str,
    full_page: FullPage,
}

#[derive(Serialize)]
struct FullPage {
    extraction_formats: Vec<&'static str>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: SearchResults,
}

#[derive(Deserialize, Default)]
struct SearchResults {
    #[serde(default)]
    web: Vec<WebResult>,
}

#[derive(Deserialize)]
struct WebResult {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    snippets: Vec<String>,
    page_age: Option<String>,
    contents: Option<PageContents>,
}

#[derive(Deserialize)]
struct PageContents {
    markdown: Option<String>,
    html: Option<String>,
}

/// Which concrete format You.com's `extraction_formats` should ask for, and the
/// label to stamp on the returned `FullContent`. You.com offers markdown/html
/// only — "text" degrades to markdown.
fn resolve_format(hint: &str) -> (&'static str, &'static str) {
    match hint {
        "html" => ("html", "html"),
        _ => ("markdown", "markdown"),
    }
}

fn parse_search_response(
    body: &str,
    n: usize,
    full_content_fmt: Option<&str>,
) -> Result<Vec<SearchResult>, ProviderError> {
    let resp: SearchResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;

    let results: Vec<SearchResult> = resp
        .results
        .web
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let snippet = if r.description.is_empty() {
                r.snippets.join(" … ")
            } else {
                r.description
            };
            let full_content = full_content_fmt.and_then(|hint| {
                let (_, label) = resolve_format(hint);
                let text = r.contents.and_then(|c| match label {
                    "html" => c.html,
                    _ => c.markdown,
                });
                text.filter(|t| !t.is_empty()).map(|text| {
                    let length = text.len();
                    FullContent {
                        text,
                        format: label.to_string(),
                        length,
                    }
                })
            });
            SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title,
                snippet,
                rank: i,
                published_date: r.page_age,
                language: None,
                full_content,
                extra_snippets: None,
            }
        })
        .collect();

    let mut results = sanitize_results(results);
    if n > 0 && results.len() > n {
        results.truncate(n);
    }
    Ok(results)
}

// --- contents request/response ---------------------------------------------

#[derive(Serialize)]
struct ContentsRequest<'a> {
    urls: &'a [String],
    formats: Vec<&'static str>,
}

#[derive(Deserialize)]
struct ContentsItem {
    url: String,
    title: Option<String>,
    markdown: Option<String>,
    html: Option<String>,
}

fn parse_contents_response(
    body: &str,
    requested: &[String],
    label: &str,
) -> Result<ContentOutput, ProviderError> {
    let items: Vec<ContentsItem> =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;

    let mut docs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let text = match label {
            "html" => item.html,
            _ => item.markdown,
        }
        .filter(|t| !t.is_empty());
        let Some(text) = text else { continue };
        seen.insert(item.url.clone());
        let length = text.len();
        docs.push(ContentDoc {
            url: item.url,
            title: item.title,
            published_date: None,
            content: FullContent {
                text,
                format: label.to_string(),
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
impl SearchProvider for YouComProvider {
    fn slug(&self) -> &str {
        "you-com"
    }

    fn supports_full_content(&self) -> bool {
        true
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let extraction = q.full_content.map(|hint| {
            let (fmt, _) = resolve_format(hint);
            Extraction {
                extraction_mode: "full_page",
                full_page: FullPage {
                    extraction_formats: vec![fmt],
                },
            }
        });

        let body = SearchRequest {
            query: q.query,
            count: q.n.clamp(1, 100),
            country: q.country,
            language: q.language,
            include_domains: q.include_domains,
            exclude_domains: q.exclude_domains,
            extraction,
        };

        let resp = self
            .client
            .post(SEARCH_URL)
            .header("X-API-Key", q.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if let Some(mut err) = map_status(status) {
            if let ProviderError::Http { message, .. } = &mut err {
                *message = resp.text().await.unwrap_or_default();
            }
            debug!(provider = "you-com", status, "search error");
            return Err(err);
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        let results = parse_search_response(&text, q.n, q.full_content)?;

        debug!(provider = "you-com", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}

#[async_trait]
impl ContentProvider for YouComProvider {
    fn slug(&self) -> &str {
        "you-com"
    }

    fn max_batch(&self) -> usize {
        YOU_COM_MAX_BATCH
    }

    async fn fetch(&self, q: ContentQuery<'_>) -> Result<ContentOutput, ProviderError> {
        let (fmt, label) = resolve_format(q.format);
        let body = ContentsRequest {
            urls: q.urls,
            formats: vec![fmt],
        };

        let resp = self
            .client
            .post(CONTENTS_URL)
            .header("X-API-Key", q.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if let Some(mut err) = map_status(status) {
            if let ProviderError::Http { message, .. } = &mut err {
                *message = resp.text().await.unwrap_or_default();
            }
            debug!(provider = "you-com", status, "contents error");
            return Err(err);
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        parse_contents_response(&text, q.urls, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_results_and_full_content() {
        let body = r##"{
            "results": { "web": [
                { "url": "https://a.test/1", "title": "A", "description": "desc a",
                  "snippets": ["s1","s2"], "page_age": "2024-01-01",
                  "contents": { "markdown": "# body a" } },
                { "url": "/relative", "title": "bad", "description": "x" }
            ]}
        }"##;
        let results = parse_search_response(body, 10, Some("markdown")).unwrap();
        assert_eq!(results.len(), 1, "relative URL dropped by sanitize");
        assert_eq!(results[0].url, "https://a.test/1");
        assert_eq!(results[0].snippet, "desc a");
        let fc = results[0].full_content.as_ref().unwrap();
        assert_eq!(fc.format, "markdown");
        assert_eq!(fc.text, "# body a");
    }

    #[test]
    fn search_snippet_falls_back_to_snippets_array() {
        let body = r#"{"results":{"web":[
            {"url":"https://a.test/1","title":"A","description":"","snippets":["one","two"]}
        ]}}"#;
        let results = parse_search_response(body, 10, None).unwrap();
        assert_eq!(results[0].snippet, "one … two");
        assert!(results[0].full_content.is_none());
    }

    #[test]
    fn contents_marks_missing_urls_failed() {
        let requested = vec![
            "https://a.test/1".to_string(),
            "https://b.test/2".to_string(),
        ];
        let body = r##"[
            { "url": "https://a.test/1", "title": "A", "markdown": "# body" }
        ]"##;
        let out = parse_contents_response(body, &requested, "markdown").unwrap();
        assert_eq!(out.docs.len(), 1);
        assert_eq!(out.docs[0].content.format, "markdown");
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].0, "https://b.test/2");
    }
}
