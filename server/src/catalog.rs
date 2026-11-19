use proviz_core::{
    models::{ApiKey, Candidate, Group, GroupMember, Provider},
    storage::{StorageBackend, StorageError},
};
use std::sync::Arc;
use tokio::sync::RwLock;

/// In-memory mirror of the provider/key/group catalog.
/// Loaded at startup, refreshable via /admin/reload without restart.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub providers: Vec<Provider>,
    pub api_keys: Vec<ApiKey>,
    pub groups: Vec<Group>,
    pub group_members: Vec<GroupMember>,
}

impl Catalog {
    /// Build the candidate pool for a given optional group slug.
    ///
    /// When `group_slug` is None, returns all active shared (provider, key) pairs.
    /// When specified, returns only the group's enabled members. Tenant keys are
    /// never part of this shared pool - see [`Catalog::candidates_for`].
    pub fn candidates(&self, group_slug: Option<&str>) -> Vec<Candidate> {
        let mut pool = self.shared_candidates(group_slug);
        pool.retain(|c| c.api_key.tenant.is_none());
        pool
    }

    /// The pool for a caller acting for `tenants` (most specific first, e.g.
    /// `["team:<id>", "client:<id>"]`). For every provider in the shared pool that
    /// has an active key for one of those tenants, the first matching tenant's key
    /// replaces that provider's shared keys at the same tier: a tenant never
    /// borrows the shared keys of a provider it has its own key for, so overflow
    /// moves on down the cascade instead of eating the shared budget.
    pub fn candidates_for(&self, group_slug: Option<&str>, tenants: &[String]) -> Vec<Candidate> {
        let pool = self.candidates(group_slug);
        if tenants.is_empty() {
            return pool;
        }
        let tenant_key = |provider_id: &str| {
            tenants.iter().find_map(|t| {
                self.api_keys.iter().find(|k| {
                    k.is_active
                        && k.provider_id == provider_id
                        && k.tenant.as_deref() == Some(t.as_str())
                })
            })
        };
        let mut out: Vec<Candidate> = Vec::with_capacity(pool.len());
        for c in pool {
            match tenant_key(&c.provider.id) {
                None => out.push(c),
                Some(key) => {
                    if let Some(existing) = out.iter_mut().find(|o| o.api_key.id == key.id) {
                        // Keep the provider's best (lowest) tier.
                        existing.member_priority =
                            match (existing.member_priority, c.member_priority) {
                                (Some(a), Some(b)) => Some(a.min(b)),
                                (a, b) => a.or(b),
                            };
                    } else {
                        out.push(Candidate {
                            provider: c.provider,
                            api_key: key.clone(),
                            member_priority: c.member_priority,
                        });
                    }
                }
            }
        }
        out
    }

    /// Every active key, shared and tenant, for key-level operations (probing a
    /// parked key back to health) - never for choosing which key serves a search.
    pub fn all_key_candidates(&self) -> Vec<Candidate> {
        self.shared_candidates(None)
    }

    fn shared_candidates(&self, group_slug: Option<&str>) -> Vec<Candidate> {
        if let Some(slug) = group_slug {
            let group = self.groups.iter().find(|g| g.slug == slug && g.is_active);
            if let Some(group) = group {
                return self
                    .group_members
                    .iter()
                    .filter(|m| m.group_id == group.id && m.is_enabled)
                    .filter_map(|m| {
                        let key = self
                            .api_keys
                            .iter()
                            .find(|k| k.id == m.api_key_id && k.is_active)?;
                        let provider = self.providers.iter().find(|p| p.id == key.provider_id)?;
                        Some(Candidate {
                            provider: provider.clone(),
                            api_key: key.clone(),
                            member_priority: Some(m.priority),
                        })
                    })
                    .collect();
            }
        }

        // No group - all active keys from all active providers
        self.api_keys
            .iter()
            .filter(|k| k.is_active)
            .filter_map(|k| {
                let provider = self
                    .providers
                    .iter()
                    .find(|p| p.id == k.provider_id && p.is_active)?;
                Some(Candidate {
                    provider: provider.clone(),
                    api_key: k.clone(),
                    member_priority: None,
                })
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct CatalogStore {
    inner: Arc<RwLock<Catalog>>,
    storage: Arc<dyn StorageBackend>,
}

impl CatalogStore {
    pub async fn new(storage: Arc<dyn StorageBackend>) -> Result<Self, StorageError> {
        let store = Self {
            inner: Arc::new(RwLock::new(Catalog::default())),
            storage,
        };
        store.reload().await?;
        Ok(store)
    }

    pub async fn reload(&self) -> Result<(), StorageError> {
        let (providers, api_keys, groups, group_members) = tokio::try_join!(
            self.storage.list_providers(),
            self.storage.list_api_keys(),
            self.storage.list_groups(),
            self.storage.list_group_members(),
        )?;

        let mut w = self.inner.write().await;
        *w = Catalog {
            providers,
            api_keys,
            groups,
            group_members,
        };
        Ok(())
    }

    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, Catalog> {
        self.inner.read().await
    }

    pub fn storage(&self) -> &Arc<dyn StorageBackend> {
        &self.storage
    }
}

#[cfg(test)]
mod tenant_tests {
    use super::*;
    use std::collections::HashMap;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            slug: id.into(),
            name: id.into(),
            base_url: None,
            is_active: true,
            priority: 0,
            avg_latency_ms: None,
            coverage_scores: HashMap::new(),
            notes: None,
            created_at: String::new(),
            no_cache: false,
        }
    }

    fn key(id: &str, provider: &str, tenant: Option<&str>) -> ApiKey {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider_id": provider, "label": id, "key_ref": "R",
            "is_active": true, "created_at": "", "tenant": tenant
        }))
        .unwrap()
    }

    fn member(key: &str, priority: i64) -> GroupMember {
        GroupMember {
            id: key.into(),
            group_id: "g".into(),
            api_key_id: key.into(),
            priority,
            is_enabled: true,
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            providers: vec![provider("staan"), provider("brave")],
            api_keys: vec![
                key("staan-1", "staan", None),
                key("staan-2", "staan", None),
                key("brave-1", "brave", None),
                key("staan-team-a", "staan", Some("team:a")),
                key("staan-client-c", "staan", Some("client:c")),
            ],
            groups: vec![Group {
                id: "g".into(),
                slug: "paid".into(),
                name: "paid".into(),
                description: None,
                is_active: true,
                created_at: String::new(),
            }],
            group_members: vec![
                member("staan-1", 1),
                member("staan-2", 1),
                member("brave-1", 2),
            ],
        }
    }

    fn ids(pool: &[Candidate]) -> Vec<(String, Option<i64>)> {
        pool.iter()
            .map(|c| (c.api_key.id.clone(), c.member_priority))
            .collect()
    }

    #[test]
    fn shared_pool_never_contains_tenant_keys() {
        let c = catalog();
        assert!(c
            .candidates(None)
            .iter()
            .all(|k| k.api_key.tenant.is_none()));
        assert_eq!(c.candidates(Some("paid")).len(), 3);
    }

    #[test]
    fn team_key_replaces_shared_keys_of_its_provider_only() {
        let c = catalog();
        let tenants = vec!["team:a".to_string(), "client:c".to_string()];
        assert_eq!(
            ids(&c.candidates_for(Some("paid"), &tenants)),
            vec![
                ("staan-team-a".into(), Some(1)),
                ("brave-1".into(), Some(2))
            ]
        );
    }

    #[test]
    fn falls_back_to_client_key_then_shared() {
        let c = catalog();
        let pool = c.candidates_for(Some("paid"), &["team:none".into(), "client:c".into()]);
        assert_eq!(pool[0].api_key.id, "staan-client-c");
        let pool = c.candidates_for(Some("paid"), &["team:none".into()]);
        assert_eq!(ids(&pool), ids(&c.candidates(Some("paid"))));
    }

    #[test]
    fn tenant_keys_can_be_probed() {
        let c = catalog();
        assert!(c
            .all_key_candidates()
            .iter()
            .any(|k| k.api_key.id == "staan-team-a"));
    }

    #[test]
    fn inactive_tenant_key_is_ignored() {
        let mut c = catalog();
        c.api_keys
            .iter_mut()
            .find(|k| k.id == "staan-team-a")
            .unwrap()
            .is_active = false;
        let pool = c.candidates_for(Some("paid"), &["team:a".into()]);
        assert_eq!(pool[0].api_key.id, "staan-1");
    }
}
