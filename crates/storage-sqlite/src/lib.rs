pub mod migrations;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use proviz_core::models::{ApiKey, Group, GroupMember, Provider, SearchLog};
use proviz_core::storage::StorageBackend;
// Re-export so callers using `storage_sqlite::StorageError` keep compiling.
pub use proviz_core::storage::StorageError;
use rusqlite::{params, Connection};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

// Private error type for inside spawn_blocking closures.
#[derive(Debug)]
enum InternalError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    NotFound(String),
}

impl std::fmt::Display for InternalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite: {e}"),
            Self::Json(e) => write!(f, "JSON: {e}"),
            Self::NotFound(s) => write!(f, "Not found: {s}"),
        }
    }
}

impl From<rusqlite::Error> for InternalError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<serde_json::Error> for InternalError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<InternalError> for StorageError {
    fn from(e: InternalError) -> Self {
        match e {
            InternalError::NotFound(s) => StorageError::NotFound(s),
            other => StorageError::Backend(other.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open(path)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        migrations::run_migrations(&conn)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        migrations::run_migrations(&conn)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<F, R>(&self, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&Connection) -> Result<R, InternalError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            f(&guard)
        })
        .await
        .map_err(|_| StorageError::Join)?
        .map_err(StorageError::from)
    }
}

fn read_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: row.get(0)?,
        provider_id: row.get(1)?,
        label: row.get(2)?,
        key_ref: row.get(3)?,
        is_active: row.get(4)?,
        rps_limit: row.get(5)?,
        rpm_limit: row.get(6)?,
        rpd_limit: row.get(7)?,
        last_used_at: row.get(8)?,
        created_at: row.get(9)?,
    })
}

#[async_trait]
impl StorageBackend for Storage {
    // -----------------------------------------------------------------------
    // Providers
    // -----------------------------------------------------------------------

    async fn list_providers(&self) -> Result<Vec<Provider>, StorageError> {
        self.with_conn(|conn| {
            type Row = (
                String,
                String,
                String,
                Option<String>,
                bool,
                i64,
                Option<i64>,
                String,
                Option<String>,
                String,
            );
            let mut stmt = conn.prepare(
                "SELECT id, slug, name, base_url, is_active, priority, avg_latency_ms,
                        coverage_scores, notes, created_at
                 FROM providers ORDER BY priority ASC",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                ))
            })?;
            let raw: Vec<Row> = mapped.collect::<rusqlite::Result<_>>()?;
            raw.into_iter()
                .map(
                    |(id, slug, name, base_url, is_active, priority, avg_latency_ms, cs_json, notes, created_at)| {
                        let coverage_scores: HashMap<String, f64> = serde_json::from_str(&cs_json)?;
                        Ok(Provider {
                            id,
                            slug,
                            name,
                            base_url,
                            is_active,
                            priority,
                            avg_latency_ms,
                            coverage_scores,
                            notes,
                            created_at,
                        })
                    },
                )
                .collect()
        })
        .await
    }

    async fn get_provider_by_slug(&self, slug: &str) -> Result<Provider, StorageError> {
        let slug = slug.to_string();
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, slug, name, base_url, is_active, priority, avg_latency_ms,
                        coverage_scores, notes, created_at
                 FROM providers WHERE slug = ?1",
                    params![slug],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, Option<i64>>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, Option<String>>(8)?,
                            row.get::<_, String>(9)?,
                        ))
                    },
                )
                .map_err(|_| InternalError::NotFound(slug.clone()))?;
            let (id, slug_out, name, base_url, is_active, priority, avg_latency_ms, cs_json, notes, created_at) = row;
            let coverage_scores: HashMap<String, f64> = serde_json::from_str(&cs_json)?;
            Ok(Provider {
                id,
                slug: slug_out,
                name,
                base_url,
                is_active,
                priority,
                avg_latency_ms,
                coverage_scores,
                notes,
                created_at,
            })
        })
        .await
    }

    async fn create_provider(&self, p: Provider) -> Result<Provider, StorageError> {
        self.with_conn(move |conn| {
            let cs_json = serde_json::to_string(&p.coverage_scores)?;
            conn.execute(
                "INSERT INTO providers (id, slug, name, base_url, is_active, priority,
                 avg_latency_ms, coverage_scores, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    p.id, p.slug, p.name, p.base_url, p.is_active, p.priority,
                    p.avg_latency_ms, cs_json, p.notes
                ],
            )?;
            Ok(p)
        })
        .await
    }

    async fn update_provider_fields(
        &self,
        slug: &str,
        priority: Option<i64>,
        is_active: Option<bool>,
        coverage_scores: Option<HashMap<String, f64>>,
        notes: Option<Option<String>>,
    ) -> Result<(), StorageError> {
        let slug = slug.to_string();
        self.with_conn(move |conn| {
            if let Some(p) = priority {
                conn.execute(
                    "UPDATE providers SET priority = ?1 WHERE slug = ?2",
                    params![p, slug],
                )?;
            }
            if let Some(a) = is_active {
                conn.execute(
                    "UPDATE providers SET is_active = ?1 WHERE slug = ?2",
                    params![a, slug],
                )?;
            }
            if let Some(cs) = coverage_scores {
                let json = serde_json::to_string(&cs)?;
                conn.execute(
                    "UPDATE providers SET coverage_scores = ?1 WHERE slug = ?2",
                    params![json, slug],
                )?;
            }
            if let Some(n) = notes {
                conn.execute(
                    "UPDATE providers SET notes = ?1 WHERE slug = ?2",
                    params![n, slug],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn update_avg_latency(
        &self,
        provider_id: &str,
        latency_ms: i64,
    ) -> Result<(), StorageError> {
        let id = provider_id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE providers SET avg_latency_ms = CASE
                    WHEN avg_latency_ms IS NULL THEN ?1
                    ELSE CAST(avg_latency_ms * 0.8 + ?1 * 0.2 AS INTEGER)
                 END WHERE id = ?2",
                params![latency_ms, id],
            )?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // API Keys
    // -----------------------------------------------------------------------

    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StorageError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                        rpd_limit, last_used_at, created_at FROM api_keys",
            )?;
            let mapped = stmt.query_map([], read_key)?;
            Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn list_keys_for_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<ApiKey>, StorageError> {
        let pid = provider_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                        rpd_limit, last_used_at, created_at FROM api_keys WHERE provider_id = ?1",
            )?;
            let mapped = stmt.query_map(params![pid], read_key)?;
            Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn get_api_key(&self, id: &str) -> Result<ApiKey, StorageError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                        rpd_limit, last_used_at, created_at FROM api_keys WHERE id = ?1",
                params![id.clone()],
                read_key,
            )
            .map_err(|_| InternalError::NotFound(id))
        })
        .await
    }

    async fn create_api_key(&self, key: ApiKey) -> Result<ApiKey, StorageError> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO api_keys (id, provider_id, label, key_ref, is_active, rps_limit,
                 rpm_limit, rpd_limit) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    key.id, key.provider_id, key.label, key.key_ref, key.is_active,
                    key.rps_limit, key.rpm_limit, key.rpd_limit
                ],
            )?;
            Ok(key)
        })
        .await
    }

    async fn update_api_key_fields(
        &self,
        id: &str,
        label: Option<String>,
        is_active: Option<bool>,
        key_ref: Option<String>,
        rpm_limit: Option<Option<i64>>,
        rpd_limit: Option<Option<i64>>,
    ) -> Result<(), StorageError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            if let Some(v) = label {
                conn.execute("UPDATE api_keys SET label = ?1 WHERE id = ?2", params![v, id])?;
            }
            if let Some(v) = is_active {
                conn.execute("UPDATE api_keys SET is_active = ?1 WHERE id = ?2", params![v, id])?;
            }
            if let Some(v) = key_ref {
                conn.execute("UPDATE api_keys SET key_ref = ?1 WHERE id = ?2", params![v, id])?;
            }
            if let Some(v) = rpm_limit {
                conn.execute("UPDATE api_keys SET rpm_limit = ?1 WHERE id = ?2", params![v, id])?;
            }
            if let Some(v) = rpd_limit {
                conn.execute("UPDATE api_keys SET rpd_limit = ?1 WHERE id = ?2", params![v, id])?;
            }
            Ok(())
        })
        .await
    }

    async fn soft_delete_api_key(&self, id: &str) -> Result<(), StorageError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.execute("UPDATE api_keys SET is_active = 0 WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    async fn touch_api_key(&self, id: &str) -> Result<(), StorageError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE api_keys SET last_used_at = datetime('now') WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Groups
    // -----------------------------------------------------------------------

    async fn list_groups(&self) -> Result<Vec<Group>, StorageError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, slug, name, description, is_active, created_at FROM groups",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok(Group {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    is_active: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?;
            Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn create_group(&self, g: Group) -> Result<Group, StorageError> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO groups (id, slug, name, description, is_active) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![g.id, g.slug, g.name, g.description, g.is_active],
            )?;
            Ok(g)
        })
        .await
    }

    async fn list_group_members(&self) -> Result<Vec<GroupMember>, StorageError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, group_id, api_key_id, priority, is_enabled FROM group_members",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok(GroupMember {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    api_key_id: row.get(2)?,
                    priority: row.get(3)?,
                    is_enabled: row.get(4)?,
                })
            })?;
            Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn add_group_member(
        &self,
        group_id: &str,
        api_key_id: &str,
        priority: i64,
    ) -> Result<GroupMember, StorageError> {
        let member = GroupMember {
            id: Uuid::new_v4().to_string(),
            group_id: group_id.to_string(),
            api_key_id: api_key_id.to_string(),
            priority,
            is_enabled: true,
        };
        let m = member.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO group_members (id, group_id, api_key_id, priority, is_enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![m.id, m.group_id, m.api_key_id, m.priority, m.is_enabled],
            )?;
            Ok(m)
        })
        .await?;
        Ok(member)
    }

    async fn remove_group_member(
        &self,
        group_id: &str,
        api_key_id: &str,
    ) -> Result<(), StorageError> {
        let (gid, kid) = (group_id.to_string(), api_key_id.to_string());
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM group_members WHERE group_id = ?1 AND api_key_id = ?2",
                params![gid, kid],
            )?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Rate events
    // -----------------------------------------------------------------------

    async fn record_rate_event(
        &self,
        api_key_id: &str,
        event_type: &str,
    ) -> Result<(), StorageError> {
        let (kid, et) = (api_key_id.to_string(), event_type.to_string());
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO rate_events (id, api_key_id, event_type) VALUES (?1, ?2, ?3)",
                params![Uuid::new_v4().to_string(), kid, et],
            )?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Search log
    // -----------------------------------------------------------------------

    async fn log_search(&self, log: SearchLog) -> Result<(), StorageError> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO search_log (id, query_hash, group_slug, language, country,
                 provider_slug, api_key_id, n_requested, n_returned, duration_ms,
                 cache_hit, success, error_type, fallback_chain)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    log.id, log.query_hash, log.group_slug, log.language, log.country,
                    log.provider_slug, log.api_key_id, log.n_requested, log.n_returned,
                    log.duration_ms, log.cache_hit, log.success, log.error_type, log.fallback_chain
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn stats_window(&self, window_secs: i64) -> Result<(i64, i64, i64), StorageError> {
        self.with_conn(move |conn| {
            let row: (i64, i64, i64) = conn.query_row(
                "SELECT COUNT(*),
                        SUM(CASE WHEN cache_hit = 1 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN success = 0 THEN 1 ELSE 0 END)
                 FROM search_log WHERE requested_at >= datetime('now', ?1)",
                params![format!("-{window_secs} seconds")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            Ok(row)
        })
        .await
    }

    async fn stats_by_provider(
        &self,
        window_secs: i64,
    ) -> Result<Vec<(String, i64, i64, Option<i64>)>, StorageError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_slug, COUNT(*),
                        SUM(CASE WHEN success = 0 THEN 1 ELSE 0 END),
                        CAST(AVG(duration_ms) AS INTEGER)
                 FROM search_log
                 WHERE requested_at >= datetime('now', ?1) AND provider_slug IS NOT NULL
                 GROUP BY provider_slug",
            )?;
            let mapped = stmt.query_map(params![format!("-{window_secs} seconds")], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}
