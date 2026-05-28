# ProvizSercilo

Self-hosted Rust HTTP service that acts as a smart search-engine router. Callers POST a single search request; ProvizSercilo selects the best available provider, handles fallbacks, tracks rate limits, and returns unified results.

## Features

- **Provider selection** — scores candidates by rate-limit headroom, language/country profile, and traffic balance
- **Automatic fallback** — cascades through providers on errors, 429s, or empty results
- **Rate-limit tracking** — sliding-window budgets (RPS / RPM / RPD) per API key, reactive cooldowns on error responses
- **Language profiles** — `profiles.toml` routes queries to region-optimal providers
- **In-memory cache** — configurable TTL via `CACHE_TTL_SECS`
- **Key rotation** — multiple API keys per provider, round-robin with cooldown awareness
- **Admin API** — manage providers and keys at runtime; reload catalog without restart

## Providers

| Slug | Type | Notes |
|------|------|-------|
| `brave` | API key | |
| `tavily` | API key | |
| `mojeek` | API key | |
| `serper` | API key | |
| `searxng` | URL | Self-hosted; supports multiple instances |
| `ddg` | URL | Requires the included Python bridge |

## Quick start

```bash
cp .env.example .env
# Fill in API keys in .env

cargo build
LOG_FORMAT=pretty cargo run --bin proviz-sercilo
```

The server listens on `PORT` (default `8090`).

## Docker

```bash
docker compose up --build
```

Brings up ProvizSercilo, SearXNG, and the DDG bridge together.

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `8090` | Listen port |
| `DATABASE_PATH` | `./proviz.db` | SQLite database file |
| `PROFILES_PATH` | `./profiles.toml` | Language/country routing config |
| `ADMIN_TOKEN` | — | Required to access `/admin/*` endpoints |
| `SECRETS_DIR` | `/run/secrets` | Directory scanned first when resolving key refs |
| `CACHE_TTL_SECS` | `3600` | Query cache TTL; `0` disables cache |
| `MAX_FALLBACKS` | `3` | Maximum provider fallback attempts per request |
| `LOG_LEVEL` | `INFO` | `TRACE` \| `DEBUG` \| `INFO` \| `WARN` \| `ERROR` |
| `LOG_FORMAT` | `json` | `json` \| `pretty` |

API key values are **never stored in the database**. The `key_ref` column holds an env-var name (e.g. `BRAVE_KEY_1`); the actual value is resolved at search time from the environment or `SECRETS_DIR`.

## API

### Search

```
POST /search
Content-Type: application/json

{
  "query": "open source LLMs",
  "language": "en",       // optional ISO 639-1
  "country": "us"         // optional ISO 3166-1 alpha-2
}
```

### Admin

All admin endpoints require `Authorization: Bearer <ADMIN_TOKEN>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/providers` | List providers |
| `POST` | `/admin/providers` | Create provider |
| `GET` | `/admin/providers/:slug/keys` | List key refs for a provider |
| `POST` | `/admin/providers/:slug/keys` | Add a key ref |
| `DELETE` | `/admin/providers/:slug/keys/:id` | Remove a key |
| `POST` | `/admin/reload` | Reload in-memory catalog from DB |
| `GET` | `/admin/stats` | Rate-limit and usage snapshot |

## Workspace layout

```
Cargo.toml                  workspace root
crates/
  core/                     models, rate_limit, selector, language_profile, key_resolver
  storage-sqlite/           rusqlite storage layer
  providers/                SearchProvider trait + adapters
  cache/                    in-memory DashMap query cache
server/                     Axum HTTP server, main binary
bridges/
  ddgs-bridge/              Python FastAPI wrapper for duckduckgo-search
profiles.toml               language/country routing config
docker-compose.yml          full stack
```

## Development

```bash
# Run tests and lints (also enforced by the pre-push hook)
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check

# Install the pre-push hook
bash scripts/install-hooks.sh
```

## Adding a provider

1. Create `crates/providers/src/<slug>.rs` implementing `SearchProvider`
2. Register it in `crates/providers/src/lib.rs`
3. Add it to `build_providers()` in `server/src/app.rs`
4. Insert a row via `POST /admin/providers`
5. Add at least one key ref via `POST /admin/providers/:slug/keys`

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

```
Copyright 2024 ProvizSercilo Contributors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```
