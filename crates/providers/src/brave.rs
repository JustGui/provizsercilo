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
    endpoint: String,
}

impl Default for BraveProvider {
    fn default() -> Self {
        Self {
            client: build_client(15),
            endpoint: "https://api.search.brave.com/res/v1/web/search".into(),
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
            .get(&self.endpoint)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", api_key)
            .query(&[("q", query), ("count", &n.clamp(1, 20).to_string())]);

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

        let body: BraveResponse = resp.json().await.map_err(|_| {
            ProviderError::Parse(
                "brave: invalid JSON search response (expected web.results)".into(),
            )
        })?;

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

#[cfg(test)]
mod http_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response(body: &'static [u8], encoding: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.contains("count=20"));
            assert!(request
                .to_ascii_lowercase()
                .contains("accept-encoding: gzip"));
            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n", body.len(), encoding);
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        format!("http://{address}/search")
    }

    async fn call(endpoint: String) -> Result<SearchOutput, ProviderError> {
        BraveProvider {
            client: build_client(2),
            endpoint,
        }
        .search(SearchQuery {
            query: "brave search",
            n: 50,
            language: None,
            country: None,
            api_key: "local-test-only",
            extra_snippets: false,
            full_content: None,
            max_snippets: None,
            min_score: None,
            include_domains: &[],
            exclude_domains: &[],
            freshness: None,
        })
        .await
    }

    #[tokio::test]
    async fn documented_brave_shape_decodes_with_and_without_gzip() {
        for (body, encoding) in [
            (
                include_bytes!("../tests/fixtures/brave-web.json").as_slice(),
                "",
            ),
            (
                include_bytes!("../tests/fixtures/brave-web.json.gz").as_slice(),
                "Content-Encoding: gzip\r\n",
            ),
        ] {
            let output = call(response(body, encoding).await).await.unwrap();
            assert_eq!(output.results.len(), 1);
            assert_eq!(output.results[0].url, "https://search.brave.com/");
        }
    }

    #[tokio::test]
    async fn invalid_brave_body_has_a_clear_safe_provider_error() {
        let err = call(response(b"not JSON", "").await).await.err().unwrap();
        assert_eq!(err.error_type_str(), "error");
        assert!(err
            .to_string()
            .contains("brave: invalid JSON search response"));
        assert!(!err.to_string().contains("not JSON"));
    }
}
