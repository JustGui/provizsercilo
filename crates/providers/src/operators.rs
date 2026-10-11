//! Search operators for engines that ignore the structured `include_domains` /
//! `exclude_domains` / freshness hints (the free tier: SearXNG, the DDG bridge, Brave,
//! Mojeek). See `docs/search-operators.md` for the measured support matrix.
//!
//! - Domain restriction becomes `site:` operators in the query text, OR-ed and capped
//!   (`MAX_SITE_OPERATORS`), and the answer is filtered to the requested domains so an
//!   engine that drops the operator cannot hand back unrestricted results.
//! - A domain entry may carry a path (`lemonde.fr/les-decodeurs`): the fact-checking
//!   section of a general outlet. Engines get `site:lemonde.fr/les-decodeurs`; providers
//!   with a structured `include_domains` field get the bare host; the result filter
//!   checks the path prefix either way.
//! - Freshness is either a relative window (`day`/`week`/`month`/`year`) or a date range.
//!   Engines that only take a window get the smallest window that covers the range.

use chrono::{Duration, NaiveDate};
use proviz_core::models::SearchResult;
use serde::{Deserialize, Serialize};

/// Longest `site:` list put in a query. Measured (2026-10-10): six OR-ed `site:` terms
/// still return restricted results on Google CSE / Yandex; longer lists start to come
/// back empty, and some engines cap the query length.
pub const MAX_SITE_OPERATORS: usize = 6;

/// Longest `-site:` list put in a query.
pub const MAX_EXCLUDE_OPERATORS: usize = 4;

/// Relative recency window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    pub fn parse(s: &str) -> Option<Recency> {
        match s.trim().to_ascii_lowercase().as_str() {
            "day" | "d" | "pd" | "24h" => Some(Recency::Day),
            "week" | "w" | "pw" => Some(Recency::Week),
            "month" | "m" | "pm" => Some(Recency::Month),
            "year" | "y" | "py" => Some(Recency::Year),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    fn days(self) -> i64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 31,
            Recency::Year => 366,
        }
    }
}

/// Requested publication window. Empty = no restriction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freshness {
    pub recency: Option<Recency>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

impl Freshness {
    /// From the request fields; unparseable values are dropped (never a 400 - freshness is
    /// a hint like the other enrichment fields).
    pub fn from_request(
        freshness: Option<&str>,
        date_from: Option<&str>,
        date_to: Option<&str>,
    ) -> Freshness {
        let date = |s: Option<&str>| {
            s.and_then(|d| {
                NaiveDate::parse_from_str(d.trim().get(..10).unwrap_or(d.trim()), "%Y-%m-%d").ok()
            })
        };
        Freshness {
            recency: freshness.and_then(Recency::parse),
            from: date(date_from),
            to: date(date_to),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.recency.is_none() && self.from.is_none() && self.to.is_none()
    }

    /// The explicit date range, with a relative window turned into dates ending today.
    pub fn range(&self, today: NaiveDate) -> (Option<NaiveDate>, Option<NaiveDate>) {
        if self.from.is_some() || self.to.is_some() {
            return (self.from, self.to);
        }
        match self.recency {
            Some(r) => (Some(today - Duration::days(r.days())), Some(today)),
            None => (None, None),
        }
    }

    /// The smallest relative window covering the request, for engines that only take a
    /// window. `None` when the request cannot be expressed as one: a range that ends in
    /// the past (`to` before yesterday) or starts more than a year ago - restricting such an
    /// engine to the last year would drop exactly the documents asked for.
    pub fn as_recency(&self, today: NaiveDate) -> Option<Recency> {
        if let Some(r) = self.recency {
            if self.from.is_none() && self.to.is_none() {
                return Some(r);
            }
        }
        if let Some(to) = self.to {
            if to < today - Duration::days(1) {
                return None;
            }
        }
        let from = self.from?;
        let age = (today - from).num_days();
        [Recency::Day, Recency::Week, Recency::Month, Recency::Year]
            .into_iter()
            .find(|r| age <= r.days())
    }

    /// Cache-key discriminator.
    pub fn cache_key(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        format!(
            "{}|{}|{}",
            self.recency.map(Recency::as_str).unwrap_or(""),
            self.from.map(|d| d.to_string()).unwrap_or_default(),
            self.to.map(|d| d.to_string()).unwrap_or_default()
        )
    }
}

/// Today's date (UTC), the reference for relative windows.
pub fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

/// `"https://www.LeMonde.fr/les-decodeurs/"` / `"*.gouv.fr"` / `"site:insee.fr"` ->
/// `"lemonde.fr/les-decodeurs"` / `"gouv.fr"` / `"insee.fr"`. Empty for garbage.
pub fn normalize_domain_entry(entry: &str) -> String {
    let mut s = entry.trim().to_ascii_lowercase();
    for prefix in ["site:", "https://", "http://", "*.", "www."] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    let s = s.trim_end_matches('/').to_string();
    if s.is_empty()
        || s.contains(char::is_whitespace)
        || !s.split('/').next().unwrap_or("").contains('.')
    {
        return String::new();
    }
    s
}

/// Host part of a normalised entry (`lemonde.fr/les-decodeurs` -> `lemonde.fr`).
pub fn entry_host(entry: &str) -> &str {
    entry.split('/').next().unwrap_or(entry)
}

/// Bare hosts of `entries`, deduplicated in order - for providers whose structured
/// `include_domains` field takes hosts only.
pub fn hosts(entries: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in entries {
        let n = normalize_domain_entry(e);
        let h = entry_host(&n);
        if !h.is_empty() && !out.iter().any(|o| o == h) {
            out.push(h.to_string());
        }
    }
    out
}

/// True when the query already restricts its own sites (the caller wrote `site:`).
pub fn has_site_operator(query: &str) -> bool {
    query.split_whitespace().any(|w| {
        let w = w.trim_start_matches(['(', '-']).to_ascii_lowercase();
        w.starts_with("site:")
    })
}

/// `query` with `site:` operators for `include` (OR-ed, at most `MAX_SITE_OPERATORS`) and
/// `-site:` operators for `exclude` (at most `MAX_EXCLUDE_OPERATORS`). A query that
/// already has a `site:` operator is left as the caller wrote it.
pub fn with_site_operators(query: &str, include: &[String], exclude: &[String]) -> String {
    let query = query.trim();
    if has_site_operator(query) {
        return query.to_string();
    }
    // A path inside an OR-ed list makes engines answer nothing (measured on Yahoo,
    // 2026-10-10: `site:a.fr OR site:lemonde.fr/les-decodeurs` -> 0 results, the same list
    // with hosts only -> 7). A lone entry keeps its path; in a list, the host stands in and
    // the result filter enforces the path.
    let entries: Vec<String> = include
        .iter()
        .map(|d| normalize_domain_entry(d))
        .filter(|d| !d.is_empty())
        .collect();
    let several = entries.len() > 1;
    let mut seen: Vec<String> = Vec::new();
    let inc: Vec<String> = entries
        .iter()
        .map(|d| {
            if several {
                entry_host(d).to_string()
            } else {
                d.clone()
            }
        })
        .filter(|d| {
            let fresh = !seen.contains(d);
            if fresh {
                seen.push(d.clone());
            }
            fresh
        })
        .take(MAX_SITE_OPERATORS)
        .map(|d| format!("site:{d}"))
        .collect();
    let exc: Vec<String> = if inc.is_empty() {
        exclude
            .iter()
            .map(|d| normalize_domain_entry(d))
            .filter(|d| !d.is_empty())
            .take(MAX_EXCLUDE_OPERATORS)
            .map(|d| format!("-site:{d}"))
            .collect()
    } else {
        // With an allow-list, exclusions are redundant and only lengthen the query.
        Vec::new()
    };
    let mut out = query.to_string();
    match inc.len() {
        0 => {}
        1 => out.push_str(&format!(" {}", inc[0])),
        _ => out.push_str(&format!(" ({})", inc.join(" OR "))),
    }
    for e in exc {
        out.push(' ');
        out.push_str(&e);
    }
    out
}

/// Does `url` fall under the normalised `entry` (host or parent host, then path prefix)?
pub fn url_matches(url: &str, entry: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let want_host = entry_host(entry);
    if want_host.is_empty() || !(host == want_host || host.ends_with(&format!(".{want_host}"))) {
        return false;
    }
    let want_path = &entry[want_host.len()..];
    if want_path.is_empty() {
        return true;
    }
    let path = u.path().to_ascii_lowercase();
    path == want_path || path.starts_with(&format!("{want_path}/"))
}

/// Keep the results inside `include` (when set) and outside `exclude`. Ranks are
/// renumbered.
pub fn filter_results(
    results: Vec<SearchResult>,
    include: &[String],
    exclude: &[String],
) -> Vec<SearchResult> {
    let inc: Vec<String> = include
        .iter()
        .map(|d| normalize_domain_entry(d))
        .filter(|d| !d.is_empty())
        .collect();
    let exc: Vec<String> = exclude
        .iter()
        .map(|d| normalize_domain_entry(d))
        .filter(|d| !d.is_empty())
        .collect();
    if inc.is_empty() && exc.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|r| inc.is_empty() || inc.iter().any(|e| url_matches(&r.url, e)))
        .filter(|r| !exc.iter().any(|e| url_matches(&r.url, e)))
        .enumerate()
        .map(|(i, mut r)| {
            r.rank = i;
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn one_domain_is_a_bare_site_operator() {
        assert_eq!(
            with_site_operators("chômage des jeunes", &s(&["insee.fr"]), &[]),
            "chômage des jeunes site:insee.fr"
        );
    }

    #[test]
    fn several_domains_are_or_ed_in_parentheses_and_capped() {
        let many = s(&[
            "insee.fr",
            "https://www.dares.travail-emploi.gouv.fr/",
            "*.gouv.fr",
            "insee.fr",
            "a.fr",
            "b.fr",
            "c.fr",
            "d.fr",
        ]);
        let q = with_site_operators("taux de chômage", &many, &[]);
        assert_eq!(
            q,
            "taux de chômage (site:insee.fr OR site:dares.travail-emploi.gouv.fr OR site:gouv.fr OR site:a.fr OR site:b.fr OR site:c.fr)"
        );
        assert_eq!(q.matches("site:").count(), MAX_SITE_OPERATORS);
    }

    #[test]
    fn a_caller_written_site_operator_is_kept_as_is() {
        assert_eq!(
            with_site_operators("site:lemonde.fr budget", &s(&["insee.fr"]), &[]),
            "site:lemonde.fr budget"
        );
    }

    #[test]
    fn exclusions_become_minus_site_only_without_an_allow_list() {
        assert_eq!(
            with_site_operators("budget", &[], &s(&["x.com", "reddit.com"])),
            "budget -site:x.com -site:reddit.com"
        );
        assert_eq!(
            with_site_operators("budget", &s(&["insee.fr"]), &s(&["x.com"])),
            "budget site:insee.fr"
        );
    }

    #[test]
    fn path_entries_keep_their_section() {
        assert_eq!(
            with_site_operators("élèves 6e lire", &s(&["lemonde.fr/les-decodeurs/"]), &[]),
            "élèves 6e lire site:lemonde.fr/les-decodeurs"
        );
        assert_eq!(
            with_site_operators(
                "q",
                &s(&[
                    "lemonde.fr/les-decodeurs",
                    "liberation.fr/checknews",
                    "tf1info.fr"
                ]),
                &[]
            ),
            "q (site:lemonde.fr OR site:liberation.fr OR site:tf1info.fr)"
        );
        assert_eq!(
            hosts(&s(&[
                "lemonde.fr/les-decodeurs",
                "www.lemonde.fr",
                "liberation.fr/checknews"
            ])),
            s(&["lemonde.fr", "liberation.fr"])
        );
    }

    #[test]
    fn garbage_entries_are_dropped() {
        assert_eq!(normalize_domain_entry("not a domain"), "");
        assert_eq!(normalize_domain_entry("localhost"), "");
        assert_eq!(with_site_operators("q", &s(&["", "  "]), &[]), "q");
    }

    fn r(url: &str) -> SearchResult {
        SearchResult {
            url: url.into(),
            title: String::new(),
            snippet: String::new(),
            domain: String::new(),
            rank: 9,
            published_date: None,
            language: None,
            full_content: None,
            extra_snippets: None,
        }
    }

    #[test]
    fn results_are_filtered_by_host_parent_and_path() {
        let got = filter_results(
            vec![
                r("https://www.insee.fr/fr/statistiques/1"),
                r("https://www.latimes.com/a"),
                r("https://dares.travail-emploi.gouv.fr/x"),
                r("https://www.lemonde.fr/les-decodeurs/article/2024/x.html"),
                r("https://www.lemonde.fr/politique/article/x.html"),
                r("https://www.lemonde.fr/les-decodeurs-bis/x.html"),
            ],
            &s(&["insee.fr", "gouv.fr", "lemonde.fr/les-decodeurs"]),
            &[],
        );
        let urls: Vec<&str> = got.iter().map(|x| x.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.insee.fr/fr/statistiques/1",
                "https://dares.travail-emploi.gouv.fr/x",
                "https://www.lemonde.fr/les-decodeurs/article/2024/x.html"
            ]
        );
        assert_eq!(
            got.iter().map(|x| x.rank).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn exclusions_filter_too() {
        let got = filter_results(
            vec![r("https://x.com/a"), r("https://insee.fr/b")],
            &[],
            &s(&["x.com"]),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://insee.fr/b");
    }

    #[test]
    fn freshness_parses_and_maps_to_windows() {
        let today = d(2026, 10, 10);
        let f = Freshness::from_request(Some("month"), None, None);
        assert_eq!(f.as_recency(today), Some(Recency::Month));
        assert_eq!(f.range(today), (Some(d(2026, 9, 9)), Some(today)));

        // A range ending today: smallest covering window.
        let f = Freshness::from_request(None, Some("2026-10-05"), None);
        assert_eq!(f.as_recency(today), Some(Recency::Week));
        let f = Freshness::from_request(None, Some("2026-02-01T00:00:00Z"), Some("2026-10-10"));
        assert_eq!(f.as_recency(today), Some(Recency::Year));
        // Historical ranges cannot be a window.
        let f = Freshness::from_request(None, Some("1962-01-01"), Some("1963-12-31"));
        assert_eq!(f.as_recency(today), None);
        assert_eq!(f.range(today), (Some(d(1962, 1, 1)), Some(d(1963, 12, 31))));
        let f = Freshness::from_request(None, Some("2020-01-01"), None);
        assert_eq!(f.as_recency(today), None);

        assert!(Freshness::from_request(Some("soon"), Some("yesterday"), None).is_empty());
        assert_eq!(Freshness::default().cache_key(), "");
        assert_eq!(
            Freshness::from_request(Some("w"), None, None).cache_key(),
            "week||"
        );
    }
}
