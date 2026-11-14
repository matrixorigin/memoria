"""Malformed successful responses must not silently terminate pagination."""

import pytest

from memoria import MemoriaResponseError
from tests.conftest import MEMORY_STUB

INVALID_PAGES = [{}, {"items": {}}, {"items": ""}, {"items": None}, [], "oops", 42]


@pytest.mark.parametrize("payload", INVALID_PAGES)
@pytest.mark.parametrize("operation", ["list", "query"])
def test_invalid_page_sync(sync_client, httpx_mock, payload, operation):
    httpx_mock.add_response(json=payload)
    kwargs = {"subject_id": "subject"} if operation == "query" else {}
    with pytest.raises(MemoriaResponseError, match="memory page"):
        getattr(sync_client.memories, operation)(**kwargs)
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("payload", INVALID_PAGES)
@pytest.mark.parametrize("operation", ["list", "query"])
async def test_invalid_page_async(async_client, httpx_mock, payload, operation):
    httpx_mock.add_response(json=payload)
    kwargs = {"subject_id": "subject"} if operation == "query" else {}
    with pytest.raises(MemoriaResponseError, match="memory page"):
        await getattr(async_client.memories, operation)(**kwargs)
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("items", [[], [MEMORY_STUB]])
@pytest.mark.parametrize("cursor_fields", [{}, {"next_cursor": None}, {"next_cursor": "a" * 32}])
@pytest.mark.parametrize("operation", ["list", "query"])
def test_valid_page_sync(sync_client, httpx_mock, items, cursor_fields, operation):
    httpx_mock.add_response(json={"items": items, **cursor_fields, "future_field": True})
    kwargs = {"subject_id": "subject"} if operation == "query" else {}
    page = getattr(sync_client.memories, operation)(**kwargs)
    assert [m.memory_id for m in page.items] == [m["memory_id"] for m in items]
    assert page.next_cursor == cursor_fields.get("next_cursor")


@pytest.mark.parametrize("items", [[], [MEMORY_STUB]])
@pytest.mark.parametrize("cursor_fields", [{}, {"next_cursor": None}, {"next_cursor": "a" * 32}])
@pytest.mark.parametrize("operation", ["list", "query"])
async def test_valid_page_async(async_client, httpx_mock, items, cursor_fields, operation):
    httpx_mock.add_response(json={"items": items, **cursor_fields, "future_field": True})
    kwargs = {"subject_id": "subject"} if operation == "query" else {}
    page = await getattr(async_client.memories, operation)(**kwargs)
    assert [m.memory_id for m in page.items] == [m["memory_id"] for m in items]
    assert page.next_cursor == cursor_fields.get("next_cursor")
