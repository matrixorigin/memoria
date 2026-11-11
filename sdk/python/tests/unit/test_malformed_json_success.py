"""Malformed JSON success responses preserve the Memoria SDK error/retry contract.

The server may return HTTP 200 with application/json and truncated JSON. A safe
read can be repeated; a write cannot be replayed without explicit opt-in.
"""

from __future__ import annotations

import asyncio
import json

import httpx
import pytest

from memoria import _http
from memoria.exceptions import MemoriaConnectionError


@pytest.fixture(params=["sync", "async"])
def invoke(request, monkeypatch):
    delays = []
    monkeypatch.setattr(_http.time, "sleep", delays.append)

    async def fake_sleep(seconds):
        delays.append(seconds)

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)

    def send(handler, *, method="GET", retries=2, unsafe=False):
        owner = _http._HttpTransport(
            "https://memoria.invalid", "sample-key", max_retries=retries,
            retry_unsafe_writes=unsafe,
        )
        transport = httpx.MockTransport(handler)
        if request.param == "sync":
            with httpx.Client(transport=transport, headers=owner._headers) as client:
                owner._http = client
                return owner._request(method, "/v1/memories")

        async def run():
            async with httpx.AsyncClient(transport=transport, headers=owner._headers) as client:
                owner._ahttp = client
                return await owner._arequest(method, "/v1/memories")

        return asyncio.run(run())

    return send, delays


def malformed_json():
    return httpx.Response(
        200, headers={"content-type": "application/json"}, content=b'{"partial":'
    )


def test_safe_get_recovers_after_malformed_json(invoke):
    send, delays = invoke
    requests = []

    def handle(request):
        requests.append(request)
        if len(requests) == 1:
            return malformed_json()
        return httpx.Response(200, json={"items": []})

    assert send(handle) == {"items": []}
    assert len(requests) == 2
    assert delays == [0.5]


def test_unsafe_write_does_not_replay_malformed_success(invoke):
    send, delays = invoke
    requests = []

    def handle(request):
        requests.append(request)
        return malformed_json()

    with pytest.raises(MemoriaConnectionError) as caught:
        send(handle, method="POST")
    assert isinstance(caught.value.__cause__, json.JSONDecodeError)
    assert str(caught.value) == "Invalid JSON response from Memoria API"
    assert len(requests) == 1
    assert delays == []


def test_opted_in_write_recovers_after_malformed_success(invoke):
    send, delays = invoke
    requests = []

    def handle(request):
        requests.append(request)
        if len(requests) == 1:
            return malformed_json()
        return httpx.Response(200, json={"memory_id": "saved"})

    assert send(handle, method="POST", unsafe=True) == {"memory_id": "saved"}
    assert len(requests) == 2
    assert delays == [0.5]


@pytest.mark.parametrize("retries", [0, 2])
def test_malformed_json_retry_budget_and_cause(invoke, retries):
    send, delays = invoke
    requests = []

    def handle(request):
        requests.append(request)
        return malformed_json()

    with pytest.raises(MemoriaConnectionError) as caught:
        send(handle, retries=retries)
    assert isinstance(caught.value.__cause__, json.JSONDecodeError)
    assert len(requests) == retries + 1
    assert delays == [0.5, 1.0][:retries]


def test_plain_text_success_is_unchanged(invoke):
    send, delays = invoke

    def handle(request):
        return httpx.Response(
            200, headers={"content-type": "text/plain"}, content=b"ready"
        )

    assert send(handle) == "ready"
    assert delays == []
