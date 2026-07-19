use async_trait::async_trait;
use proviz_core::models::{ExtraSnippet, FullContent, SearchResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
};

pub struct StaanProvider {
    client: reqwest::Client,
}

impl Default for StaanProvider {
    fn default() -> Self {
        Self {
            client: build_client(20), // enrichment (extra_snippets/full_content) budgets 8-10s per staan docs
        }
    }
}

/// Mirrors staan_2.md "Web Search for AI" — same endpoint as base web search
/// (staan_1.md), enrichment activated purely by the extra optional fields below.
/// POST (not GET) because `include_domains`/`exclude_domains` require it.
#[derive(Serialize)]
struct StaanRequest<'a> {
    q: &'a str,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    market: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_snippets: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_snippets: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_score: Option<f64>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    include_domains: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclude_domains: &'a [String],
}

#[derive(Deserialize)]
struct StaanResponse {
    web: StaanWeb,
}

#[derive(Deserialize)]
struct StaanWeb {
    results: Vec<StaanResult>,
}

#[derive(Deserialize)]
struct StaanResult {
    title: String,
    url: String,
    snippet: String,
    published_date: Option<String>,
    #[serde(default)]
    extra_snippets: Vec<StaanExtraSnippet>,
    full_content: Option<StaanFullContent>,
}

#[derive(Deserialize)]
struct StaanExtraSnippet {
    chunk: String,
    score: f64,
}

#[derive(Deserialize)]
struct StaanFullContent {
    text: String,
    format: String,
    length: usize,
}

/// Map language/country hints to one of Staan's three supported markets
/// (`fr-fr`, `en-us`, `de-de`). Falls back to the API default (`fr-fr`) via `None`.
fn resolve_market(language: Option<&str>, country: Option<&str>) -> Option<&'static str> {
    let lang = language.map(|l| l.to_lowercase());
    let cty = country.map(|c| c.to_lowercase());
    match (lang.as_deref(), cty.as_deref()) {
        (Some("en"), _) | (_, Some("us")) | (_, Some("gb")) => Some("en-us"),
        (Some("de"), _) | (_, Some("de")) => Some("de-de"),
        (Some("fr"), _) | (_, Some("fr")) => Some("fr-fr"),
        _ => None,
    }
}

#[async_trait]
impl SearchProvider for StaanProvider {
    fn slug(&self) -> &str {
        "staan"
    }

    fn supports_full_content(&self) -> bool {
        true
    }

    fn supports_extra_snippets(&self) -> bool {
        true
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let market = resolve_market(q.language, q.country);
        // include_domains/exclude_domains are mutually exclusive per staan_1.md -
        // prefer include (a caller-set allowlist) when both are somehow present.
        let (include_domains, exclude_domains): (&[String], &[String]) =
            if !q.include_domains.is_empty() {
                (q.include_domains, &[])
            } else {
                (&[], q.exclude_domains)
            };

        let body = StaanRequest {
            q: q.query,
            count: q.n,
            market,
            extra_snippets: q.extra_snippets.then_some(true),
            full_content: q.full_content,
            max_snippets: q.extra_snippets.then_some(q.max_snippets).flatten(),
            min_score: q.extra_snippets.then_some(q.min_score).flatten(),
            include_domains,
            exclude_domains,
        };

        let resp = self
            .client
            .post("https://api.staan.ai/v2/search/web")
            .header("Authorization", format!("Bearer {}", q.api_key))
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();

        if status == 429 {
            debug!(provider = "staan", status, "rate-limited");
            return Err(ProviderError::RateLimit);
        }
        if status == 401 || status == 403 {
            debug!(provider = "staan", status, "auth/blocked");
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            debug!(provider = "staan", status, body = %msg, "http error");
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: StaanResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .web
            .results
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title,
                snippet: r.snippet,
                rank: i,
                published_date: r.published_date,
                language: None,
                full_content: r.full_content.and_then(|fc| {
                    // length==0 means staan couldn't fetch the page - don't hand the
                    // caller a dead field, let it fall back to the content-fetcher path.
                    (fc.length > 0).then_some(FullContent {
                        text: fc.text,
                        format: fc.format,
                        length: fc.length,
                    })
                }),
                extra_snippets: (!r.extra_snippets.is_empty()).then(|| {
                    r.extra_snippets
                        .into_iter()
                        .map(|s| ExtraSnippet {
                            chunk: s.chunk,
                            score: s.score,
                        })
                        .collect()
                }),
            })
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "staan", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
