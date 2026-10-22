"""Opt-in live diagnostic: outputs counts/status/timing, never query/result text.

Install requirements.txt plus httpx, then run:
    python probe.py --proxy http://127.0.0.1:28505
Not collected by pytest or CI. Each sample creates a fresh engine/client;
independent engine/route series run with at most two workers.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import logging
import time

from ddgs.engines import ENGINES
import httpx
import primp

QUERIES = (
    'France inflation septembre 2026 INSEE',
    'France budget 2027 actualités',
    'France énergie renouvelable bilan 2025',
)
BACKENDS = ('brave', 'yahoo', 'yandex', 'duckduckgo', 'startpage')


def probe(engine, route, mode, index, proxy_url):
    proxy = None if route == 'direct' else proxy_url
    start = time.monotonic()
    statuses, error, count = [], None, 0
    httpx_client = None
    try:
        if engine not in ENGINES['text']:
            error = 'disabled'
        else:
            instance = ENGINES['text'][engine](proxy=proxy, timeout=6)
            if mode == 'httpx-h1':
                httpx_client = httpx.Client(
                    proxy=proxy, timeout=6, http2=False, follow_redirects=True,
                    headers={'User-Agent': 'Mozilla/5.0'},
                )
                # Engine payload builders use primp's cookie setter interface.
                httpx_client.set_cookies = lambda url, cookies: httpx_client.cookies.update(cookies)
                instance.http_client.client = httpx_client
            elif mode != 'random':
                instance.http_client.client = primp.Client(
                    proxy=proxy, timeout=6, impersonate=mode,
                )
            request = instance.http_client.request

            def record_status(*args, **kwargs):
                response = request(*args, **kwargs)
                statuses.append(response.status_code)
                return response

            instance.http_client.request = record_status
            results = instance.search(QUERIES[index], region='fr-fr', safesearch='on') or []
            count = len(results)
    except Exception as exc:
        # Raw messages can include queries or credentials; retain only categories.
        message = str(exc).lower()
        if any(s in message for s in ('tls', 'http2', 'http/2', 'close_notify')):
            error = 'tls_h2'
        elif 'tim' in message:
            error = 'timeout'
        else:
            error = 'engine_error'
    finally:
        if httpx_client is not None:
            httpx_client.close()
    print(json.dumps(dict(
        engine=engine, route=route, mode=mode, query=index, status=statuses,
        results=count, error=error, seconds=round(time.monotonic() - start, 2),
    )), flush=True)


def series(engine, route, proxy_url):
    for index in range(len(QUERIES)):
        probe(engine, route, 'random', index, proxy_url)
    for mode in ('chrome_146', 'firefox_140', 'httpx-h1'):
        probe(engine, route, mode, 0, proxy_url)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proxy', required=True, help='Private HTTP/SOCKS proxy URL (never emitted)')
    args = parser.parse_args()
    logging.disable(logging.CRITICAL)
    combinations = [(e, r) for r in ('direct', 'proxy') for e in BACKENDS]
    with ThreadPoolExecutor(max_workers=2) as pool:
        list(pool.map(lambda pair: series(*pair, args.proxy), combinations))
