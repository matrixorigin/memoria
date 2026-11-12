"""Corrupt response encodings obey SDK error and write-replay contracts."""

import asyncio
import gzip
import json

import httpx
import pytest

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaConnectionError, _http


@pytest.fixture(params=["sync", "async"])
def invoke(request, monkeypatch):
    delays = []
    monkeypatch.setattr(_http.time, "sleep", delays.append)

    async def sleep(delay):
        delays.append(delay)

    monkeypatch.setattr(asyncio, "sleep", sleep)

    def call(handler, *, operation="list", retries=2, unsafe=False):
        options = dict(max_retries=retries, retry_unsafe_writes=unsafe)
        arguments = {} if operation == "list" else {"content": "synthetic memory"}
        if operation == "correct":
            arguments = {"id": "original", "new_content": "synthetic memory"}
        if request.param == "sync":
            with MemoriaClient("https://memoria.test", "synthetic-key", **options) as client:
                client._http.close()
                client._http = httpx.Client(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return getattr(client.memories, operation)(**arguments)

        async def run():
            async with AsyncMemoriaClient(
                "https://memoria.test", "synthetic-key", **options
            ) as client:
                await client._ahttp.aclose()
                client._ahttp = httpx.AsyncClient(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return await getattr(client.memories, operation)(**arguments)

        return asyncio.run(run())

    return call, delays


def encoded_response(content):
    # Stream bytes so decoding happens in HTTPX's normal response-read path.
    return httpx.Response(
        200,
        headers={"content-type": "application/json", "content-encoding": "gzip"},
        stream=httpx.ByteStream(content),
    )


@pytest.mark.parametrize("operation,unsafe", [("list", False), ("store", True), ("correct", True)])
def test_corrupt_gzip_recovers_only_when_replay_is_allowed(invoke, operation, unsafe):
    call, delays = invoke
    requests = []
    payload = {"items": []} if operation == "list" else {"memory_id": "saved", "content": "ok"}

    def handle(request):
        requests.append(request)
        assert request.headers["authorization"] == "Bearer synthetic-key"
        if len(requests) == 1:
            return encoded_response(b"invalid gzip")
        return encoded_response(gzip.compress(json.dumps(payload).encode()))

    result = call(handle, operation=operation, unsafe=unsafe)
    assert result.items == [] if operation == "list" else result.memory_id == "saved"
    assert len(requests) == 2
    assert (requests[0].method, requests[0].url, requests[0].content) == (
        requests[1].method,
        requests[1].url,
        requests[1].content,
    )
    assert delays == [0.5]


@pytest.mark.parametrize(
    "operation,retries,attempts",
    [("list", 0, 1), ("list", 2, 3), ("store", 2, 1), ("correct", 2, 1)],
)
def test_corrupt_gzip_preserves_error_cause_budget_and_write_safety(
    invoke, operation, retries, attempts
):
    call, delays = invoke
    requests = []

    def handle(request):
        requests.append(request)
        return encoded_response(b"invalid gzip")

    with pytest.raises(MemoriaConnectionError) as caught:
        call(handle, operation=operation, retries=retries)
    assert isinstance(caught.value.__cause__, httpx.DecodingError)
    assert str(caught.value) == str(caught.value.__cause__)
    assert len(requests) == attempts
    assert delays == [0.5, 1.0][: attempts - 1]
