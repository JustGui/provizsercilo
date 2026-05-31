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
    SAFESEARCH     safe/moderate/off (default: moderate)
    BACKEND_ORDER  Comma-separated backend priority when no backend is requested
                   (default: yandex,mojeek,startpage,yahoo,google,duckduckgo,brave)
"""

import os
from typing import Optional

from ddgs import DDGS
from fastapi import FastAPI, HTTPException, Query

_DEFAULT_BACKEND_ORDER = "yandex,mojeek,startpage,yahoo,google,duckduckgo,brave"

app = FastAPI(title="DDG Bridge", version="0.2.0")


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
    safesearch: str = Query(os.getenv("SAFESEARCH", "moderate")),
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
            raw = DDGS(timeout=8).text(q, **{**kwargs, "backend": backend}) or []
        except Exception as exc:
            raise HTTPException(status_code=503, detail=str(exc))
        backend_used = backend
    else:
        # Fan-out: try backends sequentially in priority order.
        order = os.getenv("BACKEND_ORDER", _DEFAULT_BACKEND_ORDER)
        backends = [b.strip() for b in order.split(",") if b.strip()]
        raw = []
        backend_used = None
        last_err = "No results found."
        for b in backends:
            try:
                raw = DDGS(timeout=8).text(q, **{**kwargs, "backend": b}) or []
                if raw:
                    backend_used = b
                    break
            except Exception as exc:
                last_err = str(exc)
                raw = []
        if not raw:
            raise HTTPException(status_code=503, detail=last_err)

    results = [
        {
            "url": r.get("href") or r.get("url", ""),
            "title": r.get("title", ""),
            "snippet": r.get("body") or r.get("snippet", ""),
        }
        for r in raw
        if r.get("href") or r.get("url")
    ]

    return {"results": results, "backend_used": backend_used}


if __name__ == "__main__":
    import uvicorn

    port = int(os.getenv("PORT", "8001"))
    uvicorn.run(app, host="0.0.0.0", port=port)
