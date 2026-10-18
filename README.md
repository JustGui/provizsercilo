# ProvizSercilo

Self-hosted Rust HTTP service that acts as a smart search-engine router. Callers POST a single search request; ProvizSercilo selects the best available provider, handles fallbacks, tracks rate limits, and returns unified results.

## Features

- **Provider selection** - scores candidates by rate-limit headroom, language/country profile, and traffic balance
- **Automatic fallback** - cascades through providers on errors, 429s, or empty results
- **Rate-limit tracking** - sliding-window budgets (RPS / RPM / RPD) per API key, reactive cooldowns on error responses
- **Language profiles** - `profiles.toml` routes queries to region-optimal providers
- **Dual storage** - SQLite (default, zero-config) or PostgreSQL (set `DATABASE_URL`)
- **In-memory cache** - configurable TTL via `CACHE_TTL_SECS`
- **Key rotation** - multiple API keys per provider, round-robin with cooldown awareness
- **Admin API** - manage providers and keys at runtime; reload catalog without restart
- **Cost tracking** - per-key CPM (cost per 1000 requests) + currency; query aggregate spend by provider/currency
- **Content enrichment** - `extra_snippets` / `full_content` on `staan`/`tavily` fetch and rerank page content in the same call, no separate scrape step

## Providers

| Slug | Type | Notes |
|------|------|-------|
| `brave` | API key | |
| `tavily` | API key | Supports `full_content` enrichment |
| `mojeek` | API key | |
| `serper` | API key | |
| `exa` | API key | |
| `staan` | API key | Supports `full_content` + `extra_snippets` enrichment |
| `decodo` | API key | Basic-auth token |
| `you-com` | API key | Supports `full_content` enrichment; also in the `/contents` extractor pool |
| `parallel` | API key | Also in the `/contents` extractor pool |
| `deepseek` | API key | Search + Markdown enrichment; Fetch in `/contents`; `DEEPSEEK_API_KEY` or `DEEPSEEK_KEY_1`…`9` |
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
| `DATABASE_URL` | - | PostgreSQL URL (`postgres://user:pass@host/db`). When set, PostgreSQL is used and `DATABASE_PATH` is ignored. |
| `DATABASE_PATH` | `./proviz.db` | SQLite database file (used when `DATABASE_URL` is not set) |
| `PROFILES_PATH` | `./profiles.toml` | Language/country routing config |
| `ADMIN_TOKEN` | - | Required to access `/admin/*` endpoints |
| `SECRETS_DIR` | `/run/secrets` | Directory scanned first when resolving key refs |
| `CACHE_TTL_SECS` | `3600` | Query cache TTL; `0` disables cache |
| `DOC_CACHE_TTL_SECS` | `21600` (6h) | URL-keyed enrichment content cache TTL — separate from the query cache, reused across different queries that surface the same page |
| `PACING_SAFETY` | `0.95` | Effective pacing rate multiplier, finite and >0 through 1; invalid values use the default |
| `AUX_QUEUE_FRACTION` | `0.3` | Fraction of the main queue horizon available to aux requests, 0 up to but excluding 1 (reserves main capacity); invalid values use the default |
| `MAX_FALLBACKS` | `3` | Maximum provider fallback attempts per request |
| `TRANSIENT_FAILURES_BEFORE_COOLDOWN` | `3` | Consecutive transient failures (timeout, network/provider error, 5xx) before a key is put on cooldown; a success resets the count. The first cooldown expires normally; a second consecutive cooldown requires a probe. Empty results and request errors never count. Rate limits, auth and billing refusals start cooldowns at once. `1` = cool down on the first failure |
| `BILLING_COOLDOWN_SECS` | `21600` | Initial parking period for billing/quota refusals before a probe is due |
| `PROBE_QUERY` | `test` | Fixed query used for key probes (`n=1`, no enrichment or user query) |
| `PROBE_INTERVAL_SECS` | `30` | Background scan interval for due probes |
| `PROBE_CONCURRENCY` | `4` | Maximum simultaneous probes, shared by background and admin requests |
| `PROBE_TIMEOUT_MS` | `5000` | Deadline for one probe; only the second and subsequent consecutive probe timeouts increase failure count/backoff |
| `PROBE_BACKOFF_SECS` | `60` | Initial delay after a failed probe; doubles on each failure |
| `PROBE_MAX_BACKOFF_SECS` | `21600` | Cap on failed-probe backoff for billing/auth parking |
| `PROBE_MAX_BACKOFF_TRANSIENT_SECS` | `600` | Cap on failed-probe backoff for transient-origin parking |
| `LOG_LEVEL` | `INFO` | `TRACE` \| `DEBUG` \| `INFO` \| `WARN` \| `ERROR` |
| `LOG_FORMAT` | `json` | `json` \| `pretty` |

Per-key limits (`rps_limit`, `rpm_limit`, `rpd_limit` on an API key) are enforced: a key whose configured budget is used up is skipped (`budget_exhausted` in the fallback chain) instead of being sent a request the provider would answer with a 429. `rpd_limit` is therefore a hard daily cap - useful on an expensive last-resort provider. Budgets are counted in memory, so a restart starts them over.

### Storage backends

ProvizSercilo supports two storage backends with the same schema and behaviour:

- **SQLite** (default) — zero dependencies, ideal for single-node or development use. Configured via `DATABASE_PATH`.
- **PostgreSQL** — set `DATABASE_URL=postgres://…` to connect to an existing PostgreSQL instance. The schema is created automatically on first start.

### Per-key pacing

`max_queue_ms BIGINT` on `ps_api_keys` (SQLite: `api_keys`) opts a key into process-local pacing. NULL, zero or negative values preserve the immediate `budget_exhausted` skip. Both storage backends migrate existing databases at startup. Create/update key admin requests accept `max_queue_ms`; set it to `0` to disable queueing. After changing SQL directly, call authenticated `POST /admin/reload` (or restart) to refresh the catalog:

```sql
UPDATE ps_api_keys SET max_queue_ms=3000 WHERE label='staan-1';
```

The per-key clock uses the tightest positive RPS or RPM limit, multiplied by `PACING_SAFETY`. For Staan at 20 RPS, the default is 19 requests/s. A request atomically reserves a slot only if its wait fits the queue horizon and the remaining `timeout_ms`, leaving a 50 ms response margin. Overflow is `budget_exhausted` and continues to the next candidate/tier without consuming `MAX_FALLBACKS`. Provider cooldowns and daily budgets remain enforced. Retries also reserve slots; `timeout_ms` bounds the complete search, including waits and retries.

`priority: "main"` (default, also used for unknown strings) may reserve the full horizon. `priority: "aux"` may reserve only `AUX_QUEUE_FRACTION` of it: 900 ms for a 3000 ms key at the default fraction. Reservations keep their admitted order. Aux floods cannot fill the longer main horizon, so main requests retain capacity under pressure. At 19/s, a 3000 ms horizon admits 58 simultaneous requests including the immediate slot; all 60 require at least 3106 ms (for example 3200 ms). At a multiplier of 1, all 60 fit in 3000 ms.

`/health` key entries expose `queue_depth` (currently waiting requests), `next_slot_delay_ms`, and cumulative `queued`, `wait_ms` (sum of reserved waits) and `overflow`. Queue logs contain key IDs, priority and counts/durations, never query text. Cancellation clears live queue depth but does not return the slot: its short unused interval expires naturally, with no permanent gap. These clocks/counters reset at restart. Production currently has one replica; scaling to multiple replicas requires shared pacing state. Probes still respect sliding-window budgets and do not join the user queue.

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

Optional `priority` (`"main"` or `"aux"`, default `"main"`) controls the pacing horizon. Optional `fallback_on_empty` defaults to `true`. With `false`, an empty provider answer returns HTTP 200 with `results: []`, the selected provider in `meta.provider`, and a `<provider>:empty` fallback-chain entry. Provider errors (timeouts, 5xx, auth, billing, rate limits) retain fallback behavior. Empty-result policy partitions the SERP cache, so it cannot reuse an answer from the other policy. Existing requests keep their cache keys.

#### Content enrichment

Optional fields — fetch and rerank result pages in the same call instead of scraping separately. Only providers that support the requested field are tried (see the Providers table); the request 503s if none in the group qualify.

```json
{
  "query": "vector database comparison",
  "extra_snippets": true,       // semantically scored chunks; staan only
  "full_content": "markdown",   // "markdown" | "html" | "text"; staan + tavily + you-com + deepseek
  "max_snippets": 5,            // extra_snippets only, default 3
  "min_score": 0.2,             // extra_snippets only, default 0.1
  "include_domains": ["qdrant.tech", "weaviate.io"],
  "exclude_domains": []
}
```

Each result gains `full_content: {text, format, length}` and/or `extra_snippets: [{chunk, score}]` when populated. A page's enriched content is cached by URL (`DOC_CACHE_TTL_SECS`) independently of the query, so a different query surfacing the same page doesn't re-fetch it.

#### Domain and date restriction

```json
{
  "query": "taux de chômage des jeunes",
  "include_domains": ["insee.fr", "dares.travail-emploi.gouv.fr", "lemonde.fr/les-decodeurs"],
  "exclude_domains": [],
  "freshness": "year",          // "day" | "week" | "month" | "year"
  "date_from": "2026-01-01",    // optional explicit range, YYYY-MM-DD
  "date_to": "2026-10-10"
}
```

`include_domains` works on every provider: engines without a native field (SearXNG, the DDG bridge, Brave, Mojeek, Serper, Exa) get it as OR-ed `site:` operators (at most 6) and every answer is filtered to the domains. Freshness is mapped per provider. See [docs/search-operators.md](docs/search-operators.md) for the mapping and the measured operator support of each free engine.

### DeepSeek Search & Fetch

[API documentation](https://api-docs.deepseek.com/guides/search/). Export `DEEPSEEK_API_KEY` (or numbered `DEEPSEEK_KEY_1`…`9`) before starting the server; startup seeds the `deepseek` provider and key references. The server reads exported environment variables; Docker Compose loads `.env`.

Search uses `/v1/tools/search`, caps `n` at 50, maps `language` to `lang:` and freshness to `after:`/`before:`. Domain filters use the router’s existing `site:` operators and result filtering. The final query, including operators, must fit 128 Unicode characters; longer or blank queries return a request error. DeepSeek translates the request’s `country` through the server’s optional `DEEPSEEK_LOCALIZATION_IPS` map (see below).

`full_content` enables `need_content`; missing bodies remain absent. Fetch uses `/v1/tools/fetch`, one URL per upstream request. Both return Markdown, stamped as `markdown` even if another format was requested. Fetch 403/404 responses are per-page misses eligible for fallback. Search and Fetch each have an upstream limit of 5 QPS; new keys seed a 5 RPS limit and a 3-second search queue before fallback. The adapter spaces calls by at least 210 ms across its keys, with independent Search and Fetch gates. Limits are account-wide, so deployments sharing an account must coordinate their combined traffic. Seeded cost is $0.75 per 1,000 calls; existing request accounting is an estimate; `/contents` does not persist per-call costs.

For a live comparison with Staan, export both keys and run:

```bash
cargo run -p providers --example compare_deepseek_staan -- "Bardella est accusé d'antisémitisme"
```

Add `--full-content` after the query to compare enrichment too, or `--localization 86.196.134.124` to pass an explicit DeepSeek localization IP. The explicit IP option overrides the country map for that example run. This prints normalized SERPs, timings, exact and fragment-free URL overlap, and a DeepSeek Fetch smoke test without printing credentials. See the [French news comparison report](docs/deepseek-staan-comparison-2026-10-18.md) for measured results and limitations.

### Country normalization for DeepSeek

Clients keep using the shared API, for example:

```json
{"query":"Bardella est accusé d'antisémitisme","language":"fr","country":"fr"}
```

Bundled defaults cover 250 countries and territories, so `country` works without additional configuration. Override individual IP hints on the server when needed:

```sh
DEEPSEEK_LOCALIZATION_IPS='{"fr":"86.196.134.124"}'
```

DeepSeek then receives `lang:fr` and `localization: "86.196.134.124"`. Country keys are trimmed and case-insensitive two-letter codes; use ISO 3166-1 alpha-2 codes such as `fr`, `gb`, and `us`. IPv4 and IPv6 addresses are accepted. Invalid JSON, invalid IPs, malformed country keys or duplicate normalized country keys prevent startup. Restart after changing the mapping. Resolution is **configured country override → bundled country IP → omit localization**. Overrides merge into the defaults. The bundled snapshot is generated from the PDDL-licensed `sapics/ip-location-db` user-country dataset; its provenance, checksum and refresh procedure are documented in [the data README](crates/providers/data/README.md). Geolocation can change or differ from DeepSeek’s own database. This is a ranking hint, not a geographic restriction or a proxy connection.

An absent or unknown country omits `localization`; it never borrows another country’s IP or infers a country from language. The existing cache already partitions results by country. For direct adapter callers, `with_localization` overrides the map; the HTTP API continues to expose only the existing `country` field.

Current adapter mappings (country remains a best-effort hint across fallback providers):

| Providers | Mapping of `country` |
| --- | --- |
| Brave, Mojeek, You.com | `country` |
| Serper, Decodo | `gl` |
| DDG bridge backends | country combined with language into a ddgs region |
| Staan | language/country mapped to `fr-fr`, `en-us`, or `de-de` only |
| DeepSeek | country override or bundled IP → `localization` |
| Exa, Tavily, Parallel, SearXNG | not currently mapped by these adapters |


### Page content extraction

Standalone endpoint for pulling clean page bodies — hand it a batch of URLs, it
routes them through a pool of extraction-capable providers (You.com, Parallel, DeepSeek)
with per-URL fallback, and reuses the same URL-keyed cache the search enrichment
path fills.

```
POST /contents
Content-Type: application/json

{
  "urls": ["https://example.com", "https://www.rust-lang.org"],  // capped at 50
  "format": "markdown",   // "markdown" (default) | "html" | "text"
  "objective": "…",       // optional focus hint (Parallel)
  "group": "extract",     // optional — scope/prioritize the extractor pool
  "fresh": false          // true bypasses the URL cache
}
```

Returns `docs[]` (one per URL, in order) with `content: {text, format, length}`
and `source` (provider slug or `"cache"`), or an `error` string when no provider
could fetch it, plus `meta` and `attempts`.

### Admin

All admin endpoints require `Authorization: Bearer <ADMIN_TOKEN>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/providers` | List providers |
| `POST` | `/admin/providers` | Create provider |
| `GET` | `/admin/providers/:slug/keys` | List key refs for a provider |
| `POST` | `/admin/providers/:slug/keys` | Add a key ref |
| `DELETE` | `/admin/providers/:slug/keys/:id` | Remove a key |
| `PATCH` | `/admin/keys/:id` | Update a key (limits, active state, cost) |
| `POST` | `/admin/reload` | Reload in-memory catalog from DB |
| `GET` | `/admin/stats` | Rate-limit and usage snapshot |
| `GET` | `/admin/keys/parked` | Parked key IDs, reason, UTC `since`, retry delay, probe failures and running state |
| `POST` | `/admin/keys/:id/probe` | Probe an active key immediately, including before its cooldown expires |
| `GET` | `/admin/costs?window_secs=` | Spend by provider/currency over the window (default 30d) |

#### Key failures and recovery

Empty answers remain `empty` attempts in the fallback chain and the next candidate
is tried. They never cool down a key and reset its transient-failure count.
A provider 400 identifying an invalid parameter, or a 422 validation response,
stops the search immediately: the caller receives HTTP 422 with the provider
message. Other providers are **not tried** for that request, avoiding a fallback
storm for invalid input. The key remains eligible for other requests.

Billing refusals (402, 433, and credit/quota/payment messages in 400/403 bodies)
park the key with reason `billing`, emit a WARN and a `billing` rate event.
401/other 403 responses park it as `auth`. If the key had a successful user call
within `AUTH_RECENT_SUCCESS_SECS` (default 600) and no previous auth episode began within
`AUTH_REPEAT_WINDOW_SECS` (default 600), the first auth cooldown is
`AUTH_FIRST_COOLDOWN_SECS` (default 30); otherwise it is the full 300 seconds.
Both auth cooldowns require a successful probe, due at expiry. User successes
while parked do not release the key. Further in-flight auth errors in the same
episode neither extend its deadline nor refresh the repeat window. An auth
error after release starts a new episode and can trigger the full cooldown.
Failed probes retain their normal backoff. User successes
and successful probes do not clear the auth repeat window; probe successes do
not count as successful user calls. Auth/billing HTTP failures log the numeric
provider status only, without response bodies or credentials.
Rate-limit cooldowns (`rps`: 5 s,
`rpm`: 60 s, `rpd`: 3600 s) release on expiry without a probe. Transient failures
start a cooldown after `TRANSIENT_FAILURES_BEFORE_COOLDOWN` consecutive failures:
the first timeout/error cooldown releases on expiry (30 s / 60 s). A second
consecutive transient cooldown requires a successful probe before reuse, as do
billing and auth cooldowns. Concurrent failures during one cooldown do not count
as additional cooldown episodes. A successful user request, including an empty
answer, resets the transient failure and cooldown history; a late user success
does not release an already probe-gated key.

For probe-gated keys, expiry makes a background probe due. A decoded empty answer
counts as a successful probe, since it confirms that the API accepted the call.
Failed probes use exponential backoff, capped by `PROBE_MAX_BACKOFF_SECS` for
billing/auth and `PROBE_MAX_BACKOFF_TRANSIENT_SECS` for transient-origin parking.
A billing/auth refusal during a probe switches to billing/auth parking. The first
consecutive probe timeout leaves the failure count and deadline unchanged, allowing
a retry on the next scan when due; only the second and subsequent consecutive
timeouts increase backoff. Any non-timeout response resets the timeout streak.
Probes respect
configured key budgets but do not create user-search log rows or search metrics.
Their response bodies and resolved credentials are never logged.

`GET /admin/keys/parked` returns entries such as:

```json
[{"key_id":"…","reason":"billing","since":"2026-10-16T08:00:00Z","retry_after_secs":21600,"probe_failures":0,"probing":false}]
```

After replenishing credits or repairing credentials, use
`POST /admin/keys/<id>/probe` to test immediately. The response includes `success`
and `error_type`; an unsuccessful probe returns `success: false`. Probe-gated keys
stay parked until a probe succeeds.
Overlapping probes or a full probe pool return 409; inactive/missing keys return
404. Both endpoints use the existing admin bearer authentication.

Failed searches write `success=false`, `error_type` and `fallback_chain` to the
existing search log (`ps_search_log` on PostgreSQL). Audit rows contain the query
hash, never the query text or provider error body. A `no_providers_available` 503
includes `Retry-After` in seconds: the shortest remaining candidate cooldown,
rounded up, or 1 when no cooldown deadline is available. A due probe-gated key
still requires a probe, so the hint is not a guarantee of availability.

Cooldowns, parking state and probe backoff are in memory, like rate budgets; a
restart clears them, including recent user-success and auth-error history.
All probe timing/concurrency and auth timing environment values must be
positive; zero is clamped to 1. The Brave client decodes gzip responses using
reqwest's gzip support and caps its result count at Brave's documented maximum
of 20; local compressed/uncompressed documentation fixtures cover this path.

#### Cost tracking

Each API key can carry a subscription cost, so a provider with two keys on two different plans (or billed in different currencies) is tracked separately.

Set it when creating or updating a key:

```bash
curl -X PATCH localhost:8090/admin/keys/<id> \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"cost_per_mille": 4.0, "currency": "USD"}'
```

`cost_per_mille` = cost per 1000 requests, in `currency` (ISO 4217, e.g. `"USD"`, `"EUR"`). Every successful search snapshots `cost = cost_per_mille / 1000` onto its `search_log` row at request time, so later CPM edits never retroactively change historical totals. Cache hits cost nothing (no upstream call made).

Read it back:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  "localhost:8090/admin/costs?window_secs=2592000"
```

```json
[
  {"provider": "tavily", "currency": "USD", "total_cost": 12.4, "requests": 3100},
  {"provider": "staan",  "currency": "EUR", "total_cost": 3.2,  "requests": 800}
]
```

Rows are grouped by `(provider, currency)` — spend in different currencies is never summed together.

## Workspace layout

```
Cargo.toml                  workspace root
crates/
  core/                     models, rate_limit, selector, language_profile, key_resolver, StorageBackend trait
  storage-sqlite/           rusqlite storage layer (implements StorageBackend)
  storage-postgres/         sqlx/PostgreSQL storage layer (implements StorageBackend)
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
