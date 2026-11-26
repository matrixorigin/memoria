"""Empty or non-object 2xx bodies raise MemoriaResponseError, never a raw AttributeError."""

import httpx
import pytest

from memoria import MemoriaResponseError
from tests.conftest import BRANCH_STUB, MEMORY_STUB, SNAPSHOT_STUB

EMPTY_BODIES = {
    "204": httpx.Response(204),
    "empty-200": httpx.Response(200, content=b""),
    # httpx.Response(200, json=None) sends an empty body, so spell out the literal.
    "json-null": httpx.Response(
        200, content=b"null", headers={"content-type": "application/json"}
    ),
}

# (resource, method, kwargs) for every endpoint whose contract is a JSON object.
OBJECT_OPERATIONS = [
    ("profile", "me", {}),
    ("memories", "purge", {"topic": "t"}),
    ("memories", "correct", {"id": "m1", "new_content": "x"}),
    ("memories", "retrieve", {"query": "q"}),
    ("memories", "search", {"query": "q"}),
    ("snapshots", "create", {"name": "s"}),
    ("branches", "apply", {"name": "b", "adds": ["m1"]}),
    ("governance", "run", {}),
    ("governance", "consolidate", {}),
    ("governance", "reflect", {}),
]


def _respond(httpx_mock, kind):
    resp = EMPTY_BODIES[kind]
    httpx_mock.add_response(
        status_code=resp.status_code, content=resp.content, headers=resp.headers
    )


@pytest.mark.parametrize("kind", EMPTY_BODIES)
@pytest.mark.parametrize("resource,method,kwargs", OBJECT_OPERATIONS)
def test_empty_body_sync(sync_client, httpx_mock, kind, resource, method, kwargs):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="expected a JSON object"):
        getattr(getattr(sync_client, resource), method)(**kwargs)


@pytest.mark.parametrize("kind", EMPTY_BODIES)
@pytest.mark.parametrize("resource,method,kwargs", OBJECT_OPERATIONS)
async def test_empty_body_async(async_client, httpx_mock, kind, resource, method, kwargs):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="expected a JSON object"):
        await getattr(getattr(async_client, resource), method)(**kwargs)


@pytest.mark.parametrize("kind", EMPTY_BODIES)
def test_store_batch_empty_body_sync(sync_client, httpx_mock, kind):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="expected a JSON array"):
        sync_client.memories.store_batch([{"content": "x"}])


@pytest.mark.parametrize("kind", EMPTY_BODIES)
async def test_store_batch_empty_body_async(async_client, httpx_mock, kind):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="expected a JSON array"):
        await async_client.memories.store_batch([{"content": "x"}])


@pytest.mark.parametrize("payload", [[], "oops", 42])
def test_non_object_body_is_rejected(sync_client, httpx_mock, payload):
    httpx_mock.add_response(json=payload)
    with pytest.raises(MemoriaResponseError, match="expected a JSON object"):
        sync_client.profile.me()


def test_valid_object_bodies_still_parse(sync_client, httpx_mock):
    httpx_mock.add_response(json={"user_id": "u", "profile": "p", "stats": {}})
    assert sync_client.profile.me().user_id == "u"
    httpx_mock.add_response(json={"purged": 2})
    assert sync_client.memories.purge(topic="t").purged == 2
    httpx_mock.add_response(json=SNAPSHOT_STUB)
    assert sync_client.snapshots.create("snap-1").name == "snap-1"
    httpx_mock.add_response(json=[MEMORY_STUB])
    assert sync_client.memories.store_batch([{"content": "x"}])[0].memory_id == "mem_abc123"
    httpx_mock.add_response(json=BRANCH_STUB)
    assert sync_client.branches.create("experiment-1").name == "experiment-1"


MALFORMED_RETRIEVE_ENVELOPES = [
    {},
    {"error": "upstream failed"},
    {"results": {}},
    {"results": ""},
    {"results": None},
    {"items": {}},
    {"items": ""},
    {"items": None},
]


@pytest.mark.parametrize("method", ["retrieve", "search"])
@pytest.mark.parametrize("payload", MALFORMED_RETRIEVE_ENVELOPES)
def test_retrieve_rejects_malformed_envelopes_sync(
    sync_client, httpx_mock, method, payload
):
    httpx_mock.add_response(json=payload)
    with pytest.raises(MemoriaResponseError, match="results or items array"):
        getattr(sync_client.memories, method)(query="remember")


@pytest.mark.parametrize("method", ["retrieve", "search"])
@pytest.mark.parametrize("payload", MALFORMED_RETRIEVE_ENVELOPES)
async def test_retrieve_rejects_malformed_envelopes_async(
    async_client, httpx_mock, method, payload
):
    httpx_mock.add_response(json=payload)
    with pytest.raises(MemoriaResponseError, match="results or items array"):
        await getattr(async_client.memories, method)(query="remember")


@pytest.mark.parametrize(
    "payload",
    [[], {"results": []}, {"items": []}, {"results": [], "items": {}}],
)
def test_retrieve_preserves_valid_empty_envelopes(sync_client, httpx_mock, payload):
    httpx_mock.add_response(json=payload)
    assert sync_client.memories.retrieve(query="remember").items == []


def test_retrieve_preserves_items_compatibility_envelope(sync_client, httpx_mock):
    httpx_mock.add_response(json={"items": [MEMORY_STUB]})
    result = sync_client.memories.retrieve(query="remember")
    assert [item.memory_id for item in result.items] == [MEMORY_STUB["memory_id"]]
