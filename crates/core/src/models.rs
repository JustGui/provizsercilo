use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub base_url: Option<String>,
    pub is_active: bool,
    pub priority: i64,
    pub avg_latency_ms: Option<i64>,
    pub coverage_scores: HashMap<String, f64>,
    pub notes: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub no_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub provider_id: String,
    pub label: String,
    pub key_ref: String,
    pub is_active: bool,
    pub rps_limit: Option<f64>,
    pub rpm_limit: Option<i64>,
    pub rpd_limit: Option<i64>,
    pub last_used_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: String,
    pub group_id: String,
    pub api_key_id: String,
    pub priority: i64,
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub domain: String,
    pub rank: usize,
    pub published_date: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateEvent {
    pub id: String,
    pub api_key_id: String,
    pub event_type: String,
    pub occurred_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchLog {
    pub id: String,
    pub query_hash: String,
    pub group_slug: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub provider_slug: Option<String>,
    pub api_key_id: Option<String>,
    pub n_requested: Option<i64>,
    pub n_returned: Option<i64>,
    pub duration_ms: Option<i64>,
    pub cache_hit: bool,
    pub success: Option<bool>,
    pub error_type: Option<String>,
    pub fallback_chain: Option<String>,
    pub requested_at: String,
}

/// A (provider, api_key) pair with optional group membership context.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub provider: Provider,
    pub api_key: ApiKey,
    /// Priority override from group_member; None when not using a group.
    pub member_priority: Option<i64>,
}

impl Candidate {
    pub fn effective_priority(&self) -> i64 {
        self.member_priority.unwrap_or(self.provider.priority)
    }
}
