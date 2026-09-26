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
    BACKEND_PROXIES  Per-backend egress, "backend=proxy|fallback,..." ("*" = every
                   other backend). Empty = direct. Proxies are tried in order; the next
                   one is used when a proxy errors or returns nothing. Example:
                   yahoo=http://vpn-gw:8888|http://vpn-gw-proton:8888,*=http://vpn-gw-proton:8888
                   Some engines serve junk (yahoo: porn) or nothing to datacenter IPs.
"""

import os
import re
from typing import Optional

from ddgs import DDGS
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


def _run_backend(q: str, backend: str, kwargs: dict) -> list:
    """Search one backend through its configured egress chain (direct when none).
    Returns cleaned results; raises the last error when every route failed."""
    routes = _BACKEND_PROXIES.get(backend.lower()) or _BACKEND_PROXIES.get("*") or [None]
    last_exc: Optional[Exception] = None
    for proxy in routes:
        try:
            results = _clean(DDGS(timeout=8, proxy=proxy).text(q, **{**kwargs, "backend": backend}) or [])
        except Exception as exc:
            last_exc = exc
            continue
        if results:
            return results
    if last_exc is not None:
        raise last_exc
    return []


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


@app.get("/search")
def search(
    q: str = Query(..., description="Search query"),
    n: int = Query(10, ge=1, le=50, description="Number of results"),
    language: Optional[str] = Query(None, description="ISO 639-1 language code"),
    country: Optional[str] = Query(None, description="ISO 3166-1 alpha-2 country code"),
    region: Optional[str] = Query(None, description="DDG region code override (e.g. 'fr-fr')"),
    safesearch: str = Query(os.getenv("SAFESEARCH", "on")),
    backend: Optional[str] = Query(None, description="DDGS backend: duckduckgo, yahoo, brave, google, yandex, mojeek, startpage"),
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

    if backend:
        # Caller specified a backend — single attempt, no retry.
        try:
            results = _run_backend(q, backend, kwargs)
        except Exception as exc:
            raise HTTPException(status_code=503, detail=str(exc))
        backend_used = backend
    else:
        # Fan-out: try backends sequentially in priority order.
        order = os.getenv("BACKEND_ORDER", _DEFAULT_BACKEND_ORDER)
        backends = [b.strip() for b in order.split(",") if b.strip()]
        results = []
        backend_used = None
        last_err = "No results found."
        for b in backends:
            try:
                results = _run_backend(q, b, kwargs)
                if results:
                    backend_used = b
                    break
            except Exception as exc:
                last_err = str(exc)
                results = []
        if not results:
            raise HTTPException(status_code=503, detail=last_err)

    return {"results": results, "backend_used": backend_used}


if __name__ == "__main__":
    import uvicorn

    port = int(os.getenv("PORT", "8001"))
    uvicorn.run(app, host="0.0.0.0", port=port)
