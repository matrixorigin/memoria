"""Public lifecycle methods preserve scope and cursor contracts in both clients."""

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, DisabledMemoryPage, MemoriaClient
from tests.conftest import MEMORY_STUB


@pytest.mark.parametrize("operation", ["disable", "restore", "erase"])
def test_lifecycle_methods_keep_scope(
    httpx_mock: HTTPXMock, sync_client: MemoriaClient, operation: str
) -> None:
    httpx_mock.add_response(
        status_code=204 if operation == "erase" else 200,
        json=None if operation == "erase" else MEMORY_STUB,
    )
    getattr(sync_client.memories, operation)("mem_abc123", subject_id="subject-a", branch="review")
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
    await getattr(async_client.memories, operation)(
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
