use std::collections::HashMap;

use async_trait::async_trait;
use proviz_core::models::{ApiKey, Group, GroupMember, Provider, SearchLog};
use proviz_core::storage::StorageBackend;
pub use proviz_core::storage::StorageError;
use sqlx::{postgres::PgRow, PgPool, Row};
use uuid::Uuid;

// Schema split into individual statements — sqlx executes each separately.
const SCHEMA_STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS ps_providers (\
        id TEXT PRIMARY KEY, \
        slug TEXT UNIQUE NOT NULL, \
        name TEXT NOT NULL, \
        base_url TEXT, \
        is_active BOOLEAN NOT NULL DEFAULT TRUE, \
        priority BIGINT NOT NULL DEFAULT 0, \
        avg_latency_ms BIGINT, \
        coverage_scores TEXT NOT NULL DEFAULT '{}', \
        notes TEXT, \
        created_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))\
    )",
    "CREATE TABLE IF NOT EXISTS ps_api_keys (\
        id TEXT PRIMARY KEY, \
        provider_id TEXT NOT NULL REFERENCES ps_providers(id), \
        label TEXT NOT NULL, \
        key_ref TEXT NOT NULL, \
        is_active BOOLEAN NOT NULL DEFAULT TRUE, \
        rps_limit DOUBLE PRECISION, \
        rpm_limit BIGINT, \
        rpd_limit BIGINT, \
        last_used_at TEXT, \
        created_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))\
    )",
    "CREATE TABLE IF NOT EXISTS ps_groups (\
        id TEXT PRIMARY KEY, \
        slug TEXT UNIQUE NOT NULL, \
        name TEXT NOT NULL, \
        description TEXT, \
        is_active BOOLEAN NOT NULL DEFAULT TRUE, \
        created_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))\
    )",
    "CREATE TABLE IF NOT EXISTS ps_group_members (\
        id TEXT PRIMARY KEY, \
        group_id TEXT NOT NULL REFERENCES ps_groups(id), \
        api_key_id TEXT NOT NULL REFERENCES ps_api_keys(id), \
        priority BIGINT NOT NULL DEFAULT 0, \
        is_enabled BOOLEAN NOT NULL DEFAULT TRUE, \
        UNIQUE(group_id, api_key_id)\
    )",
    "CREATE TABLE IF NOT EXISTS ps_rate_events (\
        id TEXT PRIMARY KEY, \
        api_key_id TEXT NOT NULL REFERENCES ps_api_keys(id), \
        event_type TEXT NOT NULL, \
        occurred_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))\
    )",
    "CREATE TABLE IF NOT EXISTS ps_search_log (\
        id TEXT PRIMARY KEY, \
        query_hash TEXT NOT NULL, \
        group_slug TEXT, \
        language TEXT, \
        country TEXT, \
        provider_slug TEXT, \
        api_key_id TEXT, \
        n_requested BIGINT, \
        n_returned BIGINT, \
        duration_ms BIGINT, \
        cache_hit BOOLEAN NOT NULL DEFAULT FALSE, \
        success BOOLEAN, \
        error_type TEXT, \
        fallback_chain TEXT, \
        requested_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))\
    )",
    "CREATE INDEX IF NOT EXISTS ps_idx_api_keys_provider ON ps_api_keys(provider_id)",
    "CREATE INDEX IF NOT EXISTS ps_idx_group_members_group ON ps_group_members(group_id)",
    "CREATE INDEX IF NOT EXISTS ps_idx_group_members_key ON ps_group_members(api_key_id)",
    "CREATE INDEX IF NOT EXISTS ps_idx_rate_events_key ON ps_rate_events(api_key_id)",
    "CREATE INDEX IF NOT EXISTS ps_idx_search_log_requested ON ps_search_log(requested_at)",
];

fn pg_err(e: sqlx::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

fn json_err(e: serde_json::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

fn row_err(e: sqlx::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

fn read_provider(row: &PgRow) -> Result<Provider, StorageError> {
    let cs_json: String = row.try_get("coverage_scores").map_err(row_err)?;
    let coverage_scores: HashMap<String, f64> = serde_json::from_str(&cs_json).map_err(json_err)?;
    Ok(Provider {
        id: row.try_get("id").map_err(row_err)?,
        slug: row.try_get("slug").map_err(row_err)?,
        name: row.try_get("name").map_err(row_err)?,
        base_url: row.try_get("base_url").map_err(row_err)?,
        is_active: row.try_get("is_active").map_err(row_err)?,
        priority: row.try_get("priority").map_err(row_err)?,
        avg_latency_ms: row.try_get("avg_latency_ms").map_err(row_err)?,
        coverage_scores,
        notes: row.try_get("notes").map_err(row_err)?,
        created_at: row.try_get("created_at").map_err(row_err)?,
    })
}

fn read_api_key(row: &PgRow) -> Result<ApiKey, StorageError> {
    Ok(ApiKey {
        id: row.try_get("id").map_err(row_err)?,
        provider_id: row.try_get("provider_id").map_err(row_err)?,
        label: row.try_get("label").map_err(row_err)?,
        key_ref: row.try_get("key_ref").map_err(row_err)?,
        is_active: row.try_get("is_active").map_err(row_err)?,
        rps_limit: row.try_get("rps_limit").map_err(row_err)?,
        rpm_limit: row.try_get("rpm_limit").map_err(row_err)?,
        rpd_limit: row.try_get("rpd_limit").map_err(row_err)?,
        last_used_at: row.try_get("last_used_at").map_err(row_err)?,
        created_at: row.try_get("created_at").map_err(row_err)?,
    })
}

#[derive(Clone)]
pub struct PgStorage {
    pool: PgPool,
}

impl PgStorage {
    pub async fn connect(database_url: &str) -> Result<Self, anyhow::Error> {
        let pool = PgPool::connect(database_url).await?;
        for stmt in SCHEMA_STATEMENTS {
            sqlx::query(stmt).execute(&pool).await?;
        }
        Ok(Self { pool })
    }
}

#[async_trait]
impl StorageBackend for PgStorage {
    // -----------------------------------------------------------------------
    // Providers
    // -----------------------------------------------------------------------

    async fn list_providers(&self) -> Result<Vec<Provider>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, slug, name, base_url, is_active, priority, avg_latency_ms,
                    coverage_scores, notes, created_at
             FROM ps_providers ORDER BY priority ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        rows.iter().map(read_provider).collect()
    }

    async fn get_provider_by_slug(&self, slug: &str) -> Result<Provider, StorageError> {
        let row = sqlx::query(
            "SELECT id, slug, name, base_url, is_active, priority, avg_latency_ms,
                    coverage_scores, notes, created_at
             FROM ps_providers WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?
        .ok_or_else(|| StorageError::NotFound(slug.to_string()))?;
        read_provider(&row)
    }

    async fn create_provider(&self, p: Provider) -> Result<Provider, StorageError> {
        let cs_json = serde_json::to_string(&p.coverage_scores).map_err(json_err)?;
        sqlx::query(
            "INSERT INTO ps_providers (id, slug, name, base_url, is_active, priority,
             avg_latency_ms, coverage_scores, notes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&p.id)
        .bind(&p.slug)
        .bind(&p.name)
        .bind(&p.base_url)
        .bind(p.is_active)
        .bind(p.priority)
        .bind(p.avg_latency_ms)
        .bind(&cs_json)
        .bind(&p.notes)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(p)
    }

    async fn update_provider_fields(
        &self,
        slug: &str,
        priority: Option<i64>,
        is_active: Option<bool>,
        coverage_scores: Option<HashMap<String, f64>>,
        notes: Option<Option<String>>,
    ) -> Result<(), StorageError> {
        if let Some(p) = priority {
            sqlx::query("UPDATE ps_providers SET priority = $1 WHERE slug = $2")
                .bind(p)
                .bind(slug)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(a) = is_active {
            sqlx::query("UPDATE ps_providers SET is_active = $1 WHERE slug = $2")
                .bind(a)
                .bind(slug)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(cs) = coverage_scores {
            let json = serde_json::to_string(&cs).map_err(json_err)?;
            sqlx::query("UPDATE ps_providers SET coverage_scores = $1 WHERE slug = $2")
                .bind(&json)
                .bind(slug)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(n) = notes {
            sqlx::query("UPDATE ps_providers SET notes = $1 WHERE slug = $2")
                .bind(n)
                .bind(slug)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        Ok(())
    }

    async fn update_avg_latency(
        &self,
        provider_id: &str,
        latency_ms: i64,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE ps_providers SET avg_latency_ms = CASE
                WHEN avg_latency_ms IS NULL THEN $1
                ELSE (avg_latency_ms * 0.8 + $1 * 0.2)::BIGINT
             END WHERE id = $2",
        )
        .bind(latency_ms)
        .bind(provider_id)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // API Keys
    // -----------------------------------------------------------------------

    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                    rpd_limit, last_used_at, created_at FROM ps_api_keys",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        rows.iter().map(read_api_key).collect()
    }

    async fn list_keys_for_provider(&self, provider_id: &str) -> Result<Vec<ApiKey>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                    rpd_limit, last_used_at, created_at FROM ps_api_keys WHERE provider_id = $1",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        rows.iter().map(read_api_key).collect()
    }

    async fn get_api_key(&self, id: &str) -> Result<ApiKey, StorageError> {
        let row = sqlx::query(
            "SELECT id, provider_id, label, key_ref, is_active, rps_limit, rpm_limit,
                    rpd_limit, last_used_at, created_at FROM ps_api_keys WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?
        .ok_or_else(|| StorageError::NotFound(id.to_string()))?;
        read_api_key(&row)
    }

    async fn create_api_key(&self, key: ApiKey) -> Result<ApiKey, StorageError> {
        sqlx::query(
            "INSERT INTO ps_api_keys (id, provider_id, label, key_ref, is_active, rps_limit,
             rpm_limit, rpd_limit) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&key.id)
        .bind(&key.provider_id)
        .bind(&key.label)
        .bind(&key.key_ref)
        .bind(key.is_active)
        .bind(key.rps_limit)
        .bind(key.rpm_limit)
        .bind(key.rpd_limit)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(key)
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
        if let Some(v) = label {
            sqlx::query("UPDATE ps_api_keys SET label = $1 WHERE id = $2")
                .bind(&v)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(v) = is_active {
            sqlx::query("UPDATE ps_api_keys SET is_active = $1 WHERE id = $2")
                .bind(v)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(v) = key_ref {
            sqlx::query("UPDATE ps_api_keys SET key_ref = $1 WHERE id = $2")
                .bind(&v)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(v) = rpm_limit {
            sqlx::query("UPDATE ps_api_keys SET rpm_limit = $1 WHERE id = $2")
                .bind(v)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        if let Some(v) = rpd_limit {
            sqlx::query("UPDATE ps_api_keys SET rpd_limit = $1 WHERE id = $2")
                .bind(v)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(pg_err)?;
        }
        Ok(())
    }

    async fn soft_delete_api_key(&self, id: &str) -> Result<(), StorageError> {
        sqlx::query("UPDATE ps_api_keys SET is_active = FALSE WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    async fn touch_api_key(&self, id: &str) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE ps_api_keys SET last_used_at = \
             to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Groups
    // -----------------------------------------------------------------------

    async fn list_groups(&self) -> Result<Vec<Group>, StorageError> {
        let rows =
            sqlx::query("SELECT id, slug, name, description, is_active, created_at FROM ps_groups")
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?;
        rows.iter()
            .map(|row| {
                Ok(Group {
                    id: row.try_get("id").map_err(row_err)?,
                    slug: row.try_get("slug").map_err(row_err)?,
                    name: row.try_get("name").map_err(row_err)?,
                    description: row.try_get("description").map_err(row_err)?,
                    is_active: row.try_get("is_active").map_err(row_err)?,
                    created_at: row.try_get("created_at").map_err(row_err)?,
                })
            })
            .collect()
    }

    async fn create_group(&self, g: Group) -> Result<Group, StorageError> {
        sqlx::query(
            "INSERT INTO ps_groups (id, slug, name, description, is_active) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&g.id)
        .bind(&g.slug)
        .bind(&g.name)
        .bind(&g.description)
        .bind(g.is_active)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(g)
    }

    async fn list_group_members(&self) -> Result<Vec<GroupMember>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, group_id, api_key_id, priority, is_enabled FROM ps_group_members",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        rows.iter()
            .map(|row| {
                Ok(GroupMember {
                    id: row.try_get("id").map_err(row_err)?,
                    group_id: row.try_get("group_id").map_err(row_err)?,
                    api_key_id: row.try_get("api_key_id").map_err(row_err)?,
                    priority: row.try_get("priority").map_err(row_err)?,
                    is_enabled: row.try_get("is_enabled").map_err(row_err)?,
                })
            })
            .collect()
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
        sqlx::query(
            "INSERT INTO ps_group_members (id, group_id, api_key_id, priority, is_enabled)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&member.id)
        .bind(&member.group_id)
        .bind(&member.api_key_id)
        .bind(member.priority)
        .bind(member.is_enabled)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(member)
    }

    async fn remove_group_member(
        &self,
        group_id: &str,
        api_key_id: &str,
    ) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM ps_group_members WHERE group_id = $1 AND api_key_id = $2")
            .bind(group_id)
            .bind(api_key_id)
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Rate events
    // -----------------------------------------------------------------------

    async fn record_rate_event(
        &self,
        api_key_id: &str,
        event_type: &str,
    ) -> Result<(), StorageError> {
        sqlx::query("INSERT INTO ps_rate_events (id, api_key_id, event_type) VALUES ($1, $2, $3)")
            .bind(Uuid::new_v4().to_string())
            .bind(api_key_id)
            .bind(event_type)
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Search log
    // -----------------------------------------------------------------------

    async fn log_search(&self, log: SearchLog) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO ps_search_log (id, query_hash, group_slug, language, country,
             provider_slug, api_key_id, n_requested, n_returned, duration_ms,
             cache_hit, success, error_type, fallback_chain)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(&log.id)
        .bind(&log.query_hash)
        .bind(&log.group_slug)
        .bind(&log.language)
        .bind(&log.country)
        .bind(&log.provider_slug)
        .bind(&log.api_key_id)
        .bind(log.n_requested)
        .bind(log.n_returned)
        .bind(log.duration_ms)
        .bind(log.cache_hit)
        .bind(log.success)
        .bind(&log.error_type)
        .bind(&log.fallback_chain)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(())
    }

    async fn stats_window(&self, window_secs: i64) -> Result<(i64, i64, i64), StorageError> {
        let cutoff = format_cutoff(window_secs);
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT,
                    COALESCE(SUM(CASE WHEN cache_hit THEN 1 ELSE 0 END), 0)::BIGINT,
                    COALESCE(SUM(CASE WHEN success = FALSE THEN 1 ELSE 0 END), 0)::BIGINT
             FROM ps_search_log WHERE requested_at >= $1",
        )
        .bind(&cutoff)
        .fetch_one(&self.pool)
        .await
        .map_err(pg_err)?;

        Ok((
            row.try_get(0).map_err(row_err)?,
            row.try_get(1).map_err(row_err)?,
            row.try_get(2).map_err(row_err)?,
        ))
    }

    async fn stats_by_provider(
        &self,
        window_secs: i64,
    ) -> Result<Vec<(String, i64, i64, Option<i64>)>, StorageError> {
        let cutoff = format_cutoff(window_secs);
        let rows = sqlx::query(
            "SELECT provider_slug, COUNT(*)::BIGINT,
                    COALESCE(SUM(CASE WHEN success = FALSE THEN 1 ELSE 0 END), 0)::BIGINT,
                    AVG(duration_ms)::BIGINT
             FROM ps_search_log
             WHERE requested_at >= $1 AND provider_slug IS NOT NULL
             GROUP BY provider_slug",
        )
        .bind(&cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;

        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get(0).map_err(row_err)?,
                    row.try_get(1).map_err(row_err)?,
                    row.try_get(2).map_err(row_err)?,
                    row.try_get(3).map_err(row_err)?,
                ))
            })
            .collect()
    }
}

// Build a cutoff timestamp string in 'YYYY-MM-DD HH24:MI:SS' format for comparison.
fn format_cutoff(window_secs: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cutoff_ts = now.saturating_sub(window_secs);
    let dt = chrono::DateTime::from_timestamp(cutoff_ts, 0)
        .unwrap_or_default()
        .naive_utc();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}
