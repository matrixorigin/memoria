"""Unit tests for transport-error mapping and write-retry safety.

Every httpx failure that produced no response must reach callers as a
MemoriaConnectionError. RemoteProtocolError ("server disconnected without
sending a response") and ProxyError are siblings of NetworkError rather than
subclasses, so catching hand-picked subclasses let them escape the SDK's
exception hierarchy.
"""

from __future__ import annotations

import httpx
import pytest
from pytest_httpx import HTTPXMock

from memoria import (
    AsyncMemoriaClient,
    MemoriaClient,
    MemoriaConnectionError,
    MemoriaServerError,
)
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB

# Every httpx.TransportError branch: no response was produced in any of them.
TRANSPORT_ERRORS = [
    httpx.ConnectError,
    httpx.ConnectTimeout,
    httpx.ReadTimeout,
    httpx.ReadError,
    httpx.WriteError,
    httpx.RemoteProtocolError,
    httpx.LocalProtocolError,
    httpx.ProxyError,
]


@pytest.fixture
def client() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest.mark.parametrize("error", TRANSPORT_ERRORS, ids=lambda e: e.__name__)
def test_transport_error_is_wrapped_on_get(
    httpx_mock: HTTPXMock, client: MemoriaClient, error: type[httpx.TransportError]
) -> None:
    httpx_mock.add_exception(error("boom"))
    with pytest.raises(MemoriaConnectionError) as raised:
        client.memories.list()
    assert isinstance(raised.value.__cause__, error)


@pytest.mark.parametrize("error", TRANSPORT_ERRORS, ids=lambda e: e.__name__)
def test_transport_error_is_wrapped_on_post(
    httpx_mock: HTTPXMock, client: MemoriaClient, error: type[httpx.TransportError]
) -> None:
    httpx_mock.add_exception(error("boom"))
    with pytest.raises(MemoriaConnectionError) as raised:
        client.memories.store(content="x")
    assert isinstance(raised.value.__cause__, error)


@pytest.mark.parametrize("error", TRANSPORT_ERRORS, ids=lambda e: e.__name__)
@pytest.mark.asyncio
async def test_transport_error_is_wrapped_async(
    httpx_mock: HTTPXMock, error: type[httpx.TransportError]
) -> None:
    httpx_mock.add_exception(error("boom"))
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        with pytest.raises(MemoriaConnectionError) as raised:
            await client.memories.store(content="x")
    assert isinstance(raised.value.__cause__, error)


def test_disconnect_on_post_is_not_replayed_by_default(httpx_mock: HTTPXMock) -> None:
    # The server may have committed before dropping the connection, so a POST
    # must not be replayed just because the response was lost.
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    with pytest.raises(MemoriaConnectionError):
        client.memories.store(content="x")
    assert len(httpx_mock.get_requests()) == 1


def test_disconnect_on_post_is_replayed_when_opted_in(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    httpx_mock.add_response(json=MEMORY_STUB)
    client = MemoriaClient(
        base_url=BASE_URL, api_key=API_KEY, max_retries=3, retry_unsafe_writes=True
    )
    assert client.memories.store(content="x").memory_id == "mem_abc123"
    assert len(httpx_mock.get_requests()) == 2


def test_disconnect_on_get_is_retried(httpx_mock: HTTPXMock) -> None:
    # Repeating an idempotent request cannot create anything, so recovery is safe.
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    assert client.memories.list().items == []
    assert len(httpx_mock.get_requests()) == 2


def test_connect_error_on_post_is_retried(httpx_mock: HTTPXMock) -> None:
    # The request never reached the server, so it cannot have committed.
    httpx_mock.add_exception(httpx.ConnectError("refused"))
    httpx_mock.add_response(json=MEMORY_STUB)
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    assert client.memories.store(content="x").memory_id == "mem_abc123"
    assert len(httpx_mock.get_requests()) == 2


# ── PUT /v1/memories/{id}/correct is non-idempotent despite the verb ─────────
# The server mints a replacement record and supersedes the original, so a replay
# either 404s on the already-superseded memory or creates a second replacement.


@pytest.mark.parametrize("status", [502, 503, 504])
def test_correct_is_not_retried_on_gateway_error(httpx_mock: HTTPXMock, status: int) -> None:
    httpx_mock.add_response(status_code=status, json={"detail": "gateway"})
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    with pytest.raises(MemoriaServerError):
        client.memories.correct("m1", new_content="new")
    assert len(httpx_mock.get_requests()) == 1


def test_correct_is_not_retried_on_disconnect(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    with pytest.raises(MemoriaConnectionError):
        client.memories.correct("m1", new_content="new")
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.asyncio
async def test_correct_is_not_retried_async(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3) as client:
        with pytest.raises(MemoriaConnectionError):
            await client.memories.correct("m1", new_content="new")
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.asyncio
async def test_correct_gateway_error_not_retried_async(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(status_code=504, json={"detail": "gateway"})
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3) as client:
        with pytest.raises(MemoriaServerError):
            await client.memories.correct("m1", new_content="new")
    assert len(httpx_mock.get_requests()) == 1


def test_correct_is_retried_when_opted_in(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    httpx_mock.add_response(json=MEMORY_STUB)
    client = MemoriaClient(
        base_url=BASE_URL, api_key=API_KEY, max_retries=3, retry_unsafe_writes=True
    )
    client.memories.correct("m1", new_content="new")
    assert len(httpx_mock.get_requests()) == 2


def test_genuinely_idempotent_delete_is_still_retried(httpx_mock: HTTPXMock) -> None:
    # The override is per-operation: DELETE keeps its method-based default.
    httpx_mock.add_exception(httpx.RemoteProtocolError("server disconnected"))
    httpx_mock.add_response(status_code=204)
    client = MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=3)
    client.memories.delete("m1")
    assert len(httpx_mock.get_requests()) == 2
