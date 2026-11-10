"""Public lifecycle methods preserve scope and cursor contracts in both clients."""

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, DisabledMemory, DisabledMemoryPage, MemoriaClient
from tests.conftest import MEMORY_STUB


def test_disable_returns_public_reason_and_time(
    httpx_mock: HTTPXMock, sync_client: MemoriaClient
) -> None:
    httpx_mock.add_response(
        json={
            **MEMORY_STUB,
            "is_active": False,
            "disabled_at": "2026-01-01T00:00:00Z",
            "disabled_reason": "reviewed",
        }
    )
    result = sync_client.memories.disable("mem_abc123", reason="reviewed")
    assert isinstance(result, DisabledMemory)
    assert result.memory.is_active is False
    assert result.disabled_reason == "reviewed"
    assert result.disabled_at is not None


@pytest.mark.asyncio
async def test_async_disable_returns_public_reason_and_time(
    httpx_mock: HTTPXMock, async_client: AsyncMemoriaClient
) -> None:
    httpx_mock.add_response(
        json={
            **MEMORY_STUB,
            "is_active": False,
            "disabled_at": "2026-01-01T00:00:00Z",
            "disabled_reason": "reviewed",
        }
    )
    result = await async_client.memories.disable("mem_abc123", reason="reviewed")
    assert isinstance(result, DisabledMemory)
    assert result.memory.is_active is False
    assert result.disabled_reason == "reviewed"
    assert result.disabled_at is not None


@pytest.mark.parametrize("operation", ["disable", "restore", "erase"])
def test_lifecycle_methods_keep_scope(
    httpx_mock: HTTPXMock, sync_client: MemoriaClient, operation: str
) -> None:
    httpx_mock.add_response(
        status_code=204 if operation == "erase" else 200,
        json=None if operation == "erase" else MEMORY_STUB,
    )
    method = "erase_one" if operation == "erase" else operation
    getattr(sync_client.memories, method)("mem_abc123", subject_id="subject-a", branch="review")
    request = httpx_mock.get_request()
    assert request is not None
    assert request.method == "POST"
    assert request.url.path == f"/v1/memories/mem_abc123/{operation}"
    assert json.loads(request.content) == {"subject_id": "subject-a", "branch": "review"}


def test_disabled_pages_forward_opaque_cursor(
    httpx_mock: HTTPXMock, sync_client: MemoriaClient
) -> None:
    cursor = '["2026-01-01T00:00:00Z","memory-1"]'
    httpx_mock.add_response(
        json={"items": [{**MEMORY_STUB, "disabled_reason": "rejected"}], "next_cursor": cursor}
    )
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    page = sync_client.memories.list_disabled(subject_id="subject-a", branch="review", limit=1)
    assert isinstance(page, DisabledMemoryPage)
    assert page.items[0].disabled_reason == "rejected"
    assert page.next_cursor == cursor
    assert sync_client.memories.list_disabled(cursor=page.next_cursor).items == []
    request = httpx_mock.get_requests()[1]
    assert request.url.params["cursor"] == cursor


@pytest.mark.asyncio
@pytest.mark.parametrize("operation", ["disable", "restore", "erase"])
async def test_async_lifecycle_methods_keep_scope(
    httpx_mock: HTTPXMock, async_client: AsyncMemoriaClient, operation: str
) -> None:
    httpx_mock.add_response(
        status_code=204 if operation == "erase" else 200,
        json=None if operation == "erase" else MEMORY_STUB,
    )
    method = "erase_one" if operation == "erase" else operation
    await getattr(async_client.memories, method)(
        "mem_abc123", subject_id="subject-a", branch="review"
    )
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == f"/v1/memories/mem_abc123/{operation}"
    assert json.loads(request.content) == {"subject_id": "subject-a", "branch": "review"}


@pytest.mark.asyncio
async def test_async_disabled_page(httpx_mock: HTTPXMock, async_client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    page = await async_client.memories.list_disabled(
        subject_id="subject-a", branch="review", cursor="opaque"
    )
    assert isinstance(page, DisabledMemoryPage)
    assert page.items == []
    request = httpx_mock.get_request()
    assert request is not None
    assert dict(request.url.params) == {
        "subject_id": "subject-a",
        "branch": "review",
        "cursor": "opaque",
    }


@pytest.mark.asyncio
async def test_async_scoped_erase_does_not_shadow_single_memory_erase(
    httpx_mock: HTTPXMock, async_client: AsyncMemoriaClient
) -> None:
    httpx_mock.add_response(status_code=204)
    httpx_mock.add_response(
        json={"memories": 2, "snapshots_dropped": 0, "snapshots_retained": True}
    )
    await async_client.memories.erase_one("mem_abc123", subject_id="subject-a", branch="main")
    result = await async_client.memories.erase(mode="subject", subject_id="subject-a")
    assert result.memories == 2
    assert result.snapshots_retained
    requests = httpx_mock.get_requests()
    assert requests[0].url.path == "/v1/memories/mem_abc123/erase"
    assert requests[1].url.path == "/v1/memories/erase"
    assert json.loads(requests[1].content) == {"mode": "subject", "subject_id": "subject-a"}
