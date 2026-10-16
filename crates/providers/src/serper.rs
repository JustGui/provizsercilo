use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    build_client, extract_domain, sanitize_results, ProviderError, SearchOutput, SearchProvider,
    SearchQuery,
};

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
    /// Google date filter: `qdr:d|w|m|y` or `cdr:1,cd_min:M/D/YYYY,cd_max:M/D/YYYY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    tbs: Option<String>,
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
        let body = SerperRequest {
            q: query,
            num: n,
            gl: country,
            hl: language,
            tbs: freshness.and_then(google_tbs),
        };

        let resp = self
            .client
            .post("https://google.serper.dev/search")
            .header("X-API-KEY", api_key)
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
                full_content: None,
                extra_snippets: None,
            })
            .collect();
        let results = sanitize_results(results);

        debug!(provider = "serper", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}

/// Google `tbs` date filter for a freshness request.
fn google_tbs(f: &crate::operators::Freshness) -> Option<String> {
    if f.from.is_none() && f.to.is_none() {
        return f.recency.map(|r| format!("qdr:{}", &r.as_str()[..1]));
    }
    let (from, to) = f.range(crate::operators::today());
    let us = |d: chrono::NaiveDate| d.format("%-m/%-d/%Y").to_string();
    let mut out = "cdr:1".to_string();
    if let Some(d) = from {
        out.push_str(&format!(",cd_min:{}", us(d)));
    }
    if let Some(d) = to {
        out.push_str(&format!(",cd_max:{}", us(d)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operators::Freshness;

    #[test]
    fn freshness_maps_to_google_tbs() {
        assert_eq!(
            google_tbs(&Freshness::from_request(Some("month"), None, None)).as_deref(),
            Some("qdr:m")
        );
        assert_eq!(
            google_tbs(&Freshness::from_request(
                None,
                Some("1962-01-01"),
                Some("1963-12-31")
            ))
            .as_deref(),
            Some("cdr:1,cd_min:1/1/1962,cd_max:12/31/1963")
        );
    }
}
