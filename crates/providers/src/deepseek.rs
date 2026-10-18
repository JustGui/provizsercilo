use async_trait::async_trait;
use proviz_core::models::{FullContent, SearchResult};
use serde::{Deserialize, Serialize};

use crate::{
    build_client, extract_domain, sanitize_results, ContentDoc, ContentOutput, ContentProvider,
    ContentQuery, ProviderError, SearchOutput, SearchProvider, SearchQuery,
};

/// DeepSeek's standalone Search & Fetch APIs (no model invocation).
pub struct DeepSeekProvider {
    client: reqwest::Client,
    base_url: String,
    localization: Option<std::net::IpAddr>,
    country_localizations: std::collections::HashMap<String, std::net::IpAddr>,
    search_next_request: tokio::sync::Mutex<tokio::time::Instant>,
    fetch_next_request: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Default for DeepSeekProvider {
    fn default() -> Self {
        Self {
            client: build_client(30),
            base_url: "https://api.deepseek.com".into(),
            localization: None,
            country_localizations: default_country_localizations(),
            search_next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            fetch_next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }
}

#[derive(Serialize)]
struct SearchRequest {
    query: String,
    count: usize,
    need_content: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    localization: Option<std::net::IpAddr>,
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    title: String,
    url: String,
    snippet: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    content: String,
    date: Option<String>,
}

#[derive(Deserialize)]
struct FetchResponse {
    title: String,
    content: String,
}

fn request_error(message: &str) -> ProviderError {
    ProviderError::Http {
        status: 400,
        message: format!("deepseek: invalid request: {message}"),
    }
}

fn search_request(q: &SearchQuery<'_>) -> Result<SearchRequest, ProviderError> {
    // Site restrictions have already been added by the executor. Preserve user
    // operators; append only hints that have no explicit counterpart in the query.
    if q.query.trim().is_empty() {
        return Err(request_error("query must not be blank"));
    }
    let mut query = q.query.to_string();
    if let Some(language) = q.language.filter(|_| !q.query.contains("lang:")) {
        let language = language.to_ascii_lowercase();
        if language.split(',').all(|code| {
            (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphabetic())
        }) {
            query.push_str(&format!(" lang:{language}"));
        }
    }
    if let Some(freshness) = q.freshness {
        let (from, to) = freshness.range(chrono::Utc::now().date_naive());
        if let Some(from) = from.filter(|_| !q.query.contains("after:")) {
            query.push_str(&format!(" after:{from}"));
        }
        if let Some(to) = to.filter(|_| !q.query.contains("before:")) {
            query.push_str(&format!(" before:{to}"));
        }
    }
    if query.trim().is_empty() || query.chars().count() > 128 {
        return Err(request_error(
            "query must contain 1 to 128 Unicode characters including operators",
        ));
    }
    Ok(SearchRequest {
        query,
        count: q.n.clamp(1, 50),
        need_content: q.full_content.is_some(),
        localization: None,
    })
}

fn markdown(text: String) -> Option<FullContent> {
    (!text.trim().is_empty()).then(|| FullContent {
        length: text.len(),
        text,
        format: "markdown".into(),
    })
}

/// Small offline snapshot generated from sapics/ip-location-db; provenance and
/// checksum are embedded in the JSON. No geolocation service is called at runtime.
pub fn default_country_localizations() -> std::collections::HashMap<String, std::net::IpAddr> {
    #[derive(Deserialize)]
    struct Snapshot {
        country_ips: std::collections::HashMap<String, std::net::IpAddr>,
    }
    let snapshot: Snapshot = serde_json::from_str(include_str!("../data/country_ips.json"))
        .expect("bundled country IP snapshot must be valid");
    snapshot.country_ips
}

/// Operator-maintained country-to-IP hints; no IP-geolocation lookup is needed
/// at request time. Reject malformed config rather than silently dropping it.
pub fn parse_country_localizations(
    raw: &str,
) -> Result<std::collections::HashMap<String, std::net::IpAddr>, String> {
    let entries: std::collections::HashMap<String, std::net::IpAddr> =
        serde_json::from_str(raw).map_err(|e| format!("invalid DEEPSEEK_LOCALIZATION_IPS: {e}"))?;
    let mut normalized = std::collections::HashMap::new();
    for (country, ip) in entries {
        let country = country.trim().to_ascii_lowercase();
        if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err("DEEPSEEK_LOCALIZATION_IPS keys must be two-letter country codes".into());
        }
        if normalized.insert(country, ip).is_some() {
            return Err(
                "DEEPSEEK_LOCALIZATION_IPS contains duplicate normalized country codes".into(),
            );
        }
    }
    Ok(normalized)
}

impl DeepSeekProvider {
    /// Localize searches using an explicit IP hint accepted by DeepSeek.
    pub fn with_localization(mut self, ip: std::net::IpAddr) -> Self {
        self.localization = Some(ip);
        self
    }

    pub fn with_country_localizations(
        mut self,
        countries: std::collections::HashMap<String, std::net::IpAddr>,
    ) -> Self {
        self.country_localizations.extend(countries);
        self
    }

    fn localized_search_request(
        &self,
        q: &SearchQuery<'_>,
    ) -> Result<SearchRequest, ProviderError> {
        let mut body = search_request(q)?;
        body.localization = self.localization.or_else(|| {
            q.country.and_then(|country| {
                self.country_localizations
                    .get(&country.trim().to_ascii_lowercase())
                    .copied()
            })
        });
        Ok(body)
    }

    async fn wait_for_slot(&self, fetch: bool) {
        // Independent account-wide quotas for Search and Fetch. Each pool's
        // adapter is shared across keys, including individual Fetch batch chunks.
        let gate = if fetch {
            &self.fetch_next_request
        } else {
            &self.search_next_request
        };
        let mut next = gate.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = tokio::time::Instant::now() + std::time::Duration::from_millis(210);
    }

    async fn post(
        &self,
        path: &str,
        api_key: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, ProviderError> {
        self.wait_for_slot(path == "/v1/tools/fetch").await;
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }
        Ok(response)
    }
}

#[async_trait]
impl SearchProvider for DeepSeekProvider {
    fn slug(&self) -> &str {
        "deepseek"
    }

    fn supports_full_content(&self) -> bool {
        true
    }

    async fn search(&self, q: SearchQuery<'_>) -> Result<SearchOutput, ProviderError> {
        let body = self.localized_search_request(&q)?;
        let response: SearchResponse = self
            .post("/v1/tools/search", q.api_key, &body)
            .await?
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        let results = sanitize_results(
            response
                .results
                .into_iter()
                .map(|r| SearchResult {
                    domain: extract_domain(&r.url),
                    url: r.url,
                    title: r.title,
                    snippet: if r.snippet.trim().is_empty() {
                        r.summary
                    } else {
                        r.snippet
                    },
                    rank: 0,
                    published_date: r.date,
                    language: None,
                    full_content: if body.need_content {
                        markdown(r.content)
                    } else {
                        None
                    },
                    extra_snippets: None,
                })
                .collect(),
        );
        let results: Vec<_> = results
            .into_iter()
            .take(body.count)
            .enumerate()
            .map(|(rank, mut r)| {
                r.rank = rank;
                r
            })
            .collect();
        if results.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(SearchOutput::new(results))
    }
}

#[async_trait]
impl ContentProvider for DeepSeekProvider {
    fn slug(&self) -> &str {
        "deepseek"
    }

    // Fetch accepts one URL; let ContentExecutor split and account for each call.
    async fn fetch(&self, q: ContentQuery<'_>) -> Result<ContentOutput, ProviderError> {
        if q.urls.len() != 1 {
            return Err(request_error("fetch requires exactly one URL"));
        }
        let url = &q.urls[0];
        let parsed = url::Url::parse(url).map_err(|_| request_error("invalid fetch URL"))?;
        if url.chars().count() > 8192
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(request_error(
                "fetch requires an HTTP(S) URL without credentials",
            ));
        }
        let response = self
            .post(
                "/v1/tools/fetch",
                q.api_key,
                &serde_json::json!({"url": url}),
            )
            .await;
        // A policy denial or unextractable page is a URL-specific miss, not a bad key.
        let response = match response {
            Err(ProviderError::Http {
                status: 403 | 404, ..
            }) => {
                return Ok(ContentOutput {
                    docs: vec![],
                    failed: vec![(
                        url.clone(),
                        "page unavailable or blocked by fetch policy".into(),
                    )],
                })
            }
            other => other?,
        };
        let page: FetchResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        match markdown(page.content) {
            Some(content) => Ok(ContentOutput {
                docs: vec![ContentDoc {
                    url: url.clone(),
                    title: (!page.title.is_empty()).then_some(page.title),
                    published_date: None,
                    content,
                }],
                failed: vec![],
            }),
            None => Ok(ContentOutput {
                docs: vec![],
                failed: vec![(url.clone(), "no content returned".into())],
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn query(text: &str) -> SearchQuery<'_> {
        SearchQuery {
            query: text,
            n: 10,
            language: Some("fr"),
            country: Some("fr"),
            api_key: "test-key",
            extra_snippets: false,
            full_content: None,
            max_snippets: None,
            min_score: None,
            include_domains: &[],
            exclude_domains: &[],
            freshness: None,
        }
    }

    async fn mock(
        status: u16,
        response: &'static str,
    ) -> (DeepSeekProvider, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let length: usize = header
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let reply = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
            stream.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (
            DeepSeekProvider {
                client: build_client(2),
                base_url,
                localization: None,
                country_localizations: std::collections::HashMap::new(),
                search_next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
                fetch_next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            },
            task,
        )
    }

    #[test]
    fn countries_resolve_per_request_without_inferring_country_from_language() {
        let provider = DeepSeekProvider::default().with_country_localizations(
            parse_country_localizations(r#"{"FR":"86.196.134.124","US":"8.8.8.8"}"#).unwrap(),
        );
        let mut q = query("Bardella");
        for (country, expected) in [
            (Some("FR"), Some("86.196.134.124")),
            (Some(" us "), Some("8.8.8.8")),
            (Some("zz"), None),
            (None, None),
        ] {
            q.country = country;
            let body = provider.localized_search_request(&q).unwrap();
            assert_eq!(
                body.localization.map(|ip| ip.to_string()).as_deref(),
                expected
            );
            assert_eq!(body.query, "Bardella lang:fr");
            let json = serde_json::to_value(body).unwrap();
            assert_eq!(json.get("localization").is_some(), expected.is_some());
        }
        let provider = provider.with_localization("1.1.1.1".parse().unwrap());
        q.country = Some("fr");
        assert_eq!(
            provider.localized_search_request(&q).unwrap().localization,
            Some("1.1.1.1".parse().unwrap())
        );
    }

    #[test]
    fn bundled_defaults_survive_partial_overrides_and_empty_config() {
        let defaults = default_country_localizations();
        assert!(defaults.len() >= 200);
        for (country, ip) in &defaults {
            assert_eq!(country.len(), 2);
            assert!(country.bytes().all(|b| b.is_ascii_lowercase()));
            let std::net::IpAddr::V4(ip) = ip else {
                panic!("expected IPv4");
            };
            assert!(
                !ip.is_private()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !ip.is_unspecified()
                    && !ip.is_link_local()
            );
        }
        let provider = DeepSeekProvider::default()
            .with_country_localizations(
                parse_country_localizations(r#"{"fr":"86.196.134.124"}"#).unwrap(),
            )
            .with_country_localizations(parse_country_localizations("{}").unwrap());
        let mut q = query("Bardella");
        assert_eq!(
            provider.localized_search_request(&q).unwrap().localization,
            Some("86.196.134.124".parse().unwrap())
        );
        q.country = Some("GB");
        assert_eq!(
            provider.localized_search_request(&q).unwrap().localization,
            defaults.get("gb").copied()
        );
        let provider = DeepSeekProvider::default();
        q.country = Some("fr");
        assert_eq!(
            provider.localized_search_request(&q).unwrap().localization,
            defaults.get("fr").copied()
        );
    }

    #[test]
    fn localization_config_accepts_ipv6_and_rejects_malformed_maps() {
        assert_eq!(
            parse_country_localizations(r#"{"fr":"2001:db8::1"}"#)
                .unwrap()
                .len(),
            1
        );
        assert!(parse_country_localizations("{}").unwrap().is_empty());
        for raw in [
            "[]",
            "null",
            "not json",
            r#"{"fr":"bad-ip"}"#,
            r#"{"fr-fr":"86.196.134.124"}"#,
            r#"{"FR":"1.1.1.1","fr":"8.8.8.8"}"#,
        ] {
            assert!(parse_country_localizations(raw).is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn search_and_fetch_each_space_calls_without_consuming_the_other_quota() {
        let provider = DeepSeekProvider::default();
        let start = tokio::time::Instant::now();
        let search = async {
            for _ in 0..3 {
                provider.wait_for_slot(false).await;
            }
            tokio::time::Instant::now() - start
        };
        let fetch = async {
            for _ in 0..3 {
                provider.wait_for_slot(true).await;
            }
            tokio::time::Instant::now() - start
        };
        let (search, fetch) = tokio::join!(search, fetch);
        assert_eq!(search, std::time::Duration::from_millis(420));
        assert_eq!(fetch, std::time::Duration::from_millis(420));
    }

    #[test]
    fn validates_unicode_length_and_preserves_explicit_operators() {
        let mut q = query("Bardella lang:fr after:2024");
        q.n = 100;
        let fresh =
            crate::operators::Freshness::from_request(None, Some("2023-01-01"), Some("2025-01-01"));
        q.freshness = Some(&fresh);
        let body = search_request(&q).unwrap();
        assert_eq!(body.query, "Bardella lang:fr after:2024 before:2025-01-01");
        assert_eq!(body.count, 50);
        let text = "é".repeat(128);
        let mut q = query(&text);
        q.language = None;
        assert!(search_request(&q).is_ok());
        q.language = Some("fr");
        assert_eq!(
            search_request(&q).err().unwrap().error_type_str(),
            "request"
        );
        let mut blank = query("  ");
        blank.language = None;
        assert!(search_request(&blank).is_err());
    }

    #[tokio::test]
    async fn search_sends_auth_and_maps_content_without_fabricating_snippet_scores() {
        let (provider, request) = mock(200, r##"{"results":[
            {"url":"ftp://bad.test/a","title":"bad","snippet":"bad"},
            {"url":"https://www.france.test/a","title":"Titre","snippet":"","summary":"Résumé","date":"2024-01-01","content":"# Article"},
            {"url":"https://france.test/b","title":"B","snippet":"B","content":" "}
        ]}"##).await;
        let provider = provider.with_country_localizations(
            parse_country_localizations(r#"{"fr":"86.196.134.124"}"#).unwrap(),
        );
        let mut q = query("Bardella");
        q.full_content = Some("html");
        let out = provider.search(q).await.unwrap();
        assert_eq!(out.results.len(), 2);
        assert_eq!(out.results[0].domain, "france.test");
        assert_eq!(out.results[0].rank, 0);
        assert_eq!(out.results[0].snippet, "Résumé");
        assert_eq!(out.results[0].published_date.as_deref(), Some("2024-01-01"));
        assert_eq!(
            out.results[0].full_content.as_ref().unwrap().format,
            "markdown"
        );
        assert!(out.results[1].full_content.is_none());
        assert!(out.results[0].extra_snippets.is_none());
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /v1/tools/search "));
        assert!(request
            .to_lowercase()
            .contains("authorization: bearer test-key"));
        let body: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"query":"Bardella lang:fr","count":10,"need_content":true,"localization":"86.196.134.124"})
        );
    }

    #[tokio::test]
    async fn fetch_maps_markdown_and_page_misses_without_cooling_the_key() {
        let urls = vec!["https://example.fr/article".into()];
        for (status, body, ok) in [
            (200, r##"{"title":"Titre","content":"# Texte"}"##, true),
            (200, r#"{"title":"","content":" "}"#, false),
            (403, "{}", false),
            (404, "{}", false),
        ] {
            let (provider, request) = mock(status, body).await;
            let out = provider
                .fetch(ContentQuery {
                    urls: &urls,
                    format: "html",
                    objective: None,
                    api_key: "test-key",
                })
                .await
                .unwrap();
            assert_eq!(out.docs.len(), usize::from(ok));
            assert_eq!(out.failed.len(), usize::from(!ok));
            if ok {
                assert_eq!(out.docs[0].content.format, "markdown");
            }
            assert!(request.await.unwrap().starts_with("POST /v1/tools/fetch "));
        }
    }

    #[tokio::test]
    async fn empty_search_and_upstream_failures_remain_distinguishable() {
        for (status, body, error) in [
            (200, r#"{"results":[]}"#, "empty"),
            (401, "{}", "auth"),
            (402, "{}", "billing"),
            (429, "{}", "rpm"),
            (504, "{}", "timeout"),
        ] {
            let (provider, request) = mock(status, body).await;
            assert_eq!(
                provider
                    .search(query("test"))
                    .await
                    .err()
                    .unwrap()
                    .error_type_str(),
                error
            );
            request.await.unwrap();
        }
    }
}
