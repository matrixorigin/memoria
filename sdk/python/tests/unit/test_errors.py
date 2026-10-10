"""Unit tests for error mapping and retry logic."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import MemoriaClient, MemoriaServerError, MemoriaConnectionError
from memoria.exceptions import MemoriaAPIError
from tests.conftest import BASE_URL, API_KEY, MEMORY_STUB


@pytest.fixture
def client_no_retry() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest.fixture
def client_with_retry() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=2)


@pytest.fixture
def client_retrying_writes() -> MemoriaClient:
    return MemoriaClient(
        base_url=BASE_URL, api_key=API_KEY, max_retries=2, retry_unsafe_writes=True
    )


def test_500_raises_server_error(httpx_mock: HTTPXMock, client_no_retry: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=500, json={"detail": "internal error"})
    with pytest.raises(MemoriaServerError) as exc:
        client_no_retry.memories.store(content="x")
    assert exc.value.status_code == 500


def test_500_on_post_not_retried(
    httpx_mock: HTTPXMock, client_with_retry: MemoriaClient
) -> None:
    # POST is non-idempotent: 500 is NOT retried to avoid duplicate writes.
    httpx_mock.add_response(status_code=500, json={"detail": "server error"})
    with pytest.raises(MemoriaServerError):
        client_with_retry.memories.store(content="x")
    # Only one request should have been made (no retry).
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("status", [502, 503, 504])
def test_gateway_error_on_post_not_retried_by_default(
    httpx_mock: HTTPXMock, client_with_retry: MemoriaClient, status: int
) -> None:
    # A gateway error does not prove the upstream skipped the write: it can fire
    # after a commit (notably 504). Without an idempotency key the only safe
    # default for a non-idempotent method is to surface the error.
    httpx_mock.add_response(status_code=status, json={"detail": "gateway"})
    with pytest.raises(MemoriaServerError):
        client_with_retry.memories.store(content="x")
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("status", [502, 503, 504])
def test_gateway_error_on_post_retried_when_opted_in(
    httpx_mock: HTTPXMock, client_retrying_writes: MemoriaClient, status: int
) -> None:
    httpx_mock.add_response(status_code=status, json={"detail": "gateway"})
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = client_retrying_writes.memories.store(content="x")
    assert mem.memory_id == "mem_abc123"
    assert len(httpx_mock.get_requests()) == 2


@pytest.mark.parametrize("status", [500, 502, 503, 504])
def test_gateway_error_on_get_still_retried(
    httpx_mock: HTTPXMock, client_with_retry: MemoriaClient, status: int
) -> None:
    # Idempotent methods are unaffected: repeating a GET cannot create anything.
    httpx_mock.add_response(status_code=status, json={"detail": "gateway"})
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    assert client_with_retry.memories.list().items == []
    assert len(httpx_mock.get_requests()) == 2


def test_gateway_error_exhausted_retries_raises(
    httpx_mock: HTTPXMock, client_retrying_writes: MemoriaClient
) -> None:
    for _ in range(3):  # max_retries=2 → 3 total attempts
        httpx_mock.add_response(status_code=502, json={"detail": "bad gateway"})
    with pytest.raises(MemoriaServerError):
        client_retrying_writes.memories.store(content="x")


def test_unknown_4xx_raises_api_error(
    httpx_mock: HTTPXMock, client_no_retry: MemoriaClient
) -> None:
    httpx_mock.add_response(status_code=410, json={"detail": "gone"})
    with pytest.raises(MemoriaAPIError) as exc:
        client_no_retry.memories.store(content="x")
    assert exc.value.status_code == 410


def test_exception_str_includes_status_and_detail() -> None:
    err = MemoriaAPIError(422, "content is empty")
    assert "422" in str(err)
    assert "content is empty" in str(err)


def test_ping_success(httpx_mock: HTTPXMock, client_no_retry: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"status": "ok"})
    assert client_no_retry.ping() is True
