"""Read-only POST queries recover from transient faults without replaying writes."""

import asyncio
import json

import httpx
import pytest

from memoria import AsyncMemoriaClient, MemoriaClient, _http
from memoria.exceptions import (
    MemoriaConnectionError,
    MemoriaServerError,
    MemoriaUnprocessableError,
)
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB


@pytest.fixture(params=["sync", "async"])
def invoke(request, monkeypatch):
    delays = []
    monkeypatch.setattr(_http.time, "sleep", delays.append)

    async def sleep(delay):
        delays.append(delay)

    monkeypatch.setattr(asyncio, "sleep", sleep)

    def call(handler, operation, kwargs, max_retries=2):
        options = dict(base_url=BASE_URL, api_key=API_KEY, max_retries=max_retries)
        if request.param == "sync":
            with MemoriaClient(**options) as client:
                client._http.close()
                client._http = httpx.Client(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return getattr(client.memories, operation)(**kwargs)

        async def run():
            async with AsyncMemoriaClient(**options) as client:
                await client._ahttp.aclose()
                client._ahttp = httpx.AsyncClient(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return await getattr(client.memories, operation)(**kwargs)

        return asyncio.run(run())

    return call, delays


READS = [
    (
        "query",
        {"subject_id": "alice", "branch": "test", "cursor": "a" * 32},
        {"items": [MEMORY_STUB], "next_cursor": "b" * 32},
    ),
    ("fulltext_search", {"query": "example", "branch": "test", "limit": 3}, [MEMORY_STUB]),
]
FAULTS = [500, 502, 503, 504, httpx.ReadTimeout, httpx.RemoteProtocolError]


def fail(request, fault):
    if isinstance(fault, int):
        return httpx.Response(fault, json={"detail": "synthetic fault"})
    raise fault("synthetic fault", request=request)


@pytest.mark.parametrize("operation,kwargs,response", READS)
@pytest.mark.parametrize("fault", FAULTS)
def test_read_post_recovers(invoke, operation, kwargs, response, fault):
    call, delays = invoke
    requests = []

    def handler(request):
        requests.append(request)
        if len(requests) == 1:
            return fail(request, fault)
        return httpx.Response(200, json=response)

    result = call(handler, operation, kwargs)
    assert result.items[0].memory_id == MEMORY_STUB["memory_id"]
    if operation == "query":
        assert result.next_cursor == "b" * 32
    assert len(requests) == 2
    expected_path = "/v1/memories/" + operation.replace("_", "-")
    for request in requests:
        assert request.method == "POST"
        assert request.url.path == expected_path
        assert request.headers["authorization"] == f"Bearer {API_KEY}"
        assert json.loads(request.content).items() >= kwargs.items()
    assert requests[0].content == requests[1].content
    assert delays == [0.5]


@pytest.mark.parametrize("operation,kwargs,response", READS)
@pytest.mark.parametrize(
    "fault,error_type",
    [
        (503, MemoriaServerError),
        (httpx.ReadTimeout, MemoriaConnectionError),
        (422, MemoriaUnprocessableError),
    ],
)
@pytest.mark.parametrize("max_retries", [0, 2])
def test_read_post_failure_respects_budget(
    invoke, operation, kwargs, response, fault, error_type, max_retries
):
    call, delays = invoke
    requests = []

    def handler(request):
        requests.append(request)
        return fail(request, fault)

    with pytest.raises(error_type):
        call(handler, operation, kwargs, max_retries)
    attempts = 1 if fault == 422 else max_retries + 1
    assert len(requests) == attempts
    assert delays == [0.5, 1.0][: attempts - 1]


@pytest.mark.parametrize(
    "operation,kwargs",
    [
        ("store", {"content": "example"}),
        ("correct", {"id": "a" * 32, "new_content": "replacement"}),
        ("purge", {"memory_ids": ["a" * 32]}),
    ],
)
@pytest.mark.parametrize(
    "fault,error_type",
    [
        (503, MemoriaServerError),
        (httpx.ReadTimeout, MemoriaConnectionError),
    ],
)
def test_ambiguous_write_failure_is_not_retried(invoke, operation, kwargs, fault, error_type):
    call, delays = invoke
    requests = []

    def handler(request):
        requests.append(request)
        return fail(request, fault)

    with pytest.raises(error_type):
        call(handler, operation, kwargs)
    assert len(requests) == 1
    assert delays == []
