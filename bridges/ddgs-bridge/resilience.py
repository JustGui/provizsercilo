"""Process-local, thread-safe egress scheduling; stores counters, never search text."""
import random
import threading
import time
from dataclasses import dataclass
from urllib.parse import urlsplit


ENGINES = frozenset('brave yahoo yandex duckduckgo startpage google bing mojeek wikipedia grokipedia'.split())


def proxy_pool(spec):
    routes = list(dict.fromkeys(p.strip() for p in spec.split(',') if p.strip()))
    for route in routes:
        parsed = urlsplit(route)
        if parsed.scheme not in {'http', 'https', 'socks5', 'socks5h'} or not parsed.hostname or not parsed.port:
            raise ValueError('Proxy must be an http/https/socks5/socks5h URL with host and port')
    return routes


def error_code(exc):
    # Inspect transiently, never retain exception messages: they may contain queries,
    # source text, proxy passwords or response bodies.
    text = str(exc).lower()
    if any(x in text for x in ('close_notify', 'tls', 'http2', 'http/2', 'h2 connection')):
        return 'tls_h2'
    if 'timed out' in text or 'timeout' in text:
        return 'timeout'
    if 'proxy' in text or 'connect' in text:
        return 'connection'
    if '429' in text or 'ratelimit' in text or '403' in text:
        return 'blocked'
    return 'engine_error'


@dataclass
class Breaker:
    attempts: int = 0
    successes: int = 0
    failures: int = 0
    last_error: str | None = None
    retry_at: float = 0
    probing: bool = False

    def available(self, now):
        return not self.probing and now >= self.retry_at

    def acquire(self, now):
        if not self.available(now):
            return False
        if self.failures:
            self.probing = True
        self.attempts += 1
        return True

    def finish(self, error, now, base, cap, jitter):
        self.probing = False
        self.last_error = error
        if error is None:
            self.successes += 1
            self.failures = 0
            self.retry_at = 0
        else:
            self.failures += 1
            delay = min(cap, base * 2 ** min(self.failures - 1, 16))
            self.retry_at = now + min(cap, delay * (1 + jitter() * .25))

    def snapshot(self, now):
        return dict(attempts=self.attempts, successes=self.successes,
                    success_rate=self.successes / self.attempts if self.attempts else None,
                    consecutive_failures=self.failures, last_error=self.last_error,
                    state='half_open' if self.probing or (self.failures and now >= self.retry_at)
                    else 'open' if now < self.retry_at else 'closed',
                    retry_after_seconds=round(max(0, self.retry_at - now), 2))


class Scheduler:
    def __init__(self, proxies=(), overrides=None, *, clock=time.monotonic,
                 jitter=random.random, base=5, cap=300):
        self.pool = list(proxies)
        self.overrides = overrides or {}
        self.clock, self.jitter, self.base, self.cap = clock, jitter, base, cap
        self.lock = threading.Lock()
        self.cursor = 0
        self.engines = {e: Breaker() for e in sorted(ENGINES)}
        routes = set(self.pool)
        for values in self.overrides.values():
            routes.update(values)
        self.proxies = {p: Breaker() for p in sorted(routes)}

    def routes(self, engine, offset):
        routes = self.overrides.get(engine) or self.overrides.get('*') or self.pool or [None]
        i = offset % len(routes)
        return routes[i:] + routes[:i]

    def offset(self):
        with self.lock:
            value = self.cursor
            self.cursor += 1
            return value

    def acquire_engine(self, engine):
        with self.lock:
            return self.engines[engine].acquire(self.clock())

    def acquire_proxy(self, proxy):
        with self.lock:
            return proxy is None or self.proxies[proxy].acquire(self.clock())

    def finish_proxy(self, proxy, error):
        if proxy is not None:
            with self.lock:
                breaker = self.proxies[proxy]
                if error not in (None, 'tls_h2', 'timeout', 'connection'):
                    # Engine rejection proves transport reachability, but does
                    # not count as a successful search in route statistics.
                    breaker.probing = False
                    breaker.failures = 0
                    breaker.retry_at = 0
                    breaker.last_error = error
                else:
                    breaker.finish(error, self.clock(), self.base, self.cap, self.jitter)

    def finish_engine(self, engine, error, attempted=True):
        with self.lock:
            breaker = self.engines[engine]
            if attempted:
                breaker.finish(error, self.clock(), self.base, self.cap, self.jitter)
            else:
                # No route available: release a half-open lease without charging
                # an engine failure or making a healthy engine look unhealthy.
                breaker.probing = False
                breaker.attempts -= 1

    def health(self):
        with self.lock:
            now = self.clock()
            return {'engines': {e: b.snapshot(now) for e, b in self.engines.items()},
                    # Stable opaque IDs: no URLs, hosts or credentials in diagnostics.
                    'proxies': {f'proxy-{i+1}': b.snapshot(now)
                                for i, b in enumerate(self.proxies.values())}}
