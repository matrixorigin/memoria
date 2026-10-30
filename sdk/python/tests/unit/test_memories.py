"""Unit tests for MemoriesResource (sync)."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import (
    MemoriaAuthError,
    MemoriaClient,
    MemoriaForbiddenError,
    MemoriaNotFoundError,
    MemoriaUnprocessableError,
    MemoriaValidationError,
)
from memoria.models import Memory, MemoryPage, PurgeResult, RetrieveResult
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB


@pytest.fixture
def client() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


# ---------------------------------------------------------------------------
# store
# ---------------------------------------------------------------------------


def test_store_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = client.memories.store(content="test content")
    assert isinstance(mem, Memory)
    assert mem.memory_id == "mem_abc123"
    assert mem.content == "test content"
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories"
    assert "idempotency_key" not in json.loads(request.content)


def test_store_with_idempotency_key_retries_on_safe_endpoint(
    httpx_mock: HTTPXMock, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("memoria._http._backoff", lambda _: 0)
    httpx_mock.add_response(status_code=503, text="temporarily unavailable")
    httpx_mock.add_response(status_code=201, json=MEMORY_STUB)

    with MemoriaClient(
        base_url=BASE_URL,
        api_key=API_KEY,
        max_retries=1,
    ) as client:
        mem = client.memories.store(
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


def test_memory_preserves_legacy_positional_field_order() -> None:
    memory = Memory(
        "mem_legacy",
        "legacy content",
        "semantic",
        "T3",
        0.65,
        True,
        "user_1",
        "author_1",
        "session_1",
        None,
        None,
        0.75,
    )

    assert memory.author_id == "author_1"
    assert memory.session_id == "session_1"
    assert memory.retrieval_score == 0.75
    assert memory.subject_id is None
    assert memory.extra_metadata is None


def test_store_with_all_params(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = client.memories.store(
        content="test",
        memory_type="profile",
        session_id="sess_1",
        trust_tier="T1",
        branch="my-branch",
    )
    assert mem.memory_id == "mem_abc123"
    req = httpx_mock.get_request()
    assert req is not None
    body = json.loads(req.content)
    assert body["memory_type"] == "profile"
    assert body["session_id"] == "sess_1"
    assert body["trust_tier"] == "T1"
    assert body["branch"] == "my-branch"


def test_store_401_raises_auth_error(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=401, json={"detail": "invalid key"})
    with pytest.raises(MemoriaAuthError) as exc:
        client.memories.store(content="x")
    assert exc.value.status_code == 401


def test_store_403_raises_forbidden(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        status_code=403, json={"detail": "main is read-only in group mode"}
    )
    with pytest.raises(MemoriaForbiddenError):
        client.memories.store(content="x")


def test_store_422_raises_unprocessable(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=422, json={"detail": "content exceeds 32 KiB"})
    with pytest.raises(MemoriaUnprocessableError) as exc:
        client.memories.store(content="x")
    assert "32 KiB" in exc.value.detail


# ---------------------------------------------------------------------------
# store_batch
# ---------------------------------------------------------------------------


def test_store_batch_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=[MEMORY_STUB, MEMORY_STUB])
    mems = client.memories.store_batch(
        [{"content": "a"}, {"content": "b"}]
    )
    assert len(mems) == 2
    # Verify the request body uses "memories" (not "items") to match the server contract
    req = httpx_mock.get_request()
    assert req is not None
    body = json.loads(req.content)
    assert "memories" in body
    assert "items" not in body


def test_store_batch_over_limit_raises_validation_error(client: MemoriaClient) -> None:
    items = [{"content": f"item {i}"} for i in range(101)]
    with pytest.raises(MemoriaValidationError, match="100"):
        client.memories.store_batch(items)


# ---------------------------------------------------------------------------
# retrieve
# ---------------------------------------------------------------------------


def test_retrieve_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"results": [MEMORY_STUB], "explain": None})
    result = client.memories.retrieve(query="test")
    assert isinstance(result, RetrieveResult)
    assert len(result.items) == 1


def test_retrieve_session_scope_without_session_id_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="session_id"):
        client.memories.retrieve(query="x", session_scope="prefer")


def test_retrieve_with_explain(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        json={"results": [MEMORY_STUB], "explain": {"path": "hybrid", "elapsed_ms": 12}}
    )
    result = client.memories.retrieve(query="test", explain=True)
    assert result.explain is not None
    assert result.explain["path"] == "hybrid"


def test_fulltext_search_with_structured_filters(
    httpx_mock: HTTPXMock, client: MemoriaClient
) -> None:
    response = {
        **MEMORY_STUB,
        "retrieval_score": 1.25,
        "subject_id": "subject_1",
        "extra_metadata": {"scene": "incident", "rank": 2},
    }
    httpx_mock.add_response(json=[response])
    result = client.memories.fulltext_search(
        "MatrixOne database",
        extra_metadata_filter={"scene": "incident", "rank": 2},
        subject_id="subject_1",
        memory_types=["semantic", " semantic ", "semantic"],
        session_id="session_1",
        trust_tier="T2",
        branch="experiment",
        limit=20,
    )
    assert isinstance(result, RetrieveResult)
    assert result.items[0].retrieval_score == 1.25
    assert result.items[0].subject_id == "subject_1"
    assert result.items[0].extra_metadata == {"scene": "incident", "rank": 2}
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/fulltext-search"
    body = json.loads(request.content)
    assert body["extra_metadata_filter"] == {"scene": "incident", "rank": 2}
    assert body["memory_types"] == ["semantic"]
    assert body["branch"] == "experiment"


@pytest.mark.parametrize("query", ["", "!!!"])
def test_fulltext_search_rejects_empty_token_query(
    client: MemoriaClient, query: str
) -> None:
    with pytest.raises(MemoriaValidationError, match="query"):
        client.memories.fulltext_search(query)


def test_fulltext_search_rejects_invalid_limit_and_metadata(
    client: MemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="100"):
        client.memories.fulltext_search("valid", limit=101)
    with pytest.raises(MemoriaValidationError, match="finite"):
        client.memories.fulltext_search(
            "valid", extra_metadata_filter={"rank": float("nan")}
        )


@pytest.mark.parametrize("query", [123, None])
def test_fulltext_search_rejects_non_string_query(
    client: MemoriaClient, query: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="string"):
        client.memories.fulltext_search(query)  # type: ignore[arg-type]


@pytest.mark.parametrize("limit", [True, "10"])
def test_fulltext_search_rejects_non_integer_limit(
    client: MemoriaClient, limit: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="limit"):
        client.memories.fulltext_search("valid", limit=limit)  # type: ignore[arg-type]


def test_fulltext_search_rejects_oversized_utf8_query(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="4096 bytes"):
        client.memories.fulltext_search("界" * 1366)


def test_fulltext_search_translates_invalid_query_unicode(
    client: MemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="valid UTF-8"):
        client.memories.fulltext_search("\ud800valid")


@pytest.mark.parametrize("metadata", [[], {1: "value"}])
def test_fulltext_search_rejects_invalid_metadata_container_or_key(
    client: MemoriaClient, metadata: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="dictionary|keys must be strings"):
        client.memories.fulltext_search(
            "valid", extra_metadata_filter=metadata  # type: ignore[arg-type]
        )


def test_fulltext_search_reports_metadata_key_byte_limit(
    client: MemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="64 bytes"):
        client.memories.fulltext_search(
            "valid", extra_metadata_filter={"a" * 65: "value"}
        )


@pytest.mark.parametrize(
    "value", [10**5000, "\ud800"], ids=["large-integer", "lone-surrogate"]
)
def test_fulltext_search_translates_metadata_serialization_errors(
    client: MemoriaClient, value: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="valid JSON scalars"):
        client.memories.fulltext_search("valid", extra_metadata_filter={"value": value})


@pytest.mark.parametrize(
    "memory_types",
    [["unknown"], [1], "semantic", ["   "], ["semantic", ""]],
)
def test_fulltext_search_rejects_invalid_memory_types(
    client: MemoriaClient, memory_types: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="memory_types|memory type"):
        client.memories.fulltext_search(
            "valid", memory_types=memory_types  # type: ignore[arg-type]
        )


@pytest.mark.parametrize(
    ("field", "kwargs"),
    [
        ("subject_id", {"subject_id": "   "}),
        ("session_id", {"session_id": "   "}),
        ("trust_tier", {"trust_tier": "   "}),
        ("branch", {"branch": "   "}),
    ],
)
def test_fulltext_search_rejects_blank_structured_filter(
    client: MemoriaClient, field: str, kwargs: dict[str, str]
) -> None:
    with pytest.raises(MemoriaValidationError, match=field):
        client.memories.fulltext_search("valid", **kwargs)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------


def test_list_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [MEMORY_STUB], "next_cursor": None})
    page = client.memories.list()
    assert isinstance(page, MemoryPage)
    assert len(page.items) == 1
    assert page.next_cursor is None


def test_list_with_cursor(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    page = client.memories.list(limit=10, cursor="abc123", memory_type="semantic")
    assert page.items == []


def test_list_over_max_limit_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="500"):
        client.memories.list(limit=501)


def test_structured_query(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    response = {
        **MEMORY_STUB,
        "subject_id": "subject_1",
        "extra_metadata": {"scene": "incident", "rank": 2},
    }
    httpx_mock.add_response(json={"items": [response], "next_cursor": "next_id"})

    page = client.memories.query(
        extra_metadata_filter={"scene": "incident", "rank": 2},
        subject_id="subject_1",
        memory_types=["semantic", " semantic ", "semantic"],
        limit=10,
    )

    assert page.items[0].subject_id == "subject_1"
    assert page.items[0].extra_metadata == {"scene": "incident", "rank": 2}
    assert page.next_cursor == "next_id"
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/query"
    body = json.loads(request.content)
    assert body["extra_metadata_filter"] == {"scene": "incident", "rank": 2}
    assert body["memory_types"] == ["semantic"]
    assert "query" not in body


def test_structured_query_requires_selector(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="selector"):
        client.memories.query()


@pytest.mark.parametrize("limit", ["1", 1.5, True, None])
def test_structured_query_rejects_non_integer_limit(client: MemoriaClient, limit: object) -> None:
    with pytest.raises(MemoriaValidationError, match="limit must be an integer"):
        client.memories.query(subject_id="subject", limit=limit)  # type: ignore[arg-type]


@pytest.mark.parametrize("extra_metadata_filter", [[("scene", "incident")], "scene=incident"])
def test_structured_query_rejects_non_dictionary_metadata_filter(
    client: MemoriaClient, extra_metadata_filter: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="must be a dictionary"):
        client.memories.query(
            subject_id="subject",
            extra_metadata_filter=extra_metadata_filter,  # type: ignore[arg-type]
        )


def test_structured_query_accepts_branch_only(
    httpx_mock: HTTPXMock, client: MemoriaClient
) -> None:
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    page = client.memories.query(branch="experiment")
    assert page.items == []
    request = httpx_mock.get_request()
    assert request is not None
    assert json.loads(request.content)["branch"] == "experiment"


def test_structured_query_rejects_nested_metadata(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="strings, numbers, or booleans"):
        client.memories.query(extra_metadata_filter={"nested": {"key": "value"}})


def test_structured_query_rejects_invalid_key_and_oversized_value(
    client: MemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="must start"):
        client.memories.query(extra_metadata_filter={"1scene": "incident"})
    with pytest.raises(MemoriaValidationError, match="1024"):
        client.memories.query(extra_metadata_filter={"scene": "x" * 1025})


def test_structured_query_reports_metadata_key_byte_limit(
    client: MemoriaClient,
) -> None:
    with pytest.raises(MemoriaValidationError, match="64 bytes"):
        client.memories.query(extra_metadata_filter={"a" * 65: "incident"})


@pytest.mark.parametrize("memory_types", [["unknown"], [1], "semantic"])
def test_structured_query_rejects_invalid_memory_types(
    client: MemoriaClient, memory_types: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="memory_types|memory type"):
        client.memories.query(memory_types=memory_types)  # type: ignore[arg-type]


@pytest.mark.parametrize(
    "kwargs",
    [
        {"subject_id": "   "},
        {"session_id": "   "},
        {"trust_tier": "   "},
        {"branch": "   "},
        {"memory_types": []},
        {"memory_types": ["semantic", "   "]},
    ],
)
def test_structured_query_rejects_blank_supplied_selector(
    client: MemoriaClient, kwargs: dict[str, object]
) -> None:
    with pytest.raises(MemoriaValidationError, match="empty|at least one"):
        client.memories.query(
            extra_metadata_filter={"scene": "incident"},
            **kwargs,  # type: ignore[arg-type]
        )


@pytest.mark.parametrize(
    "value", [10**5000, "\ud800"], ids=["large-integer", "lone-surrogate"]
)
def test_structured_query_translates_metadata_serialization_errors(
    client: MemoriaClient, value: object
) -> None:
    with pytest.raises(MemoriaValidationError, match="valid JSON scalars"):
        client.memories.query(extra_metadata_filter={"value": value})


@pytest.mark.parametrize("value", [float("nan"), float("inf"), float("-inf")])
def test_structured_query_rejects_non_finite_metadata_number(
    client: MemoriaClient, value: float
) -> None:
    with pytest.raises(MemoriaValidationError, match="finite"):
        client.memories.query(extra_metadata_filter={"rank": value})


# ---------------------------------------------------------------------------
# correct / correct_by_query
# ---------------------------------------------------------------------------


def test_correct_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    updated = {**MEMORY_STUB, "content": "updated content"}
    httpx_mock.add_response(json=updated)
    mem = client.memories.correct("mem_abc123", new_content="updated content", reason="fix")
    assert mem.content == "updated content"


def test_correct_404_raises_not_found(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=404, json={"detail": "not found"})
    with pytest.raises(MemoriaNotFoundError):
        client.memories.correct("missing_id", new_content="x")


def test_correct_by_query_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = client.memories.correct_by_query(
        query="old content", new_content="new content", reason="user correction"
    )
    assert mem.memory_id == "mem_abc123"


def test_correct_by_query_session_scope_without_id(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="session_id"):
        client.memories.correct_by_query(
            query="x", new_content="y", session_scope="only"
        )


# ---------------------------------------------------------------------------
# delete
# ---------------------------------------------------------------------------


def test_delete_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=204)
    client.memories.delete("mem_abc123", reason="done")  # should not raise


# ---------------------------------------------------------------------------
# purge
# ---------------------------------------------------------------------------


def test_purge_by_ids(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        json={"purged": 2, "snapshot_name": "pre_auto_snap", "warning": None}
    )
    result = client.memories.purge(memory_ids=["id1", "id2"], reason="cleanup")
    assert isinstance(result, PurgeResult)
    assert result.purged == 2
    assert result.snapshot_name == "pre_auto_snap"


def test_purge_by_topic(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"purged": 5, "snapshot_name": None})
    result = client.memories.purge(topic="debug session", reason="done")
    assert result.purged == 5


def test_purge_by_session_with_types(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"purged": 3})
    result = client.memories.purge(
        session_id="sess_abc", memory_types=["working"], reason="session ended"
    )
    assert result.purged == 3


def test_purge_no_selector_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="must specify"):
        client.memories.purge(reason="oops")


def test_purge_multiple_selectors_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="mutually exclusive"):
        client.memories.purge(memory_ids=["id1"], topic="foo")


def test_purge_memory_types_without_session_id_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="session_id"):
        client.memories.purge(topic="x", memory_types=["working"])


# ---------------------------------------------------------------------------
# feedback
# ---------------------------------------------------------------------------


def test_feedback_happy_path(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=204)
    client.memories.feedback("mem_abc123", signal="useful", context="helped answer")
