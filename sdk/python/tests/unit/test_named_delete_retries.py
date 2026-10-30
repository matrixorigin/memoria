"""A retry must not delete a resource recreated under the same name."""

import asyncio

import httpx
import pytest

from memoria import AsyncMemoriaClient, MemoriaClient, _http
from memoria.exceptions import MemoriaConnectionError, MemoriaServerError
from tests.conftest import API_KEY, BASE_URL


@pytest.fixture(params=["sync", "async"])
def invoke(request, monkeypatch):
    delays = []
    monkeypatch.setattr(_http.time, "sleep", delays.append)

    async def sleep(delay):
        delays.append(delay)

    monkeypatch.setattr(asyncio, "sleep", sleep)

    def call(handler, resource, *, unsafe=False):
        options = dict(
            base_url=BASE_URL, api_key=API_KEY, max_retries=1, retry_unsafe_writes=unsafe
        )
        if request.param == "sync":
            with MemoriaClient(**options) as client:
                client._http.close()
                client._http = httpx.Client(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return getattr(client, resource).delete("reused_name")

        async def run():
            async with AsyncMemoriaClient(**options) as client:
                await client._ahttp.aclose()
                client._ahttp = httpx.AsyncClient(
                    transport=httpx.MockTransport(handler), headers=client._headers
                )
                return await getattr(client, resource).delete("reused_name")

        return asyncio.run(run())

    return call, delays


@pytest.mark.parametrize("resource", ["branches", "snapshots"])
@pytest.mark.parametrize(
    "failure,error_type",
    [
        ("gateway", MemoriaServerError),
        ("internal", MemoriaServerError),
        ("timeout", MemoriaConnectionError),
        ("disconnect", MemoriaConnectionError),
    ],
)
def test_recreated_resource_survives_ambiguous_delete(invoke, resource, failure, error_type):
    call, delays = invoke
    requests = []
    current = "original"

    def handler(request):
        nonlocal current
        requests.append(request)
        assert request.method == "DELETE"
        assert request.url.path == f"/v1/{resource}/reused_name"
        if len(requests) == 1:
            assert current == "original"
            # The original was deleted; another caller creates a replacement
            # before the lost response triggers any client retry.
            current = "replacement"
            if failure == "gateway":
                return httpx.Response(503, json={"detail": "gateway failure"})
            if failure == "internal":
                return httpx.Response(500, json={"detail": "server failure"})
            if failure == "disconnect":
                raise httpx.RemoteProtocolError("response lost", request=request)
            raise httpx.ReadTimeout("response lost", request=request)
        current = None
        return httpx.Response(204)

    caught = None
    try:
        call(handler, resource)
    except error_type as error:
        caught = error
    assert current == "replacement"
    assert caught is not None
    assert len(requests) == 1
    assert delays == []


@pytest.mark.parametrize("resource", ["branches", "snapshots"])
@pytest.mark.parametrize(
    "error_type", [httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout]
)
def test_named_delete_retries_before_sending(invoke, resource, error_type):
    call, delays = invoke
    requests = []

    def handler(request):
        requests.append(request)
        if len(requests) == 1:
            raise error_type("connection failed before send", request=request)
        return httpx.Response(204)

    assert call(handler, resource) is None
    assert len(requests) == 2
    assert requests[0].url == requests[1].url
    assert delays == [0.5]


@pytest.mark.parametrize("resource", ["branches", "snapshots"])
def test_named_delete_can_retry_ambiguous_failure_when_opted_in(invoke, resource):
    call, delays = invoke
    requests = []

    def handler(request):
        requests.append(request)
        if len(requests) == 1:
            return httpx.Response(503, json={"detail": "gateway failure"})
        return httpx.Response(204)

    assert call(handler, resource, unsafe=True) is None
    assert len(requests) == 2
    assert requests[0].url == requests[1].url
    assert delays == [0.5]
