"""
DDG Bridge - thin FastAPI wrapper around duckduckgo-search.

Called by ProvizSercilo as a standard HTTP provider.
The Rust service treats this bridge like any other provider:
  - key_ref in the DB resolves to this service's base URL (e.g. "http://localhost:8001")
  - Rate limiting, cooldowns, and fallback chain all apply normally.

Usage:
    pip install -r requirements.txt
    uvicorn main:app --host 0.0.0.0 --port 8001

Optional env vars:
    PORT           Listen port (default: 8001)
    MAX_RESULTS    Default max results (default: 10)
    SAFESEARCH     on/moderate/off (default: on). Several ddgs engines ignore it
                   (yahoo, mojeek, yandex, startpage), hence the adult filter below.
    BACKEND_ORDER  Comma-separated backend priority when no backend is requested
                   (default: yandex,startpage,yahoo,duckduckgo,brave)
    DDG_PROXIES    Optional comma-separated rotating HTTP/SOCKS egress pool.
    DDG_MAX_ATTEMPTS  Maximum route attempts per search (default: 6).
    DDG_SEARCH_BUDGET_SECONDS  Soft request budget (default: 20).
    BACKEND_PROXIES  Per-backend egress, "backend=proxy|fallback,..." ("*" = every
                   other backend). Empty = direct. Proxies are tried in order; the next
                   one is used when a proxy errors or returns nothing. Example:
                   yahoo=http://vpn-gw:8888|http://vpn-gw-proton:8888,*=http://vpn-gw-proton:8888
                   Some engines serve junk (yahoo: porn) or nothing to datacenter IPs.
"""

import os
import logging
import time
import re
from typing import Optional

from ddgs.engines import ENGINES as DDGS_ENGINES
from ddgs import DDGS
from resilience import ENGINES, Scheduler, error_code, proxy_pool

# DDGS logs raw exception strings (including request URLs) at INFO.
logging.getLogger("ddgs").setLevel(logging.CRITICAL)
from fastapi import FastAPI, HTTPException, Query

_DEFAULT_BACKEND_ORDER = "yandex,startpage,yahoo,duckduckgo,brave"

app = FastAPI(title="DDG Bridge", version="0.3.0")

# --- Adult-content filter ---------------------------------------------------
# Incident 2026-09-26: the yahoo backend answered questions about French social
# housing with xnxx/xhamster/pornhub pages only (the ddgs yahoo engine ignores
# `safesearch`). A result list is dropped result by result; a backend whose
# answer is entirely adult counts as empty, so the next backend (fan-out) or the
# next proviz-sercilo provider is tried. Kept in step with rtfc-core `adult.rs`.
_ADULT_DOMAIN_MARKERS = (
    "porn", "xxx", "xnxx", "xvideo", "xhamster", "redtube", "youporn", "bokep", "hentai",
    "spankbang", "eporner", "chaturbate", "stripchat", "brazzers", "tube8", "beeg", "motherless",
)
# Pornic / Pornichet (French towns), jeuxvideo.com ("jeu-xvideo").
_ADULT_DOMAIN_EXCEPTIONS = ("pornic", "jeuxvideo")
_ADULT_ALLOWED_PARENTS = ("fandom.com",)  # xxx.fandom.com = the film's wiki
_ADULT_TLDS = {"xxx", "porn", "sex", "adult"}
_ADULT_TEXT_DECISIVE = {"xnxx", "xvideos", "xvideo", "xhamster", "youporn", "redtube", "bokep", "ngentot"}
_ADULT_TEXT_EXPLICIT = {
    "porn", "porno", "xxx", "pussy", "horny", "milf", "cumshot", "creampie", "blowjob",
    "pornstar", "pornstars", "stepmom", "stepsister", "stepbrother", "hentai", "memek",
}
_WORD = re.compile(r"\w+", re.UNICODE)


def _host(url: str) -> str:
    host = url.split("://", 1)[-1].split("/", 1)[0].split("?", 1)[0].split("#", 1)[0]
    host = host.rsplit("@", 1)[-1].split(":", 1)[0].lower()
    return host[4:] if host.startswith("www.") else host


def is_adult(url: str, title: str, snippet: str) -> bool:
    host = _host(url)
    if any(host == p or host.endswith("." + p) for p in _ADULT_ALLOWED_PARENTS):
        return False
    labels = [label for label in host.split(".") if label]
    if len(labels) >= 2 and labels[-1] in _ADULT_TLDS:
        return True
    for label in labels:
        if label.startswith("xn--") or any(e in label for e in _ADULT_DOMAIN_EXCEPTIONS):
            continue
        if any(m in label for m in _ADULT_DOMAIN_MARKERS):
            return True
    words = set(_WORD.findall(f"{title} {snippet}".lower()))
    return bool(words & _ADULT_TEXT_DECISIVE) or len(words & _ADULT_TEXT_EXPLICIT) >= 2


def _parse_proxies(spec: str) -> dict:
    """"yahoo=http://a|http://b,*=http://c" -> {"yahoo": ["http://a", "http://b"], "*": ["http://c"]}."""
    out: dict = {}
    for part in spec.split(","):
        name, sep, value = part.partition("=")
        if not sep or not name.strip():
            continue
        proxies = [p.strip() for p in value.split("|") if p.strip()]
        if proxies:
            out[name.strip().lower()] = proxies
    return out


_BACKEND_PROXIES = _parse_proxies(os.getenv("BACKEND_PROXIES", ""))
for _routes in _BACKEND_PROXIES.values():
    proxy_pool(",".join(_routes))  # fail fast without echoing credentials
_SCHEDULER = Scheduler(proxy_pool(os.getenv("DDG_PROXIES", "")), _BACKEND_PROXIES)
_MAX_ATTEMPTS = max(1, int(os.getenv("DDG_MAX_ATTEMPTS", "6")))
_BUDGET = max(1, float(os.getenv("DDG_SEARCH_BUDGET_SECONDS", "20")))


def _run_backend(q, backend, kwargs, offset, deadline, remaining):
    """Try each healthy route once. No direct escape from a configured pool."""
    if backend not in DDGS_ENGINES["text"]:
        return [], 0
    if not _SCHEDULER.acquire_engine(backend):
        return [], 0
    attempts, last_error = 0, 'empty'
    try:
        for proxy in _SCHEDULER.routes(backend, offset):
            if attempts >= remaining or time.monotonic() >= deadline:
                break
            if not _SCHEDULER.acquire_proxy(proxy):
                continue
            attempts += 1
            try:
                timeout = max(.1, min(8, deadline - time.monotonic()))
                results = _clean(DDGS(timeout=timeout, proxy=proxy).text(
                    q, **{**kwargs, "backend": backend}) or [])
                error = None if results else 'empty'
            except Exception as exc:
                error, results = error_code(exc), []
            # An engine block/empty response is not proof of a broken proxy.
            # It still counts as a failed proxy attempt, but only transport
            # failures open the shared proxy circuit.
            if proxy is not None and error not in (None, 'tls_h2', 'timeout', 'connection'):
                with _SCHEDULER.lock:
                    p = _SCHEDULER.proxies[proxy]
                    p.probing = False
                    p.failures = 0
                    p.retry_at = 0
                    p.last_error = error
            else:
                _SCHEDULER.finish_proxy(proxy, error)
            if results:
                last_error = None
                return results, attempts
            last_error = error
        return [], attempts
    finally:
        _SCHEDULER.finish_engine(backend, last_error, attempted=bool(attempts))


def _clean(raw: list) -> list:
    out = []
    for r in raw:
        url = r.get("href") or r.get("url", "")
        if not url:
            continue
        title = r.get("title", "")
        snippet = r.get("body") or r.get("snippet", "")
        if is_adult(url, title, snippet):
            continue
        out.append({"url": url, "title": title, "snippet": snippet})
    return out


def _ddg_region(language: Optional[str], country: Optional[str]) -> str:
    if not language:
        return "wt-wt"
    lang = language.lower()
    if country:
        return f"{lang}-{country.lower()}"
    return "wt-wt" if lang == "en" else f"{lang}-{lang}"


@app.get("/health")
def health():
    return {"status": "ok"}


@app.get("/health/engines")
def engine_health():
    health = _SCHEDULER.health()
    for engine, stats in health["engines"].items():
        stats["enabled"] = engine in DDGS_ENGINES["text"]
    return health


@app.get("/search")
def search(
    q: str = Query(..., description="Search query"),
    n: int = Query(10, ge=1, le=50, description="Number of results"),
    language: Optional[str] = Query(None, description="ISO 639-1 language code"),
    country: Optional[str] = Query(None, description="ISO 3166-1 alpha-2 country code"),
    region: Optional[str] = Query(None, description="DDG region code override (e.g. 'fr-fr')"),
    safesearch: str = Query(os.getenv("SAFESEARCH", "on")),
    backend: Optional[str] = Query(None, description="DDGS backend: duckduckgo, yahoo, brave, google, yandex, mojeek, startpage"),
    timelimit: Optional[str] = Query(None, description="Publication window: d, w, m or y (ignored by backends without a date filter)"),
):
    """
    Execute a DDG web search and return normalised results.

    When `backend` is omitted, tries all backends sequentially in BACKEND_ORDER
    until one returns results, and reports which one succeeded in `backend_used`.

    Returns:
        { "results": [...], "backend_used": "yandex" }
    """
    if not q.strip():
        raise HTTPException(status_code=400, detail="Query cannot be empty")

    kwargs: dict = {
        "max_results": n,
        "safesearch": safesearch,
        "region": region if region else _ddg_region(language, country),
    }
    if timelimit in ("d", "w", "m", "y"):
        kwargs["timelimit"] = timelimit

    order = [b.strip().lower() for b in os.getenv("BACKEND_ORDER", _DEFAULT_BACKEND_ORDER).split(",") if b.strip()]
    requested = [b.strip().lower() for b in backend.split(",") if b.strip()] if backend else []
    backends = list(dict.fromkeys(requested + order))
    if not backends or any(b not in ENGINES for b in backends):
        raise HTTPException(status_code=400, detail="Unsupported search backend")
    deadline = time.monotonic() + _BUDGET
    offset, attempts = _SCHEDULER.offset(), 0
    for b in backends:
        results, used = _run_backend(q, b, kwargs, offset, deadline, _MAX_ATTEMPTS - attempts)
        attempts += used
        if results:
            backend_used = b
            break
        if attempts >= _MAX_ATTEMPTS or time.monotonic() >= deadline:
            break
    else:
        results = []
    if not results:
        raise HTTPException(status_code=503, detail="Search routes unavailable")

    return {"results": results, "backend_used": backend_used}


if __name__ == "__main__":
    import uvicorn

    port = int(os.getenv("PORT", "8001"))
    uvicorn.run(app, host="0.0.0.0", port=port, access_log=False)
