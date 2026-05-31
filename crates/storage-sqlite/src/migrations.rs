pub const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS providers (
    id            TEXT PRIMARY KEY,
    slug          TEXT UNIQUE NOT NULL,
    name          TEXT NOT NULL,
    base_url      TEXT,
    is_active     INTEGER NOT NULL DEFAULT 1,
    priority      INTEGER NOT NULL DEFAULT 0,
    avg_latency_ms INTEGER,
    coverage_scores TEXT NOT NULL DEFAULT '{}',
    notes         TEXT,
    no_cache      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS api_keys (
    id            TEXT PRIMARY KEY,
    provider_id   TEXT NOT NULL REFERENCES providers(id),
    label         TEXT NOT NULL,
    key_ref       TEXT NOT NULL,
    is_active     INTEGER NOT NULL DEFAULT 1,
    rps_limit     REAL,
    rpm_limit     INTEGER,
    rpd_limit     INTEGER,
    last_used_at  TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS groups (
    id          TEXT PRIMARY KEY,
    slug        TEXT UNIQUE NOT NULL,
    name        TEXT NOT NULL,
    description TEXT,
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS group_members (
    id          TEXT PRIMARY KEY,
    group_id    TEXT NOT NULL REFERENCES groups(id),
    api_key_id  TEXT NOT NULL REFERENCES api_keys(id),
    priority    INTEGER NOT NULL DEFAULT 0,
    is_enabled  INTEGER NOT NULL DEFAULT 1,
    UNIQUE(group_id, api_key_id)
);

CREATE TABLE IF NOT EXISTS rate_events (
    id           TEXT PRIMARY KEY,
    api_key_id   TEXT NOT NULL REFERENCES api_keys(id),
    event_type   TEXT NOT NULL,
    occurred_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS search_log (
    id              TEXT PRIMARY KEY,
    query_hash      TEXT NOT NULL,
    group_slug      TEXT,
    language        TEXT,
    country         TEXT,
    provider_slug   TEXT,
    api_key_id      TEXT,
    n_requested     INTEGER,
    n_returned      INTEGER,
    duration_ms     INTEGER,
    cache_hit       INTEGER NOT NULL DEFAULT 0,
    success         INTEGER,
    error_type      TEXT,
    fallback_chain  TEXT,
    requested_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_api_keys_provider ON api_keys(provider_id);
CREATE INDEX IF NOT EXISTS idx_group_members_group ON group_members(group_id);
CREATE INDEX IF NOT EXISTS idx_group_members_key ON group_members(api_key_id);
CREATE INDEX IF NOT EXISTS idx_rate_events_key ON rate_events(api_key_id);
CREATE INDEX IF NOT EXISTS idx_search_log_requested ON search_log(requested_at);
"#;

pub fn run_migrations(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA_V1)?;
    Ok(())
}
