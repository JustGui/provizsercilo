use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
};

pub struct BraveProvider {
    client: reqwest::Client,
}

impl Default for BraveProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Option<Vec<BraveResult>>,
}

#[derive(Deserialize)]
struct BraveResult {
    url: String,
    title: String,
    description: Option<String>,
    page_age: Option<String>,
    language: Option<String>,
}

#[async_trait]
impl SearchProvider for BraveProvider {
    fn slug(&self) -> &str {
        "brave"
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let SearchQuery {
            query,
            n,
            language,
            country,
            api_key,
            ..
        } = q;
        let mut req = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", api_key)
            .query(&[("q", query), ("count", &n.to_string())]);

        if let Some(lang) = language {
            req = req.query(&[("search_lang", lang)]);
        }
        if let Some(cty) = country {
            req = req.query(&[("country", cty)]);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();

        if status == 429 {
            debug!(provider = "brave", status, "rate-limited");
            return Err(ProviderError::RateLimit);
        }
        if status == 401 || status == 403 {
            debug!(provider = "brave", status, "auth/blocked");
            return Err(ProviderError::Blocked);
        }
        if !resp.status().is_success() {
            let msg = resp.text().await.unwrap_or_default();
            debug!(provider = "brave", status, body = %msg, "http error");
            return Err(ProviderError::Http {
                status,
                message: msg,
            });
        }

        let body: BraveResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .web
            .and_then(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title,
                snippet: r.description.unwrap_or_default(),
                rank: i,
                published_date: r.page_age,
                language: r.language,
                full_content: None,
                extra_snippets: None,
            })
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "brave", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
