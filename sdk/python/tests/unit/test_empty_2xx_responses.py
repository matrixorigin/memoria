"""Empty 2xx bodies and malformed resource payloads raise MemoriaResponseError."""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Iterator
from typing import Any

import httpx
import pytest

from memoria import (
    ApplyResult,
    AsyncMemoriaClient,
    Branch,
    GovernanceResult,
    MemoriaClient,
    MemoriaError,
    MemoriaResponseError,
    Memory,
    MemoryPage,
    ObserveResult,
    Profile,
    PurgeResult,
    RetrieveResult,
    Snapshot,
)


def empty_2xx_response(kind: str) -> httpx.Response:
    if kind == "204_no_content":
        return httpx.Response(204)
    if kind == "200_empty_body":
        return httpx.Response(200, content=b"")
    if kind == "200_empty_json":
        return httpx.Response(
            200, headers={"content-type": "application/json"}, content=b""
        )
    if kind == "200_json_null":
        return httpx.Response(
            200, headers={"content-type": "application/json"}, content=b"null"
        )
    if kind == "200_whitespace_text":
        return httpx.Response(
            200, headers={"content-type": "text/plain"}, content=b"   "
        )
    raise AssertionError(f"unknown response kind: {kind}")


EMPTY_2XX_KINDS = [
    "204_no_content",
    "200_empty_body",
    "200_empty_json",
    "200_json_null",
    "200_whitespace_text",
]


class _DispatchState:
    def __init__(self) -> None:
        self.handler: Callable[[httpx.Request], httpx.Response] = (
            lambda _req: httpx.Response(204)
        )

    def __call__(self, request: httpx.Request) -> httpx.Response:
        return self.handler(request)


@pytest.fixture(scope="module")
def _shared_sync_client() -> Iterator[tuple[MemoriaClient, _DispatchState]]:
    dispatch = _DispatchState()
    with MemoriaClient("https://memoria.test", "synthetic-key", max_retries=0) as client:
        client._http.close()
        client._http = httpx.Client(
            transport=httpx.MockTransport(dispatch),
            headers=client._headers,
            trust_env=False,
        )
        yield client, dispatch


@pytest.fixture(scope="module")
def _shared_async_template() -> AsyncMemoriaClient:
    client = AsyncMemoriaClient("https://memoria.test", "synthetic-key", max_retries=0)
    asyncio.run(client.aclose())
    return client


@pytest.fixture(params=["sync", "async"])
def run_resource(
    request: pytest.FixtureRequest,
    monkeypatch: pytest.MonkeyPatch,
    _shared_sync_client: tuple[MemoriaClient, _DispatchState],
    _shared_async_template: AsyncMemoriaClient,
) -> Any:
    delays: list[float] = []
    monkeypatch.setattr("memoria._http.time.sleep", delays.append)

    async def fake_sleep(delay: float) -> None:
        delays.append(delay)

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)
    sync_client, sync_dispatch = _shared_sync_client

    def invoke(
        handler: Callable[[httpx.Request], httpx.Response],
        caller: Callable[[Any], Any],
        *,
        max_retries: int = 2,
    ) -> Any:
        if request.param == "sync":
            sync_dispatch.handler = handler
            prev_retries = sync_client._max_retries
            sync_client._max_retries = max_retries
            try:
                return caller(sync_client)
            finally:
                sync_client._max_retries = prev_retries

        async def _arun() -> Any:
            prev_retries = _shared_async_template._max_retries
            _shared_async_template._max_retries = max_retries
            try:
                async with httpx.AsyncClient(
                    transport=httpx.MockTransport(handler),
                    headers=_shared_async_template._headers,
                    trust_env=False,
                ) as ahttp:
                    _shared_async_template._ahttp = ahttp
                    return await caller(_shared_async_template)
            finally:
                _shared_async_template._max_retries = prev_retries

        return asyncio.run(_arun())

    return invoke, delays


RESOURCE_PARSE_OPERATIONS: list[tuple[str, Callable[[Any], Any]]] = [
    ("profile.me", lambda c: c.profile.me()),
    ("governance.run", lambda c: c.governance.run()),
    ("governance.consolidate", lambda c: c.governance.consolidate()),
    ("governance.reflect", lambda c: c.governance.reflect()),
    ("snapshots.create", lambda c: c.snapshots.create("snap_1")),
    ("memories.store", lambda c: c.memories.store(content="hello")),
    ("memories.store_batch", lambda c: c.memories.store_batch([{"content": "hello"}])),
    ("memories.retrieve", lambda c: c.memories.retrieve(query="hello")),
    ("memories.search", lambda c: c.memories.search(query="hello")),
    ("memories.fulltext_search", lambda c: c.memories.fulltext_search("hello")),
    ("memories.list", lambda c: c.memories.list()),
    ("memories.query", lambda c: c.memories.query(subject_id="subj_1")),
    ("memories.correct", lambda c: c.memories.correct("mem_1", new_content="updated")),
    (
        "memories.correct_by_query",
        lambda c: c.memories.correct_by_query(query="hello", new_content="updated"),
    ),
    ("memories.purge", lambda c: c.memories.purge(topic="cleanup")),
    ("branches.apply", lambda c: c.branches.apply("exp_1", adds=["mem_1"])),
    ("branches.diff", lambda c: c.branches.diff("exp_1")),
    ("branches.diff_items", lambda c: c.branches.diff_items("exp_1")),
    (
        "branches.pick",
        lambda c: c.branches.pick(
            "exp_1", selector={"type": "key_list", "keys": ["mem_1"]}
        ),
    ),
]


@pytest.mark.parametrize("kind", EMPTY_2XX_KINDS)
@pytest.mark.parametrize(
    "name,caller",
    RESOURCE_PARSE_OPERATIONS,
    ids=[op[0] for op in RESOURCE_PARSE_OPERATIONS],
)
def test_empty_2xx_raises_response_error_without_retry(
    run_resource: Any, kind: str, name: str, caller: Callable[[Any], Any]
) -> None:
    invoke, delays = run_resource
    requests: list[httpx.Request] = []

    def handle(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return empty_2xx_response(kind)

    with pytest.raises(MemoriaResponseError) as caught:
        invoke(handle, caller, max_retries=2)

    assert isinstance(caught.value, MemoriaError)
    assert len(requests) == 1
    assert delays == []


VOID_OPERATIONS: list[tuple[str, Callable[[Any], Any]]] = [
    ("memories.delete", lambda c: c.memories.delete("mem_1")),
    ("memories.feedback", lambda c: c.memories.feedback("mem_1", signal="useful")),
    ("snapshots.rollback", lambda c: c.snapshots.rollback("snap_1")),
    ("snapshots.delete_single", lambda c: c.snapshots.delete("snap_1")),
    ("snapshots.delete_bulk", lambda c: c.snapshots.delete(prefix="pre_")),
    ("branches.checkout", lambda c: c.branches.checkout("exp_1")),
    ("branches.merge", lambda c: c.branches.merge("exp_1")),
    ("branches.delete", lambda c: c.branches.delete("exp_1")),
]


@pytest.mark.parametrize("kind", EMPTY_2XX_KINDS)
@pytest.mark.parametrize(
    "name,caller",
    VOID_OPERATIONS,
    ids=[op[0] for op in VOID_OPERATIONS],
)
def test_void_operations_accept_empty_2xx(
    run_resource: Any, kind: str, name: str, caller: Callable[[Any], Any]
) -> None:
    invoke, delays = run_resource
    requests: list[httpx.Request] = []

    def handle(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return empty_2xx_response(kind)

    assert invoke(handle, caller, max_retries=0) is None
    assert len(requests) == 1
    assert delays == []


@pytest.mark.parametrize("kind", [*EMPTY_2XX_KINDS, "200_ok_text"])
def test_ping_accepts_empty_2xx_sync_and_async(
    _shared_sync_client: tuple[MemoriaClient, _DispatchState],
    _shared_async_template: AsyncMemoriaClient,
    kind: str,
) -> None:
    sync_client, sync_dispatch = _shared_sync_client

    def handle(request: httpx.Request) -> httpx.Response:
        assert request.method == "GET"
        assert request.url.path == "/health"
        if kind == "200_ok_text":
            return httpx.Response(
                200, headers={"content-type": "text/plain"}, content=b"ok"
            )
        return empty_2xx_response(kind)

    sync_dispatch.handler = handle
    assert sync_client.ping() is True

    async def _check_async() -> None:
        async with httpx.AsyncClient(
            transport=httpx.MockTransport(handle),
            headers=_shared_async_template._headers,
            trust_env=False,
        ) as ahttp:
            _shared_async_template._ahttp = ahttp
            assert await _shared_async_template.ping() is True

    asyncio.run(_check_async())


@pytest.mark.parametrize(
    "payload",
    [
        {"not": "a list"},
        "plain text",
        42,
        [None],
        [{"memory_id": "only_id"}],
    ],
)
def test_store_batch_rejects_non_list_or_invalid_elements(
    run_resource: Any, payload: Any
) -> None:
    invoke, _ = run_resource

    def handle(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=payload)

    with pytest.raises(MemoriaResponseError):
        invoke(
            handle,
            lambda c: c.memories.store_batch([{"content": "hello"}]),
            max_retries=0,
        )


@pytest.mark.parametrize(
    "payload",
    [
        "plain text",
        42,
        {"branches": "not-a-list"},
        {"branches": 42},
        {"branches": [None]},
        {"branches": [{}]},
        [None],
    ],
)
def test_branch_list_rejects_malformed_containers_and_items(
    run_resource: Any, payload: Any
) -> None:
    invoke, _ = run_resource

    def handle(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=payload)

    with pytest.raises(MemoriaResponseError):
        invoke(handle, lambda c: c.branches.list(), max_retries=0)


@pytest.mark.parametrize(
    "model_cls",
    [
        Memory,
        MemoryPage,
        PurgeResult,
        Snapshot,
        Branch,
        ApplyResult,
        Profile,
        GovernanceResult,
        ObserveResult,
    ],
)
@pytest.mark.parametrize("bad_input", [None, [], "string", 123, True])
def test_model_from_dict_rejects_non_dict_inputs(
    model_cls: Any, bad_input: Any
) -> None:
    with pytest.raises(MemoriaResponseError):
        model_cls.from_dict(bad_input)


@pytest.mark.parametrize(
    "bad_input",
    [
        None,
        "string",
        123,
        True,
        [None],
        [{}],
        {"results": None},
        {"results": "not-a-list"},
        {"items": None},
        {"results": [], "explain": "not-a-dict"},
    ],
)
def test_retrieve_result_from_dict_rejects_invalid_shapes(bad_input: Any) -> None:
    with pytest.raises(MemoriaResponseError):
        RetrieveResult.from_dict(bad_input)


@pytest.mark.parametrize(
    "call",
    [
        lambda: Memory.from_dict({}),
        lambda: Memory.from_dict({"memory_id": "m1"}),
        lambda: Memory.from_dict({"content": "c1"}),
        lambda: Memory.from_dict({"memory_id": None, "content": "c1"}),
        lambda: Memory.from_dict(
            {"memory_id": "m1", "content": "c1", "initial_confidence": "bad"}
        ),
        lambda: MemoryPage.from_dict({"items": [None]}),
        lambda: MemoryPage.from_dict({"items": [{}]}),
        lambda: PurgeResult.from_dict({"purged": None}),
        lambda: PurgeResult.from_dict({"purged": "bad"}),
        lambda: PurgeResult.from_dict({"purged": float("inf")}),
        lambda: Snapshot.from_dict({}),
        lambda: Snapshot.from_dict({"name": ""}),
        lambda: Snapshot.from_dict({"name": None}),
        lambda: Branch.from_dict({}),
        lambda: Branch.from_dict({"name": ""}),
        lambda: Branch.from_dict({"name": None}),
        lambda: ApplyResult.from_dict({"applied_adds": None}),
        lambda: ApplyResult.from_dict({"applied_adds": "not-a-list"}),
        lambda: Profile.from_dict({"user_id": None}),
        lambda: Profile.from_dict({"profile": None}),
        lambda: Profile.from_dict({"stats": None}),
        lambda: Profile.from_dict({"stats": []}),
        lambda: GovernanceResult.from_dict({"quarantined": None}),
        lambda: GovernanceResult.from_dict({"warnings": None}),
        lambda: GovernanceResult.from_dict({"candidates": "not-a-list"}),
        lambda: ObserveResult.from_dict({"memories": None}),
        lambda: ObserveResult.from_dict({"memories": "not-a-list"}),
        lambda: ObserveResult.from_dict({"memories": [None]}),
    ],
)
def test_model_from_dict_rejects_malformed_fields(call: Callable[[], Any]) -> None:
    with pytest.raises(MemoriaResponseError):
        call()
