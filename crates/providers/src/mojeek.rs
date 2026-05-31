use async_trait::async_trait;
use proviz_core::models::SearchResult;
use serde::Deserialize;
use tracing::debug;

use crate::{build_client, extract_domain, ProviderError, SearchOutput, SearchProvider};

pub struct MojeekProvider {
    client: reqwest::Client,
}

impl Default for MojeekProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

#[derive(Deserialize)]
struct MojeekResponse {
    results: Option<Vec<MojeekResult>>,
}

#[derive(Deserialize)]
struct MojeekResult {
    url: String,
    title: Option<String>,
    #[serde(alias = "desc")]
    description: Option<String>,
    date: Option<String>,
    #[serde(alias = "lan")]
    language: Option<String>,
}

#[async_trait]
impl SearchProvider for MojeekProvider {
    fn slug(&self) -> &str {
        "mojeek"
    }

    async fn search(
        &self,
        query: &str,
        n: usize,
        language: Option<&str>,
        country: Option<&str>,
        api_key: &str,
    ) -> Result<SearchOutput, ProviderError> {
        let mut req = self.client.get("https://www.mojeek.com/search").query(&[
            ("q", query),
            ("api_key", api_key),
            ("fmt", "json"),
            ("t", &n.to_string()),
        ]);

        if let Some(lang) = language {
            req = req.query(&[("lang", lang)]);
        }
        if let Some(cty) = country {
            req = req.query(&[("country", cty)]);
        }

        let resp = req.send().await?;
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

        let body: MojeekResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        let results: Vec<SearchResult> = body
            .results
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearchResult {
                domain: extract_domain(&r.url),
                url: r.url,
                title: r.title.unwrap_or_default(),
                snippet: r.description.unwrap_or_default(),
                rank: i,
                published_date: r.date,
                language: r.language,
            })
            .collect();

        debug!(provider = "mojeek", n = results.len(), "search complete");
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}
