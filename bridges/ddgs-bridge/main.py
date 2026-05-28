"""
DDG Bridge — thin FastAPI wrapper around duckduckgo-search.

Called by ProvizSercilo as a standard HTTP provider.
The Rust service treats this bridge like any other provider:
  - key_ref in the DB resolves to this service's base URL (e.g. "http://localhost:8001")
  - Rate limiting, cooldowns, and fallback chain all apply normally.

Usage:
    pip install -r requirements.txt
    uvicorn main:app --host 0.0.0.0 --port 8001

Optional env vars:
    PORT          Listen port (default: 8001)
    MAX_RESULTS   Default max results (default: 10)
    SAFESEARCH    safe/moderate/off (default: moderate)
"""

import os
from typing import Optional

from duckduckgo_search import DDGS
from fastapi import FastAPI, HTTPException, Query
from fastapi.responses import JSONResponse

app = FastAPI(title="DDG Bridge", version="0.1.0")


@app.get("/health")
def health():
    return {"status": "ok"}


@app.get("/search")
def search(
    q: str = Query(..., description="Search query"),
    n: int = Query(10, ge=1, le=50, description="Number of results"),
    language: Optional[str] = Query(None, description="ISO 639-1 language code"),
    region: Optional[str] = Query(None, description="DDG region code (e.g. 'fr-fr')"),
    safesearch: str = Query(os.getenv("SAFESEARCH", "moderate")),
):
    """
    Execute a DDG web search and return normalised results.

    Returns:
        { "results": [{ "url", "title", "snippet" }, ...] }
    """
    if not q.strip():
        raise HTTPException(status_code=400, detail="Query cannot be empty")

    kwargs: dict = {
        "max_results": n,
        "safesearch": safesearch,
    }
    if region:
        kwargs["region"] = region
    elif language:
        # Map language to a DDG region code (best-effort; DDG uses e.g. "fr-fr")
        kwargs["region"] = f"{language}-{language}"

    try:
        raw = DDGS().text(q, **kwargs) or []
    except Exception as exc:
        # Surface as 503 so ProvizSercilo triggers the normal error fallback.
        raise HTTPException(status_code=503, detail=str(exc))

    results = [
        {
            "url": r.get("href") or r.get("url", ""),
            "title": r.get("title", ""),
            "snippet": r.get("body") or r.get("snippet", ""),
        }
        for r in raw
        if r.get("href") or r.get("url")
    ]

    return {"results": results}


if __name__ == "__main__":
    import uvicorn

    port = int(os.getenv("PORT", "8001"))
    uvicorn.run(app, host="0.0.0.0", port=port)
