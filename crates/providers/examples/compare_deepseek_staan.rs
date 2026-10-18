//! Run with exported keys (never prints credentials):
//! cargo run -p providers --example compare_deepseek_staan -- "Bardella est accusé d'antisémitisme"
use providers::{ContentProvider, ContentQuery, SearchProvider, SearchQuery};
use serde_json::{json, Value};
use std::time::Instant;

#[tokio::main]
async fn main() {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Bardella est accusé d'antisémitisme".into());
    let enriched = std::env::args().any(|a| a == "--full-content");
    let args: Vec<String> = std::env::args().collect();
    let localization: Option<std::net::IpAddr> = args
        .windows(2)
        .find(|pair| pair[0] == "--localization")
        .map(|pair| {
            pair[1]
                .parse()
                .expect("--localization requires a valid IP address")
        });
    let mut country_ips = providers::deepseek::default_country_localizations();
    country_ips.extend(
        providers::deepseek::parse_country_localizations(
            &std::env::var("DEEPSEEK_LOCALIZATION_IPS").unwrap_or_else(|_| "{}".into()),
        )
        .expect("valid DEEPSEEK_LOCALIZATION_IPS"),
    );
    let country_localization = country_ips.get("fr").copied();
    let deepseek =
        providers::deepseek::DeepSeekProvider::default().with_country_localizations(country_ips);
    let deepseek = if let Some(ip) = localization {
        deepseek.with_localization(ip)
    } else {
        deepseek
    };
    let staan = providers::staan::StaanProvider::default();
    let mut report = json!({"query":query,"language":"fr","country":"fr","n":10,"full_content":enriched,"deepseek_localization":localization.or(country_localization),"runs":[]});
    for (slug, key_names, provider) in [
        (
            "deepseek",
            vec!["DEEPSEEK_API_KEY", "DEEPSEEK_KEY_1"],
            &deepseek as &dyn SearchProvider,
        ),
        ("staan", vec!["STAAN_KEY_1"], &staan as &dyn SearchProvider),
    ] {
        let key = key_names
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|v| !v.trim().is_empty());
        let Some(key) = key else {
            report["runs"]
                .as_array_mut()
                .unwrap()
                .push(json!({"provider":slug,"status":"missing_key"}));
            continue;
        };
        // Plain SERPs by default; --full-content compares enrichment separately.
        let started = Instant::now();
        let out = provider
            .search(SearchQuery {
                query: &query,
                n: 10,
                language: Some("fr"),
                country: Some("fr"),
                api_key: &key,
                extra_snippets: false,
                full_content: enriched.then_some("markdown"),
                max_snippets: None,
                min_score: None,
                include_domains: &[],
                exclude_domains: &[],
                freshness: None,
            })
            .await;
        let ms = started.elapsed().as_millis();
        match out {
            Ok(out) => {
                if slug == "deepseek" {
                    // Exercise Fetch on one result through the same adapter.
                    if let Some(result) = out.results.first() {
                        let urls = [result.url.clone()];
                        let started = Instant::now();
                        let fetched = deepseek.fetch(ContentQuery { urls:&urls, format:"markdown", objective:None, api_key:&key }).await;
                        report["fetch"] = match fetched {
                            Ok(f) => json!({"url":urls[0],"duration_ms":started.elapsed().as_millis(),
                                "docs": f.docs.iter().map(|d| json!({"title":d.title,"format":d.content.format,"bytes":d.content.length})).collect::<Vec<_>>(),"failed":f.failed}),
                            Err(e) => json!({"url":urls[0],"error_type":e.error_type_str()}),
                        };
                    }
                }
                report["runs"].as_array_mut().unwrap().push(json!({"provider":slug,"duration_ms":ms,"status":"ok","results":out.results}));
            },
            Err(e) => report["runs"].as_array_mut().unwrap().push(json!({"provider":slug,"duration_ms":ms,"status":"error","error_type":e.error_type_str()})),
        }
    }
    let successful: Vec<&Value> = report["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["status"] == "ok")
        .collect();
    if successful.len() == 2 {
        let a = successful[0]["results"].as_array().unwrap();
        let b = successful[1]["results"].as_array().unwrap();
        let fragment_free_overlap = a
            .iter()
            .filter(|r| {
                b.iter().any(|s| {
                    let strip = |v: &Value| {
                        v["url"]
                            .as_str()
                            .unwrap_or("")
                            .split('#')
                            .next()
                            .unwrap_or("")
                            .to_owned()
                    };
                    strip(s) == strip(r)
                })
            })
            .count();
        let exact_overlap = a
            .iter()
            .filter(|r| b.iter().any(|s| s["url"] == r["url"]))
            .count();
        report["fragment_free_url_overlap"] = json!(fragment_free_overlap);
        report["exact_url_overlap"] = json!(exact_overlap);
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
