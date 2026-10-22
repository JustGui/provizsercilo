# Free-search availability, 2026-10-22

Local macOS host, fresh Python 3.12 venv, ddgs **9.16.0**, primp **2.0.1**.
Read-only local Proton gateway access through a task-owned TCP forwarder; no
production access, tunnel rotation, shared-stack restart, or paid search.
The bridge was also served locally on isolated ports 28504 and 28506.

## Measured engine behavior

Three public French news queries (inflation, budget, renewable energy; see
`probe.py`) per engine and route with default primp random impersonation. Each
sample creates a fresh engine. Success means at least one parsed result, **not**
relevance or a verified claim. Raw engine counts precede the bridge's adult
filter. Mean wall time includes failures. The small sample is diagnostic, not an
availability estimate for production's OVH IP.

| Engine | Direct successes | Mean seconds | Local Proton successes | Mean seconds | HTTP statuses / outcome |
|---|---:|---:|---:|---:|---|
| Brave | 0/3 (0%) | 0.32 | 3/3 (100%) | 1.74 | direct 429; proxy 200 |
| Yahoo | 1/3 (33%) | 0.88 | 1/3 (33%) | 1.49 | both 200, often no parsed results |
| DuckDuckGo | 0/3 (0%) | 0.17 | 0/3 (0%) | 1.31 | direct 202; proxy 403 |
| Startpage | 0/3 (0%) | 0.33 | 0/3 (0%) | 1.19 | both 200, no parsed results |
| Yandex | unavailable | — | unavailable | — | disabled by ddgs; no network request |

`measurements-2026-10-22.jsonl` preserves all 60 sample outcomes (engine, route,
query index, counts, status, error category, elapsed time). Yandex rows say
`KeyError`: the diagnostic registry lookup found no enabled engine. The committed
probe reports this as `disabled` explicitly. No query/source/page text, URLs,
credentials or egress IPs are recorded in the measurement artifact.

Single-query controls, one fresh sample per profile and route:

| Engine | Chrome 146 direct/proxy | Firefox 140 direct/proxy | Plain httpx HTTP/1 direct/proxy |
|---|---|---|---|
| Brave | 0/1, 1/1 | 0/1, 1/1 | 0/1, 0/1 (429) |
| Yahoo | 1/1, 0/1 | 1/1, 0/1 | 0/1, 0/1 (500) |
| DuckDuckGo | 0/1, 0/1 | 0/1, 0/1 | 1/1, 1/1 (200, 10 results each) |
| Startpage | 0/1, 0/1 | 0/1, 0/1 | 0/1, 0/1 (200, empty) |

An additional Startpage in-memory structural check found a `captcha` marker on
the direct response; the proxied response had neither that marker nor a result
selector. No response body was retained. HTTP 200 alone is not search success;
challenge pages, engine markup and region behavior need separate investigation.

A subsequent 30-request **bridge HTTP** check (three requests/engine/route,
BACKEND_ORDER empty to isolate each engine, adult filter and breakers enabled)
returned Brave direct 0/3, Proton 2/3; Yahoo direct 1/3, Proton 1/3;
DuckDuckGo/Startpage 0/3 each on both routes; Yandex 503 without network access.
These are user-visible response rates, including open-circuit skips, not 30
independent upstream requests. This second pass illustrates short-term variance.

## Diagnosis and upstream verification

- [ddgs v9.16.0](https://github.com/deedy5/ddgs/releases/tag/v9.16.0) updates Google
  and DuckDuckGo; [primp v2.0.1](https://github.com/deedy5/primp/releases/tag/v2.0.1)
  hardens TLS/H2/proxy/DNS. Both were the latest installed from PyPI in the clean
  venv. Updating alone is not the fix. Runtime requirements pin these measured
  versions to keep engine-registry and client behavior reproducible.
- [ddgs HTTP client](https://github.com/deedy5/ddgs/blob/v9.16.0/ddgs/http_client.py)
  uses random browser/OS impersonation. Installed primp's constructor exposes
  `http2_only=False`, **not an HTTP/2-off switch**. That flag permits HTTP/2;
  setting it false does not force HTTP/1. The httpx control uses `http2=False`
  explicitly, with a plain User-Agent and different TLS fingerprint. Its success
  cannot isolate HTTP version from TLS/header fingerprint effects.
- Brave's same-query direct 429 vs proxied 200 supports an IP/egress reputation
  explanation. DuckDuckGo succeeding with httpx while primp fails on both routes
  supports a fingerprint/protocol-related explanation. Neither proves the cause.
  No `close_notify` failure reproduced locally; production's H2 symptom remains
  unconfirmed and the change treats it as a retriable transport category.
- The enabled text registry in 9.16.0 is brave, duckduckgo, google, grokipedia,
  mojeek, startpage, wikipedia, yahoo. Yandex and Bing text are disabled despite
  broader README engine lists. ddgs silently switches an unavailable backend to
  `auto`, potentially searching unrelated engines. The bridge now skips disabled
  known engines and exposes `enabled=false`, avoiding this hidden fan-out.
- Open issues checked on the measurement date include
  [DuckDuckGo 403 (480)](https://github.com/deedy5/ddgs/issues/480),
  [rate limits not raised distinctly (478)](https://github.com/deedy5/ddgs/issues/478)
  and [result-loss handling (427)](https://github.com/deedy5/ddgs/issues/427).
  No open primp issue specifically matching `close_notify` was found; the search
  matched [the v2 restructure PR](https://github.com/deedy5/primp/pull/160).
  These reports do not establish a library defect behind the production incident.

No automatic httpx scraping fallback is shipped: the single-query control is too
small to justify maintaining another engine/client adapter. HTTP/1 remains a
useful follow-up experiment for DuckDuckGo. Existing filtering stays active.

## Configuration and tests

`DDG_PROXIES=http://vpn-gw-proton:8888,http://vpn-gw-proton-fr:8888` enables a
rotating pool; empty means direct. `BACKEND_PROXIES` takes precedence per engine
or `*`. Supported URL schemes are http, https, socks5 and socks5h; include a port.
A configured pool never silently escapes directly. Requested engines are tried
first, then BACKEND_ORDER (set it empty for engine-isolated diagnosis).

Per-process engine breakers open after all attempted routes fail; proxy breakers
open only for transport/TLS/timeout failures so an engine's empty response or
HTTP block does not suppress the same proxy for other engines. Backoff starts at
5s, doubles with up to 25% jitter and caps at 300s. One request acquires a
half-open probe; success resets it. Breaker state is not shared across replicas.

`DDG_MAX_ATTEMPTS` (default 6) limits route attempts; `DDG_SEARCH_BUDGET_SECONDS`
(default 20) prevents starting further attempts after the soft deadline, and
per-call timeouts are at most 8s or remaining budget. ddgs can make multiple
requests and wait for executor shutdown, so an in-flight call may overrun this
budget. `/health/engines` contains only counters, categorical errors, enabled
flags, opaque proxy IDs and breaker states. `/health` remains liveness only.
Search access logging is disabled in `python main.py`; use `--no-access-log`
when starting uvicorn separately.

```bash
python -m venv /tmp/ddg-bridge-test
/tmp/ddg-bridge-test/bin/pip install -r requirements.txt pytest httpx
/tmp/ddg-bridge-test/bin/pytest -q test_main.py   # 17 passed, no network
# Optional live diagnostic, not a CI test:
/tmp/ddg-bridge-test/bin/python probe.py --proxy http://127.0.0.1:28505
```

RTFC wiring and coordinated exit-IP rotation instructions are documented in
`docs/free-search-egress.md` in the RTFC repository. Proxy egress ships OFF there.
No scheduled rotation: gateways are shared with yt-dlp, watcher and live streams;
an uncoordinated reconnect would interrupt them. Stop/start can reuse the same IP.
