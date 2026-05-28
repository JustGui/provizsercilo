use proviz_core::models::{ApiKey, Candidate, Group, GroupMember, Provider};
use std::sync::Arc;
use tokio::sync::RwLock;

use storage_sqlite::Storage;

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
    /// When `group_slug` is None, returns all active (provider, key) pairs.
    /// When specified, returns only the group's enabled members.
    pub fn candidates(&self, group_slug: Option<&str>) -> Vec<Candidate> {
        if let Some(slug) = group_slug {
            let group = self.groups.iter().find(|g| g.slug == slug && g.is_active);
            if let Some(group) = group {
                return self
                    .group_members
                    .iter()
                    .filter(|m| m.group_id == group.id && m.is_enabled)
                    .filter_map(|m| {
                        let key = self.api_keys.iter().find(|k| k.id == m.api_key_id && k.is_active)?;
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

        // No group — all active keys from all active providers
        self.api_keys
            .iter()
            .filter(|k| k.is_active)
            .filter_map(|k| {
                let provider = self.providers.iter().find(|p| p.id == k.provider_id && p.is_active)?;
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
    storage: Arc<Storage>,
}

impl CatalogStore {
    pub async fn new(storage: Arc<Storage>) -> Result<Self, storage_sqlite::StorageError> {
        let store = Self {
            inner: Arc::new(RwLock::new(Catalog::default())),
            storage,
        };
        store.reload().await?;
        Ok(store)
    }

    pub async fn reload(&self) -> Result<(), storage_sqlite::StorageError> {
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

    pub fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }
}
