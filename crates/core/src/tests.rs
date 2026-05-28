use std::collections::HashMap;

use crate::{
    language_profile::{LanguageProfile, ProfileMatcher},
    models::{ApiKey, Candidate, Provider},
    rate_limit::{ErrorType, RateLimitState, UsageTracker},
    selector::{SelectRequest, Selector},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_provider(slug: &str, priority: i64, active: bool) -> Provider {
    Provider {
        id: slug.to_string(),
        slug: slug.to_string(),
        name: slug.to_string(),
        base_url: None,
        is_active: active,
        priority,
        avg_latency_ms: None,
        coverage_scores: HashMap::new(),
        notes: None,
        created_at: String::new(),
    }
}

fn make_key(id: &str, provider_id: &str, active: bool) -> ApiKey {
    ApiKey {
        id: id.to_string(),
        provider_id: provider_id.to_string(),
        label: id.to_string(),
        key_ref: format!("{id}_REF"),
        is_active: active,
        rps_limit: None,
        rpm_limit: None,
        rpd_limit: None,
        last_used_at: None,
        created_at: String::new(),
    }
}

fn make_candidate(slug: &str, key_id: &str, priority: i64, active: bool) -> Candidate {
    Candidate {
        provider: make_provider(slug, priority, active),
        api_key: make_key(key_id, slug, active),
        member_priority: None,
    }
}

fn make_selector() -> Selector {
    Selector::new(
        RateLimitState::default(),
        UsageTracker::default(),
        ProfileMatcher::new(vec![]),
    )
}

// ---------------------------------------------------------------------------
// Rate limit tests
// ---------------------------------------------------------------------------

#[test]
fn test_rate_limit_cooldown() {
    let rl = RateLimitState::default();
    assert!(!rl.is_limited("key1"));
    rl.report_error("key1", ErrorType::Rps); // 5s cooldown
    assert!(rl.is_limited("key1"));
    assert!(rl.cooldown_remaining_ms("key1") > 0);
    assert!(rl.cooldown_remaining_ms("key1") <= 5000);
}

#[test]
fn test_rate_limit_different_keys() {
    let rl = RateLimitState::default();
    rl.report_error("key1", ErrorType::Rpm);
    assert!(rl.is_limited("key1"));
    assert!(!rl.is_limited("key2"));
}

#[test]
fn test_error_type_cooldown_ordering() {
    assert!(ErrorType::Rps.cooldown_secs() < ErrorType::Rpm.cooldown_secs());
    assert!(ErrorType::Rpm.cooldown_secs() < ErrorType::Rpd.cooldown_secs());
    assert!(ErrorType::Auth.cooldown_secs() > ErrorType::Empty.cooldown_secs());
}

// ---------------------------------------------------------------------------
// Usage tracker tests
// ---------------------------------------------------------------------------

#[test]
fn test_usage_tracker_headroom_no_limit() {
    let ut = UsageTracker::default();
    // No limit configured → always full headroom
    assert_eq!(ut.rpm_headroom("key1", None), 1.0);
    assert_eq!(ut.rpd_headroom("key1", None), 1.0);
    assert_eq!(ut.rps_headroom("key1", None), 1.0);
}

#[test]
fn test_usage_tracker_decrements_on_reserve() {
    let ut = UsageTracker::default();
    assert_eq!(ut.rpm_headroom("key1", Some(10)), 1.0);
    ut.reserve("key1");
    let headroom = ut.rpm_headroom("key1", Some(10));
    assert!(headroom < 1.0);
    assert!(headroom >= 0.9);
}

#[test]
fn test_usage_tracker_five_min_count() {
    let ut = UsageTracker::default();
    assert_eq!(ut.five_min_count("key1"), 0);
    ut.reserve("key1");
    ut.reserve("key1");
    assert_eq!(ut.five_min_count("key1"), 2);
}

// ---------------------------------------------------------------------------
// Selector tests
// ---------------------------------------------------------------------------

#[test]
fn test_selector_picks_lowest_priority() {
    let sel = make_selector();
    let pool = vec![
        make_candidate("brave", "k1", 5, true),
        make_candidate("tavily", "k2", 0, true), // lower priority = preferred
    ];
    let req = SelectRequest::default();
    let (winner, _) = sel.select(&pool, &req, &[], false).unwrap();
    assert_eq!(winner.provider.slug, "tavily");
}

#[test]
fn test_selector_skips_inactive_provider() {
    let sel = make_selector();
    let pool = vec![
        make_candidate("brave", "k1", 0, false), // inactive
        make_candidate("tavily", "k2", 5, true),
    ];
    let req = SelectRequest::default();
    let (winner, _) = sel.select(&pool, &req, &[], false).unwrap();
    assert_eq!(winner.provider.slug, "tavily");
}

#[test]
fn test_selector_skips_rate_limited_key() {
    let rl = RateLimitState::default();
    rl.report_error("k1", ErrorType::Rpm);
    let sel = Selector::new(rl, UsageTracker::default(), ProfileMatcher::new(vec![]));

    let pool = vec![
        make_candidate("brave", "k1", 0, true), // rate limited
        make_candidate("tavily", "k2", 5, true),
    ];
    let req = SelectRequest::default();
    let (winner, _) = sel.select(&pool, &req, &[], false).unwrap();
    assert_eq!(winner.provider.slug, "tavily");
}

#[test]
fn test_selector_skips_excluded_key() {
    let sel = make_selector();
    let pool = vec![
        make_candidate("brave", "k1", 0, true),
        make_candidate("tavily", "k2", 5, true),
    ];
    let req = SelectRequest::default();
    let (winner, _) = sel.select(&pool, &req, &["k1".to_string()], false).unwrap();
    assert_eq!(winner.provider.slug, "tavily");
}

#[test]
fn test_selector_returns_none_when_all_excluded() {
    let sel = make_selector();
    let pool = vec![make_candidate("brave", "k1", 0, true)];
    let req = SelectRequest::default();
    let result = sel.select(&pool, &req, &["k1".to_string()], false);
    assert!(result.is_none());
}

#[test]
fn test_selector_debug_output() {
    let sel = make_selector();
    let pool = vec![
        make_candidate("brave", "k1", 0, true),
        make_candidate("tavily", "k2", 5, true),
    ];
    let req = SelectRequest::default();
    let (_, decisions) = sel.select(&pool, &req, &[], true).unwrap();
    assert!(!decisions.is_empty());
    // Winner should have outcome "selected"
    let selected = decisions.iter().find(|d| d.outcome == "selected");
    assert!(selected.is_some());
}

#[test]
fn test_selector_respects_request_exclude_provider_slugs() {
    let sel = make_selector();
    let pool = vec![
        make_candidate("brave", "k1", 0, true),
        make_candidate("tavily", "k2", 5, true),
    ];
    let req = SelectRequest {
        exclude_provider_slugs: vec!["brave".to_string()],
        ..Default::default()
    };
    let (winner, _) = sel.select(&pool, &req, &[], false).unwrap();
    assert_eq!(winner.provider.slug, "tavily");
}

// ---------------------------------------------------------------------------
// Language profile tests
// ---------------------------------------------------------------------------

#[test]
fn test_profile_specificity_exact_match() {
    let matcher = ProfileMatcher::new(vec![
        LanguageProfile {
            language: "*".into(),
            country: "*".into(),
            priority: vec!["fallback".into()],
            exclude: vec![],
        },
        LanguageProfile {
            language: "fr".into(),
            country: "*".into(),
            priority: vec!["lang_match".into()],
            exclude: vec![],
        },
        LanguageProfile {
            language: "fr".into(),
            country: "fr".into(),
            priority: vec!["exact".into()],
            exclude: vec![],
        },
    ]);
    let p = matcher.find(Some("fr"), Some("fr")).unwrap();
    assert_eq!(p.priority[0], "exact");
}

#[test]
fn test_profile_language_wildcard_fallback() {
    let matcher = ProfileMatcher::new(vec![
        LanguageProfile {
            language: "*".into(),
            country: "*".into(),
            priority: vec!["catchall".into()],
            exclude: vec![],
        },
        LanguageProfile {
            language: "fr".into(),
            country: "*".into(),
            priority: vec!["french".into()],
            exclude: vec![],
        },
    ]);
    let p = matcher.find(Some("fr"), Some("ca")).unwrap();
    assert_eq!(p.priority[0], "french");
}

#[test]
fn test_profile_global_fallback() {
    let matcher = ProfileMatcher::new(vec![LanguageProfile {
        language: "*".into(),
        country: "*".into(),
        priority: vec!["global".into()],
        exclude: vec![],
    }]);
    let p = matcher.find(Some("ja"), Some("jp")).unwrap();
    assert_eq!(p.priority[0], "global");
}

#[test]
fn test_profile_coverage_boost() {
    let profile = LanguageProfile {
        language: "en".into(),
        country: "*".into(),
        priority: vec!["mojeek".into(), "brave".into(), "tavily".into()],
        exclude: vec![],
    };
    let boost_mojeek = ProfileMatcher::coverage_boost(&profile, "mojeek");
    let boost_brave = ProfileMatcher::coverage_boost(&profile, "brave");
    let boost_tavily = ProfileMatcher::coverage_boost(&profile, "tavily");
    let boost_unknown = ProfileMatcher::coverage_boost(&profile, "serper");

    assert!(boost_mojeek > boost_brave);
    assert!(boost_brave > boost_tavily);
    assert_eq!(boost_unknown, 0.0);
}

#[test]
fn test_profile_exclude_removes_from_pool() {
    let sel = Selector::new(
        RateLimitState::default(),
        UsageTracker::default(),
        ProfileMatcher::new(vec![LanguageProfile {
            language: "fr".into(),
            country: "*".into(),
            priority: vec![],
            exclude: vec!["mojeek".into()],
        }]),
    );

    let pool = vec![
        make_candidate("mojeek", "k1", 0, true),
        make_candidate("brave", "k2", 5, true),
    ];
    let req = SelectRequest {
        language: Some("fr".to_string()),
        ..Default::default()
    };
    let (winner, _) = sel.select(&pool, &req, &[], false).unwrap();
    assert_eq!(winner.provider.slug, "brave");
}

// ---------------------------------------------------------------------------
// Key resolver tests
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_key_from_env() {
    use crate::key_resolver::resolve_key;
    let dir = std::path::PathBuf::from("/tmp/nonexistent_secrets_dir_proviz");
    std::env::set_var("PROVIZ_TEST_KEY_XYZ", "test-secret-value");
    let result = resolve_key("PROVIZ_TEST_KEY_XYZ", &dir).unwrap();
    assert_eq!(result, "test-secret-value");
    std::env::remove_var("PROVIZ_TEST_KEY_XYZ");
}

#[test]
fn test_resolve_key_not_found() {
    use crate::key_resolver::resolve_key;
    let dir = std::path::PathBuf::from("/tmp/nonexistent_secrets_dir_proviz");
    let result = resolve_key("PROVIZ_NONEXISTENT_KEY_12345", &dir);
    assert!(result.is_err());
}

#[test]
fn test_resolve_key_from_file() {
    use crate::key_resolver::resolve_key;
    let dir = tempdir::TempDir::new("proviz_test").expect("tempdir");
    let key_file = dir.path().join("MY_KEY");
    std::fs::write(&key_file, "  file-secret-value  \n").unwrap();
    let result = resolve_key("MY_KEY", dir.path()).unwrap();
    assert_eq!(result, "file-secret-value");
}
