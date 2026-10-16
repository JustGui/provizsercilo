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
            freshness,
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
        if let Some(f) = freshness.and_then(brave_freshness) {
            req = req.query(&[("freshness", f.as_str())]);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();

        if !resp.status().is_success() {
            return Err(ProviderError::Http {
                status,
                message: resp.text().await.unwrap_or_default(),
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

/// Brave `freshness`: `pd`/`pw`/`pm`/`py`, or `YYYY-MM-DDtoYYYY-MM-DD` for a date range.
fn brave_freshness(f: &crate::operators::Freshness) -> Option<String> {
    use crate::operators::Recency;
    if f.from.is_none() && f.to.is_none() {
        return f.recency.map(|r| {
            match r {
                Recency::Day => "pd",
                Recency::Week => "pw",
                Recency::Month => "pm",
                Recency::Year => "py",
            }
            .to_string()
        });
    }
    let today = crate::operators::today();
    let (from, to) = f.range(today);
    let from = from.unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(1990, 1, 1).unwrap());
    Some(format!("{}to{}", from, to.unwrap_or(today)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operators::Freshness;

    #[test]
    fn freshness_maps_to_brave_codes_and_ranges() {
        assert_eq!(
            brave_freshness(&Freshness::from_request(Some("week"), None, None)).as_deref(),
            Some("pw")
        );
        assert_eq!(
            brave_freshness(&Freshness::from_request(
                None,
                Some("1962-01-01"),
                Some("1963-12-31")
            ))
            .as_deref(),
            Some("1962-01-01to1963-12-31")
        );
        assert_eq!(brave_freshness(&Freshness::default()), None);
    }
}
