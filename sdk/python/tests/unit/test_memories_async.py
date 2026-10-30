"""Unit tests for AsyncMemoriesResource — mirrors test_memories.py with async/await."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaAuthError, MemoriaValidationError
from memoria.models import Memory, MemoryPage, PurgeResult, RetrieveResult
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB


@pytest.fixture
def client() -> AsyncMemoriaClient:
    return AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest.mark.asyncio
async def test_store_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = await client.memories.store(content="test content")
    assert isinstance(mem, Memory)
    assert mem.memory_id == "mem_abc123"
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories"
    assert "idempotency_key" not in json.loads(request.content)


@pytest.mark.asyncio
async def test_store_with_idempotency_key_retries_on_safe_endpoint(
    httpx_mock: HTTPXMock, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("memoria._http._backoff", lambda _: 0)
    httpx_mock.add_response(status_code=503, text="temporarily unavailable")
    httpx_mock.add_response(status_code=201, json=MEMORY_STUB)

    async with AsyncMemoriaClient(
        base_url=BASE_URL,
        api_key=API_KEY,
        max_retries=1,
    ) as client:
        mem = await client.memories.store(
            content="test content",
            idempotency_key="session-1:turn-2",
        )

    assert mem.memory_id == "mem_abc123"
    requests = httpx_mock.get_requests()
    assert len(requests) == 2
    assert all(request.url.path == "/v1/memories/idempotent" for request in requests)
    assert all(
        json.loads(request.content)["idempotency_key"] == "session-1:turn-2"
        for request in requests
    )


@pytest.mark.asyncio
async def test_store_batch_over_limit_raises(client: AsyncMemoriaClient) -> None:
    items = [{"content": f"item {i}"} for i in range(101)]
    with pytest.raises(MemoriaValidationError, match="100"):
        await client.memories.store_batch(items)


@pytest.mark.asyncio
async def test_retrieve_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"results": [MEMORY_STUB]})
    result = await client.memories.retrieve(query="hello")
    assert isinstance(result, RetrieveResult)
    assert len(result.items) == 1


@pytest.mark.asyncio
async def test_fulltext_search(
    httpx_mock: HTTPXMock, client: AsyncMemoriaClient
) -> None:
    response = {**MEMORY_STUB, "retrieval_score": 0.75}
    httpx_mock.add_response(json=[response])
    result = await client.memories.fulltext_search(
        "MatrixOne", extra_metadata_filter={"scene": "incident"}, limit=10
    )
    assert isinstance(result, RetrieveResult)
    assert result.items[0].retrieval_score == 0.75
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/fulltext-search"


@pytest.mark.asyncio
async def test_fulltext_search_rejects_invalid_runtime_types(
    client: AsyncMemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="string"):
        await client.memories.fulltext_search(123)  # type: ignore[arg-type]
    with pytest.raises(MemoriaValidationError, match="limit"):
        await client.memories.fulltext_search("valid", limit=True)
    with pytest.raises(MemoriaValidationError, match="dictionary"):
        await client.memories.fulltext_search(
            "valid", extra_metadata_filter=[]  # type: ignore[arg-type]
        )
    with pytest.raises(MemoriaValidationError, match="session_id"):
        await client.memories.fulltext_search("valid", session_id="   ")
    with pytest.raises(MemoriaValidationError, match="memory_types"):
        await client.memories.fulltext_search("valid", memory_types=["   "])


@pytest.mark.asyncio
async def test_list_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [MEMORY_STUB], "next_cursor": "cursor_xyz"})
    page = await client.memories.list(limit=10)
    assert isinstance(page, MemoryPage)
    assert page.next_cursor == "cursor_xyz"


@pytest.mark.asyncio
async def test_structured_query(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    response = {**MEMORY_STUB, "extra_metadata": {"scene": "incident"}}
    httpx_mock.add_response(json={"items": [response], "next_cursor": None})
    page = await client.memories.query(
        extra_metadata_filter={"scene": "incident"}, trust_tier="T2"
    )
    assert page.items[0].extra_metadata == {"scene": "incident"}
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/query"


@pytest.mark.asyncio
async def test_structured_query_accepts_branch_only(
    httpx_mock: HTTPXMock, client: AsyncMemoriaClient
) -> None:
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    page = await client.memories.query(branch="experiment")
    assert page.items == []


@pytest.mark.asyncio
async def test_structured_query_rejects_blank_supplied_selector(
    client: AsyncMemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="subject_id"):
        await client.memories.query(
            extra_metadata_filter={"scene": "incident"}, subject_id="   "
        )


@pytest.mark.asyncio
async def test_structured_query_rejects_invalid_runtime_types(
    client: AsyncMemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="limit must be an integer"):
        await client.memories.query(
            subject_id="subject",
            limit="1",  # type: ignore[arg-type]
        )
    with pytest.raises(MemoriaValidationError, match="must be a dictionary"):
        await client.memories.query(
            subject_id="subject",
            extra_metadata_filter=[("scene", "incident")],  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_purge_by_ids(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"purged": 1, "snapshot_name": "snap_x"})
    result = await client.memories.purge(memory_ids=["id1"], reason="done")
    assert isinstance(result, PurgeResult)
    assert result.purged == 1


@pytest.mark.asyncio
async def test_401_raises_auth_error(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(status_code=401, json={"detail": "rate limit exceeded"})
    with pytest.raises(MemoriaAuthError):
        await client.memories.store(content="x")


@pytest.mark.asyncio
async def test_session_scope_without_session_id_raises(client: AsyncMemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="session_id"):
        await client.memories.search(query="x", session_scope="only")
