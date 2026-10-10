# Search operators on the free engines

What the free engines behind proviz-sercilo do with query operators, and how the
`/search` request fields map onto them. Measured 2026-10-10 for JustGui/rtfc#284.

## Request fields

| Field | Meaning | Engines without native support |
|---|---|---|
| `include_domains` | Only results from these domains. An entry may carry a path (`lemonde.fr/les-decodeurs`). | Rewritten into `site:` operators in the query text (see below). |
| `exclude_domains` | No results from these domains. | Rewritten into `-site:` operators, only when `include_domains` is empty. |
| `freshness` | `day` / `week` / `month` / `year`: published in that window. | Ignored. |
| `date_from`, `date_to` | `YYYY-MM-DD` publication range (either bound optional). | Window-only engines get the smallest window covering the range, and nothing for a range that ends in the past or starts more than a year ago. |

Every answer to a domain-restricted request is filtered to the requested domains (host,
subdomains, path prefix). An answer that had results but none inside the domains counts as
`empty`, so the fallback chain moves to the next provider. Restricted and dated requests
have their own SERP cache entries.

### `site:` rewriting (`crates/providers/src/operators.rs`)

- One entry: `query site:insee.fr` (a path is kept: `site:lemonde.fr/les-decodeurs`).
- Several: `query (site:a.fr OR site:b.fr ...)`, at most 6 (`MAX_SITE_OPERATORS`), hosts
  only - a path inside an OR list makes Yahoo answer nothing; the result filter still
  enforces the path.
- `*.gouv.fr`, `https://www.x.fr/` and `site:x.fr` entries are normalised (`gouv.fr`,
  `x.fr`); `site:gouv.fr` already matches every subdomain.
- A query that already has a `site:` operator is sent as written.
- Providers with a native field (`supports_include_domains`: staan, tavily, you.com) get
  the bare hosts in that field and the query unchanged.

### Freshness per provider

| Provider | Parameter sent |
|---|---|
| SearXNG | `time_range=day|week|month|year` (window only) |
| DDG bridge | `timelimit=d|w|m|y`, passed to ddgs (window only) |
| Brave API | `freshness=pd|pw|pm|py`, or `YYYY-MM-DDtoYYYY-MM-DD` |
| Serper | `tbs=qdr:d|w|m|y`, or `cdr:1,cd_min:M/D/YYYY,cd_max:M/D/YYYY` |
| Exa | `startPublishedDate` / `endPublishedDate` |
| Tavily | `time_range`, or `start_date` / `end_date` |
| you.com | `freshness=day|week|month|year`, or `YYYY-MM-DDtoYYYY-MM-DD` |
| Mojeek, staan, decodo, parallel | not mapped (no documented date parameter) |

## Measured operator support

Method: a private SearXNG (this repo's `searxng/settings.yml`, without the Tor proxy) and a
private ddg-bridge container, from a residential dev box without the production VPN
egress. Per engine, French queries; a cell is the share of the top 10 results that honour
the operator (`site:` = on the domain, quote = phrase in title/snippet, `-term` = term
absent from the URL, `filetype:pdf` = `.pdf` URL, `intitle:` = word in title). Freshness:
share of results that changed when the window was set to one week. `0 res.` = the engine
answered nothing (operator rejected, or nothing matched). `blocked` = the engine refused
the dev box (CAPTCHA, 429, or no answer even to a plain query).

| Engine | `site:` | `(site: OR site:)` | `site: OR site:` | `site:` parent (`gouv.fr`) | `site:` with path | `"phrase"` | `-term` | `filetype:pdf` | `intitle:` | date window |
|---|---|---|---|---|---|---|---|---|---|---|
| SearXNG default mix (Google CSE answering) | 1.0 | 1.0 | 1.0 | 1.0 | 0 res. | 1.0 | 1.0 | 1.0 | 0.9 | 0.9 changed (`time_range`) |
| SearXNG `google cse` | 1.0 | 1.0 | 1.0 | 1.0 | 0 res. | 1.0 | 1.0 | blocked (429) | blocked | blocked |
| SearXNG `duckduckgo` | 1.0 | blocked (CAPTCHA after one query) | | | | | | | | |
| SearXNG `brave` | blocked (429) | | | | | | | | | |
| ddgs `yahoo` | 1.0 | 0 res. / 1.0 (7 res. on two other queries) | 1.0 | 1.0 | 1.0 (alone); 0 res. inside an OR list | 1.0 (6 res.) | 1.0 | 0 res. | 0 res. | 0.86 changed (`timelimit`) |
| ddgs `yandex` | 1.0 | 1.0 | 1.0 | 0 res. | 0 res. | 0 res. | 1.0 | 0 res. | 0.86 | 0 res. |
| ddgs `duckduckgo`, `google`, `brave`, `mojeek`, `startpage` | blocked: no answer to a plain query from the dev box (production reaches them through the VPN gateways) | | | | | | | | | |
| Brave API, Mojeek API | not measured (paid keys). Brave documents `site:`, quotes, `-`, `filetype:` and `freshness`; Mojeek documents `site:` and quotes. | | | | | | | | | |

Reading:

- `site:` single and OR-ed works on every engine that answered; paths work alone on
  Yahoo but not on Google CSE or Yandex for the tested query, and break an OR list -
  hence hosts-only lists plus the result filter.
- Six OR-ed `site:` terms still returned restricted results (Yahoo: `taux de croissance
  France 1962 (site:insee.fr OR site:gouv.fr OR site:ccomptes.fr OR site:vie-publique.fr
  OR site:oecd.org OR site:banque-france.fr)` -> INSEE, OECD "Études économiques: France
  1962", observatoire-des-territoires.gouv.fr ...). The cap is 6.
- Quotes and `-term` are safe; quoting a phrase nobody wrote returns nothing, so callers
  should retry unquoted (RTFC does).
- `filetype:pdf` and `intitle:` only reach Google-backed engines; elsewhere the query
  becomes empty rather than ignored. Use them only in a search that is allowed to fail.
- Date windows: SearXNG `time_range` and ddgs `timelimit` change the answer on the
  engines that support them; Yandex answered nothing with a window.
- Rate limits dominate on the free tier: Google CSE, DuckDuckGo and Brave throttled the
  dev box after a few dozen queries. Measurements from the production egress may differ.

Reproduce: `docker run` the `searxng` and `ddg-bridge` images on spare ports, then query
`/search?format=json&engines=<engine>&q=...` (SearXNG) or `ddgs.DDGS().text(q,
backend=..., timelimit=...)` inside the bridge container. Free engines only.
