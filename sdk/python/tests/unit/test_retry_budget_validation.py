"""Transport configuration must fail before allocating an HTTP client."""

import math
from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaServerError
from memoria.exceptions import MemoriaValidationError
from tests.conftest import API_KEY, BASE_URL


class _ApiKey(str):
    """A string subtype accepted anywhere a plain API key string is accepted."""


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("api_key", ["", None, 0, False, [], {}])
def test_invalid_api_key_fails_before_allocating_http_client(
    monkeypatch: pytest.MonkeyPatch, client_type: type, api_key: object
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    with pytest.raises(MemoriaValidationError, match="api_key.*non-empty string"):
        client_type(base_url=BASE_URL, api_key=api_key)  # type: ignore[arg-type]
    sync_factory.assert_not_called()
    async_factory.assert_not_called()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("api_key", [API_KEY, _ApiKey(API_KEY)])
def test_string_api_key_reaches_http_client_factory(
    monkeypatch: pytest.MonkeyPatch, client_type: type, api_key: str
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    client_type(base_url=BASE_URL, api_key=api_key)
    expected_factory = async_factory if client_type is AsyncMemoriaClient else sync_factory
    expected_factory.assert_called_once()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("flag", [None, 0, 1, "false", [], {}])
def test_invalid_unsafe_retry_flag_fails_before_allocating_http_client(
    monkeypatch: pytest.MonkeyPatch, client_type: type, flag: object
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    with pytest.raises(MemoriaValidationError, match="retry_unsafe_writes.*boolean"):
        client_type(  # type: ignore[arg-type]
            base_url=BASE_URL,
            api_key=API_KEY,
            retry_unsafe_writes=flag,
        )
    sync_factory.assert_not_called()
    async_factory.assert_not_called()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("flag", [False, True])
def test_boolean_unsafe_retry_flag_reaches_http_client_factory(
    monkeypatch: pytest.MonkeyPatch, client_type: type, flag: bool
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    client_type(base_url=BASE_URL, api_key=API_KEY, retry_unsafe_writes=flag)
    expected_factory = async_factory if client_type is AsyncMemoriaClient else sync_factory
    expected_factory.assert_called_once()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize(
    "timeout", [-1, 0, True, False, math.nan, math.inf, -math.inf, 10**400, "30", None]
)
def test_invalid_timeout_fails_before_allocating_http_client(
    monkeypatch: pytest.MonkeyPatch, client_type: type, timeout: object
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    with pytest.raises(MemoriaValidationError, match="timeout.*positive finite number"):
        client_type(base_url=BASE_URL, api_key=API_KEY, timeout=timeout)
    sync_factory.assert_not_called()
    async_factory.assert_not_called()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("timeout", [0.001, 1, 30.0])
def test_valid_timeout_reaches_http_client_factory(
    monkeypatch: pytest.MonkeyPatch, client_type: type, timeout: float
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    client_type(base_url=BASE_URL, api_key=API_KEY, timeout=timeout)
    expected_factory = async_factory if client_type is AsyncMemoriaClient else sync_factory
    expected_factory.assert_called_once()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("budget", [-1, -100, True, False, 1.5, "2", None])
def test_invalid_retry_budget_fails_before_allocating_http_client(
    monkeypatch: pytest.MonkeyPatch, client_type: type, budget: object
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    with pytest.raises(MemoriaValidationError, match="max_retries.*non-negative integer"):
        client_type(base_url=BASE_URL, api_key=API_KEY, max_retries=budget)
    sync_factory.assert_not_called()
    async_factory.assert_not_called()


@pytest.mark.parametrize("budget", [0, 1, 3])
def test_sync_retry_budget_keeps_initial_attempt_and_exact_retry_count(
    monkeypatch: pytest.MonkeyPatch, httpx_mock: HTTPXMock, budget: int
) -> None:
    sleep = Mock()
    monkeypatch.setattr("memoria._http.time.sleep", sleep)
    for _ in range(budget + 1):
        httpx_mock.add_response(status_code=503, json={"detail": "busy"})
    with MemoriaClient(BASE_URL, API_KEY, max_retries=budget) as client:
        with pytest.raises(MemoriaServerError):
            client.memories.list()
    assert len(httpx_mock.get_requests()) == budget + 1
    assert sleep.call_count == budget


@pytest.mark.parametrize("budget", [0, 1, 3])
async def test_async_retry_budget_keeps_initial_attempt_and_exact_retry_count(
    monkeypatch: pytest.MonkeyPatch, httpx_mock: HTTPXMock, budget: int
) -> None:
    sleep = AsyncMock()
    monkeypatch.setattr("asyncio.sleep", sleep)
    for _ in range(budget + 1):
        httpx_mock.add_response(status_code=503, json={"detail": "busy"})
    async with AsyncMemoriaClient(BASE_URL, API_KEY, max_retries=budget) as client:
        with pytest.raises(MemoriaServerError):
            await client.memories.list()
    assert len(httpx_mock.get_requests()) == budget + 1
    assert sleep.await_count == budget

@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize(
    "base_url",
    [
        "",
        "   ",
        "localhost:8100",
        "/relative",
        "ftp://memoria.test",
        "http://",
        "https://memoria.test?tenant=one",
        "https://memoria.test?",
        "https://memoria.test#fragment",
        "https://memoria.test#",
        "http://memoria.test:0",
        "http://memoria.test:65536",
        "http://memoria.test:99999",
        None,
        8100,
    ],
)
def test_invalid_base_url_fails_before_allocating_http_client(
    monkeypatch: pytest.MonkeyPatch, client_type: type, base_url: object
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    with pytest.raises(MemoriaValidationError, match="base_url.*absolute HTTP URL"):
        client_type(base_url=base_url, api_key=API_KEY)  # type: ignore[arg-type]
    sync_factory.assert_not_called()
    async_factory.assert_not_called()


@pytest.mark.parametrize("client_type", [MemoriaClient, AsyncMemoriaClient])
@pytest.mark.parametrize("base_url", ["http://localhost:8100", "https://memoria.test/prefix/"])
def test_valid_base_url_reaches_http_client_factory(
    monkeypatch: pytest.MonkeyPatch, client_type: type, base_url: str
) -> None:
    sync_factory = Mock()
    async_factory = Mock()
    monkeypatch.setattr(httpx, "Client", sync_factory)
    monkeypatch.setattr(httpx, "AsyncClient", async_factory)
    client_type(base_url=base_url, api_key=API_KEY)
    expected_factory = async_factory if client_type is AsyncMemoriaClient else sync_factory
    expected_factory.assert_called_once()


