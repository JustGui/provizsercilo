use std::collections::HashMap;

use async_trait::async_trait;
use thiserror::Error;

use crate::models::{ApiKey, Group, GroupMember, Provider, SearchLog};

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Storage error: {0}")]
    Backend(String),
    #[error("Task join error")]
    Join,
}

#[async_trait]
pub trait StorageBackend: Send + Sync {
    // --- Providers ---
    async fn list_providers(&self) -> Result<Vec<Provider>, StorageError>;
    async fn get_provider_by_slug(&self, slug: &str) -> Result<Provider, StorageError>;
    async fn create_provider(&self, p: Provider) -> Result<Provider, StorageError>;
    async fn update_provider_fields(
        &self,
        slug: &str,
        priority: Option<i64>,
        is_active: Option<bool>,
        coverage_scores: Option<HashMap<String, f64>>,
        notes: Option<Option<String>>,
    ) -> Result<(), StorageError>;
    async fn update_avg_latency(
        &self,
        provider_id: &str,
        latency_ms: i64,
    ) -> Result<(), StorageError>;

    // --- API Keys ---
    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StorageError>;
    async fn list_keys_for_provider(&self, provider_id: &str) -> Result<Vec<ApiKey>, StorageError>;
    async fn get_api_key(&self, id: &str) -> Result<ApiKey, StorageError>;
    async fn create_api_key(&self, key: ApiKey) -> Result<ApiKey, StorageError>;
    async fn update_api_key_fields(
        &self,
        id: &str,
        label: Option<String>,
        is_active: Option<bool>,
        key_ref: Option<String>,
        rpm_limit: Option<Option<i64>>,
        rpd_limit: Option<Option<i64>>,
    ) -> Result<(), StorageError>;
    async fn soft_delete_api_key(&self, id: &str) -> Result<(), StorageError>;
    async fn touch_api_key(&self, id: &str) -> Result<(), StorageError>;

    // --- Groups ---
    async fn list_groups(&self) -> Result<Vec<Group>, StorageError>;
    async fn create_group(&self, g: Group) -> Result<Group, StorageError>;
    async fn list_group_members(&self) -> Result<Vec<GroupMember>, StorageError>;
    async fn add_group_member(
        &self,
        group_id: &str,
        api_key_id: &str,
        priority: i64,
    ) -> Result<GroupMember, StorageError>;
    async fn remove_group_member(
        &self,
        group_id: &str,
        api_key_id: &str,
    ) -> Result<(), StorageError>;

    // --- Rate events ---
    async fn record_rate_event(
        &self,
        api_key_id: &str,
        event_type: &str,
    ) -> Result<(), StorageError>;

    // --- Search log ---
    async fn log_search(&self, log: SearchLog) -> Result<(), StorageError>;
    async fn stats_window(&self, window_secs: i64) -> Result<(i64, i64, i64), StorageError>;
    async fn stats_by_provider(
        &self,
        window_secs: i64,
    ) -> Result<Vec<(String, i64, i64, Option<i64>)>, StorageError>;
}
