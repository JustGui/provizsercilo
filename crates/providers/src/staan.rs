use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
};

pub struct StaanProvider {
    client: reqwest::Client,
}

impl Default for StaanProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
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

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let mut req = self
            .client
            .get("https://api.staan.ai/v2/search/web")
            .header("Authorization", format!("Bearer {api_key}"))
            .query(&[("q", query), ("count", &n.to_string())]);

        if let Some(market) = resolve_market(language, country) {
            req = req.query(&[("market", market)]);
        }

        let resp = req.send().await?;
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
