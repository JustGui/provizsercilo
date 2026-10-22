"""No external network: exercise the public route with mocked DDGS engines."""
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient
import main
from resilience import Breaker, Scheduler, error_code, proxy_pool


@pytest.fixture
def client(monkeypatch):
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler())
    monkeypatch.setenv('BACKEND_ORDER', 'brave,yahoo')
    return TestClient(main.app)


def result():
    return [{'href': 'https://example.org', 'title': 'Example', 'body': 'Public result'}]


def test_tls_moves_proxy_and_rotates_next_request(client, monkeypatch):
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler(['http://a:8888', 'http://b:8888']))
    calls = []
    def create(**kw):
        calls.append(kw['proxy'])
        class Engine:
            def text(self, q, **args):
                if kw['proxy'] == 'http://a:8888':
                    raise RuntimeError('h2 connection close_notify sensitive query password')
                return result()
        return Engine()
    monkeypatch.setattr(main, 'DDGS', create)
    assert client.get('/search', params={'q': 'query', 'backend': 'brave'}).status_code == 200
    assert client.get('/search', params={'q': 'query', 'backend': 'brave'}).status_code == 200
    assert calls == ['http://a:8888', 'http://b:8888', 'http://b:8888']
    health = client.get('/health/engines').json()
    assert health['proxies']['proxy-1']['state'] == 'open'
    assert health['engines']['brave']['successes'] == 2
    assert 'sensitive' not in str(health) and 'http://' not in str(health)


def test_engine_fallback_and_cleaning(client, monkeypatch):
    calls = []
    class Engine:
        def __init__(self, **kw): pass
        def text(self, q, **kw):
            calls.append(kw['backend'])
            if kw['backend'] == 'brave':
                return [{'href': 'https://porn.example', 'title': '', 'body': ''}]
            return result()
    monkeypatch.setattr(main, 'DDGS', Engine)
    response = client.get('/search', params={'q': 'query', 'backend': 'brave'})
    assert response.json()['backend_used'] == 'yahoo'
    assert calls == ['brave', 'yahoo']
    assert client.get('/health/engines').json()['engines']['brave']['last_error'] == 'empty'


def test_engine_error_does_not_poison_other_engines_proxy(client, monkeypatch):
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler(['http://a:8888']))
    with patch.object(main, 'DDGS') as ddgs:
        ddgs.return_value.text.side_effect = [RuntimeError('HTTP 403'), result()]
        assert client.get('/search', params={'q': 'query'}).json()['backend_used'] == 'yahoo'
    assert main._SCHEDULER.health()['proxies']['proxy-1']['state'] == 'closed'


def test_all_routes_failed_safe_error_and_no_direct(client, monkeypatch):
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler(['socks5://user:secret@a:9050']))
    with patch.object(main, 'DDGS') as ddgs:
        ddgs.return_value.text.side_effect = RuntimeError('TLS secret query https://private')
        response = client.get('/search', params={'q': 'private'})
        assert response.status_code == 503
        assert response.json()['detail'] == 'Search routes unavailable'
        assert ddgs.call_count == 1
        assert ddgs.call_args.kwargs['proxy'].startswith('socks5://')
    assert main._SCHEDULER.health()['engines']['yahoo']['attempts'] == 0


def test_budget_and_attempt_cap(client, monkeypatch):
    monkeypatch.setattr(main, '_MAX_ATTEMPTS', 1)
    with patch.object(main, 'DDGS') as ddgs:
        ddgs.return_value.text.return_value = []
        assert client.get('/search', params={'q': 'query'}).status_code == 503
        assert ddgs.call_count == 1
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler())
    monkeypatch.setattr(main, '_BUDGET', -1)
    with patch.object(main, 'DDGS') as ddgs:
        assert client.get('/search', params={'q': 'query'}).status_code == 503
        ddgs.assert_not_called()


def test_invalid_backend_never_invokes_ddgs(client):
    with patch.object(main, 'DDGS') as ddgs:
        assert client.get('/search', params={'q': 'query', 'backend': 'invalid'}).status_code == 400
        assert client.get('/search', params={'q': ' '}).status_code == 400
        ddgs.assert_not_called()
    assert client.get('/health').json() == {'status': 'ok'}


def test_breaker_exponential_cap_half_open_and_reset():
    b = Breaker()
    for now, expected in [(0, 5), (5, 15), (15, 35), (35, 55)]:
        assert b.acquire(now)
        if b.probing:
            assert not b.acquire(now)
        b.finish('timeout', now, 5, 20, lambda: 0)
        assert b.retry_at == expected
        assert not b.acquire(expected - .01)
    assert b.acquire(55)
    assert b.snapshot(55)['state'] == 'half_open'
    b.finish(None, 55, 5, 20, lambda: 1)
    assert b.snapshot(55)['state'] == 'closed'
    assert b.failures == 0
    b.acquire(56)
    b.finish('timeout', 56, 5, 20, lambda: 1)
    assert b.retry_at == 62.25


def test_half_open_probe_is_exclusive():
    now = [0]
    s = Scheduler(clock=lambda: now[0], jitter=lambda: 0)
    assert s.acquire_engine('brave')
    s.finish_engine('brave', 'timeout')
    now[0] = 5
    with ThreadPoolExecutor(max_workers=8) as pool:
        assert sum(pool.map(lambda _: s.acquire_engine('brave'), range(20))) == 1
    s.finish_engine('brave', None)
    assert s.health()['engines']['brave']['state'] == 'closed'


def test_pool_validation_overrides_and_rotation():
    assert proxy_pool('http://a:8888,http://a:8888,socks5h://b:9050') == ['http://a:8888','socks5h://b:9050']
    for invalid in ['ftp://a:99', 'http://a', 'secret']:
        with pytest.raises(ValueError): proxy_pool(invalid)
    s = Scheduler(['http://a:88','http://b:88'], {'yahoo':['http://c:88']})
    assert s.routes('brave', s.offset()) == ['http://a:88','http://b:88']
    assert s.routes('brave', s.offset()) == ['http://b:88','http://a:88']
    assert s.routes('yahoo', 1) == ['http://c:88']


@pytest.mark.parametrize('message,code', [('TLS close_notify','tls_h2'), ('timed out','timeout'),
    ('proxy connect failure','connection'), ('HTTP 429','blocked'), ('secret source text','engine_error')])
def test_safe_error_codes(message, code):
    assert error_code(RuntimeError(message)) == code


def test_disabled_engine_cannot_silently_fan_out(client, monkeypatch):
    monkeypatch.setenv('BACKEND_ORDER', '')
    monkeypatch.setattr(main, 'DDGS_ENGINES', {'text': {'brave': object()}})
    with patch.object(main, 'DDGS') as ddgs:
        assert client.get('/search', params={'q': 'query', 'backend': 'yandex'}).status_code == 503
        ddgs.assert_not_called()
    assert not client.get('/health/engines').json()['engines']['yandex']['enabled']


def test_transport_recovery_half_open(client, monkeypatch):
    now = [0]
    s = Scheduler(['http://a:8888'], clock=lambda: now[0], jitter=lambda: 0)
    monkeypatch.setattr(main, '_SCHEDULER', s)
    with patch.object(main, 'DDGS') as ddgs:
        ddgs.return_value.text.side_effect = [RuntimeError('TLS failure'), result()]
        assert client.get('/search', params={'q': 'query'}).status_code == 503
        assert client.get('/search', params={'q': 'query'}).status_code == 503
        assert ddgs.call_count == 1
        now[0] = 5
        assert client.get('/search', params={'q': 'query'}).status_code == 200
    assert s.health()['engines']['brave']['state'] == 'closed'
    assert s.health()['proxies']['proxy-1']['state'] == 'closed'


def test_healthy_routes_rotate_per_request(client, monkeypatch):
    monkeypatch.setattr(main, '_SCHEDULER', Scheduler(['http://a:8888','http://b:8888']))
    with patch.object(main, 'DDGS') as ddgs:
        ddgs.return_value.text.return_value = result()
        for _ in range(2):
            assert client.get('/search', params={'q': 'query'}).status_code == 200
        assert [call.kwargs['proxy'] for call in ddgs.call_args_list] == ['http://a:8888','http://b:8888']
