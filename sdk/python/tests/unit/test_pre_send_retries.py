"""Pre-send failures are retryable without replaying ambiguous writes."""

import asyncio
import json

import httpx
import pytest

from memoria import _http
from memoria.exceptions import MemoriaConnectionError, MemoriaServerError

PRE_SEND_ERRORS = [httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout]
WRITES = [("POST", None), ("PATCH", None), ("PUT", False)]
BODY = {"content": "synthetic retry regression"}


@pytest.fixture(params=["sync", "async"])
def send(request, monkeypatch):
    delays = []
    monkeypatch.setattr(_http.time, "sleep", delays.append)

    async def sleep(delay):
        delays.append(delay)

    monkeypatch.setattr(asyncio, "sleep", sleep)

    def invoke(handler, *, method="POST", idempotent=None, max_retries=2, unsafe=False):
        owner = _http._HttpTransport(
            "https://memoria.invalid",
            "synthetic-key",
            timeout=7,
            max_retries=max_retries,
            retry_unsafe_writes=unsafe,
        )
        options = {"transport": httpx.MockTransport(handler), "headers": owner._headers}
        kwargs = {
            "json": BODY,
            "params": {"branch": "test", "unused": None},
            "idempotent": idempotent,
        }
        if request.param == "sync":
            with httpx.Client(**options) as client:
                owner._http = client
                return owner._request(method, "/v1/memories", **kwargs)

        async def run():
            async with httpx.AsyncClient(**options) as client:
                owner._ahttp = client
                return await owner._arequest(method, "/v1/memories", **kwargs)

        return asyncio.run(run())

    return invoke, delays


def check_request(request, method):
    assert request.method == method
    assert str(request.url) == "https://memoria.invalid/v1/memories?branch=test"
    assert request.headers["authorization"] == "Bearer synthetic-key"
    assert json.loads(request.content) == BODY
    assert request.extensions["timeout"] == dict.fromkeys(
        ["connect", "read", "write", "pool"],
        7,
    )


@pytest.mark.parametrize("error_type", PRE_SEND_ERRORS)
@pytest.mark.parametrize("method,idempotent", WRITES)
def test_write_recovers_after_pre_send_failure(send, error_type, method, idempotent):
    invoke, delays = send
    requests = []

    def handle(request):
        requests.append(request)
        check_request(request, method)
        if len(requests) == 1:
            raise error_type("pre-send failure", request=request)
        return httpx.Response(201, json={"memory_id": "created-once"})

    assert invoke(handle, method=method, idempotent=idempotent) == {
        "memory_id": "created-once",
    }
    assert len(requests) == 2
    assert requests[0].content == requests[1].content
    assert delays == [0.5]


@pytest.mark.parametrize("error_type", PRE_SEND_ERRORS)
@pytest.mark.parametrize("max_retries", [0, 2])
def test_pre_send_failure_respects_budget_and_preserves_cause(send, error_type, max_retries):
    invoke, delays = send
    errors = []

    def handle(request):
        error = error_type(f"attempt {len(errors) + 1}", request=request)
        errors.append(error)
        raise error

    with pytest.raises(MemoriaConnectionError) as caught:
        invoke(handle, max_retries=max_retries)
    assert len(errors) == max_retries + 1
    assert caught.value.__cause__ is errors[-1]
    assert str(caught.value) == str(errors[-1])
    assert delays == [0.5, 1.0][:max_retries]


@pytest.mark.parametrize(
    "error_type",
    [
        httpx.ReadTimeout,
        httpx.WriteTimeout,
        httpx.RemoteProtocolError,
        httpx.ReadError,
        httpx.WriteError,
        httpx.ProxyError,
        httpx.LocalProtocolError,
    ],
)
@pytest.mark.parametrize("method,idempotent", WRITES)
def test_ambiguous_write_failure_is_not_retried(send, error_type, method, idempotent):
    invoke, delays = send
    requests = []
    error = error_type("outcome unknown")

    def handle(request):
        requests.append(request)
        raise error

    with pytest.raises(MemoriaConnectionError) as caught:
        invoke(handle, method=method, idempotent=idempotent)
    assert caught.value.__cause__ is error
    assert len(requests) == 1
    assert delays == []


@pytest.mark.parametrize("status", [500, 502, 503, 504])
@pytest.mark.parametrize("method,idempotent", WRITES)
def test_ambiguous_write_response_is_not_retried(send, status, method, idempotent):
    invoke, delays = send
    requests = []

    def handle(request):
        requests.append(request)
        return httpx.Response(status, json={"detail": "outcome unknown"})

    with pytest.raises(MemoriaServerError) as caught:
        invoke(handle, method=method, idempotent=idempotent)
    assert caught.value.status_code == status
    assert len(requests) == 1
    assert delays == []


@pytest.mark.parametrize("error_type", [httpx.ConnectTimeout, httpx.PoolTimeout])
@pytest.mark.parametrize("method,idempotent", WRITES)
def test_pre_send_retry_stops_on_later_ambiguous_failure(send, error_type, method, idempotent):
    invoke, delays = send
    requests = []
    ambiguous = httpx.ReadTimeout("write may have committed")

    def handle(request):
        requests.append(request)
        if len(requests) == 1:
            raise error_type("not sent", request=request)
        raise ambiguous

    with pytest.raises(MemoriaConnectionError) as caught:
        invoke(handle, method=method, idempotent=idempotent)
    assert caught.value.__cause__ is ambiguous
    assert len(requests) == 2
    assert delays == [0.5]


@pytest.mark.parametrize("method,unsafe", [("GET", False), ("POST", True)])
def test_existing_safe_and_opt_in_retry_policy_is_preserved(send, method, unsafe):
    invoke, delays = send
    requests = []

    def handle(request):
        requests.append(request)
        if len(requests) == 1:
            raise httpx.ReadTimeout("response lost", request=request)
        return httpx.Response(200, json={"ok": True})

    assert invoke(handle, method=method, unsafe=unsafe) == {"ok": True}
    assert len(requests) == 2
    assert delays == [0.5]


def test_idempotent_request_recovers_after_proxy_error(send):
    invoke, delays = send
    requests = []

    def handle(request):
        requests.append(request)
        check_request(request, "GET")
        if len(requests) == 1:
            raise httpx.ProxyError("proxy temporarily unavailable", request=request)
        return httpx.Response(200, json={"ok": True})

    assert invoke(handle, method="GET") == {"ok": True}
    assert len(requests) == 2
    assert delays == [0.5]
