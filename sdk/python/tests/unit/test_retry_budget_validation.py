"""Retry budgets are configuration, not a source of phantom connection failures."""

from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaServerError
from memoria.exceptions import MemoriaValidationError
from tests.conftest import API_KEY, BASE_URL


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
