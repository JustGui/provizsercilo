use proviz_core::models::{ApiKey, Group, Provider};
use std::collections::HashMap;

use crate::Storage;

fn make_storage() -> Storage {
    Storage::open_in_memory().expect("in-memory storage")
}

fn make_provider(slug: &str) -> Provider {
    Provider {
        id: uuid::Uuid::new_v4().to_string(),
        slug: slug.to_string(),
        name: format!("{slug} Search"),
        base_url: None,
        is_active: true,
        priority: 0,
        avg_latency_ms: None,
        coverage_scores: HashMap::new(),
        notes: None,
        created_at: String::new(),
    }
}

fn make_key(provider_id: &str, label: &str) -> ApiKey {
    ApiKey {
        id: uuid::Uuid::new_v4().to_string(),
        provider_id: provider_id.to_string(),
        label: label.to_string(),
        key_ref: format!("{label}_REF"),
        is_active: true,
        rps_limit: None,
        rpm_limit: Some(60),
        rpd_limit: Some(2000),
        last_used_at: None,
        created_at: String::new(),
    }
}

#[tokio::test]
async fn test_create_and_list_providers() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let providers = storage.list_providers().await.unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].slug, "brave");
}

#[tokio::test]
async fn test_get_provider_by_slug() {
    let storage = make_storage();
    let p = make_provider("tavily");
    storage.create_provider(p.clone()).await.unwrap();
    let found = storage.get_provider_by_slug("tavily").await.unwrap();
    assert_eq!(found.id, p.id);
}

#[tokio::test]
async fn test_get_provider_not_found() {
    let storage = make_storage();
    let result = storage.get_provider_by_slug("nonexistent").await;
    assert!(matches!(result, Err(crate::StorageError::NotFound(_))));
}

#[tokio::test]
async fn test_create_and_list_api_keys() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let k = make_key(&p.id, "key1");
    storage.create_api_key(k.clone()).await.unwrap();

    let keys = storage.list_api_keys().await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].label, "key1");
    assert_eq!(keys[0].rpm_limit, Some(60));
}

#[tokio::test]
async fn test_soft_delete_api_key() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let k = make_key(&p.id, "key1");
    storage.create_api_key(k.clone()).await.unwrap();

    storage.soft_delete_api_key(&k.id).await.unwrap();
    let retrieved = storage.get_api_key(&k.id).await.unwrap();
    assert!(!retrieved.is_active);
}

#[tokio::test]
async fn test_update_provider_fields() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    storage
        .update_provider_fields("brave", Some(10), Some(false), None, None)
        .await
        .unwrap();
    let updated = storage.get_provider_by_slug("brave").await.unwrap();
    assert_eq!(updated.priority, 10);
    assert!(!updated.is_active);
}

#[tokio::test]
async fn test_create_group_and_add_member() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let k = make_key(&p.id, "key1");
    storage.create_api_key(k.clone()).await.unwrap();

    let group = Group {
        id: uuid::Uuid::new_v4().to_string(),
        slug: "test-group".to_string(),
        name: "Test Group".to_string(),
        description: None,
        is_active: true,
        created_at: String::new(),
    };
    storage.create_group(group.clone()).await.unwrap();
    storage.add_group_member(&group.id, &k.id, 0).await.unwrap();

    let members = storage.list_group_members().await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].api_key_id, k.id);
}

#[tokio::test]
async fn test_remove_group_member() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let k = make_key(&p.id, "key1");
    storage.create_api_key(k.clone()).await.unwrap();

    let group = Group {
        id: uuid::Uuid::new_v4().to_string(),
        slug: "g1".to_string(),
        name: "G1".to_string(),
        description: None,
        is_active: true,
        created_at: String::new(),
    };
    storage.create_group(group.clone()).await.unwrap();
    storage.add_group_member(&group.id, &k.id, 0).await.unwrap();
    storage.remove_group_member(&group.id, &k.id).await.unwrap();

    let members = storage.list_group_members().await.unwrap();
    assert!(members.is_empty());
}

#[tokio::test]
async fn test_record_rate_event() {
    let storage = make_storage();
    let p = make_provider("brave");
    storage.create_provider(p.clone()).await.unwrap();
    let k = make_key(&p.id, "key1");
    storage.create_api_key(k.clone()).await.unwrap();
    storage.record_rate_event(&k.id, "rpm").await.unwrap();
    // No assertion needed - just ensure it doesn't error
}

#[tokio::test]
async fn test_coverage_scores_roundtrip() {
    let storage = make_storage();
    let mut p = make_provider("mojeek");
    p.coverage_scores.insert("en_gb".to_string(), 0.95);
    p.coverage_scores.insert("fr".to_string(), 0.3);
    storage.create_provider(p.clone()).await.unwrap();
    let retrieved = storage.get_provider_by_slug("mojeek").await.unwrap();
    assert_eq!(retrieved.coverage_scores.get("en_gb"), Some(&0.95));
    assert_eq!(retrieved.coverage_scores.get("fr"), Some(&0.3));
}
