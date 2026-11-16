"""observe() rejects 2xx bodies that are not JSON objects instead of inventing a result."""

import httpx
import pytest

from memoria import MemoriaResponseError

MESSAGES = [{"role": "user", "content": "hi"}]

# (label, response) pairs for every body shape reported in #498 plus the empty forms.
INVALID_BODIES = {
    "list": httpx.Response(200, json=["mem_1"]),
    "empty-list": httpx.Response(200, json=[]),
    "string": httpx.Response(200, json="text"),
    "number": httpx.Response(200, json=42),
    "false": httpx.Response(200, json=False),
    "json-null": httpx.Response(200, json=None),
    "empty-200": httpx.Response(200, content=b""),
    "204": httpx.Response(204),
    "memories-string": httpx.Response(200, json={"memories": "abc"}),
    "memories-object": httpx.Response(200, json={"memories": {}}),
}


def _respond(httpx_mock, kind):
    resp = INVALID_BODIES[kind]
    httpx_mock.add_response(
        status_code=resp.status_code, content=resp.content, headers=resp.headers
    )


def _call(client, method):
    if method == "observe":
        return client.observe(MESSAGES)
    return client.observe_deduplicated(MESSAGES, exclude_memory_ids=["m1"])


@pytest.mark.parametrize("method", ["observe", "observe_deduplicated"])
@pytest.mark.parametrize("kind", INVALID_BODIES)
def test_invalid_observe_body_sync(sync_client, httpx_mock, kind, method):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="Invalid observe response"):
        _call(sync_client, method)


@pytest.mark.parametrize("method", ["observe", "observe_deduplicated"])
@pytest.mark.parametrize("kind", INVALID_BODIES)
async def test_invalid_observe_body_async(async_client, httpx_mock, kind, method):
    _respond(httpx_mock, kind)
    with pytest.raises(MemoriaResponseError, match="Invalid observe response"):
        await _call(async_client, method)


@pytest.mark.parametrize("method", ["observe", "observe_deduplicated"])
def test_null_memories_field_is_empty_sync(sync_client, httpx_mock, method):
    httpx_mock.add_response(json={"memories": None, "warning": "w"})
    result = _call(sync_client, method)
    assert result.memories == []
    assert result.warning == "w"


@pytest.mark.parametrize("method", ["observe", "observe_deduplicated"])
async def test_null_memories_field_is_empty_async(async_client, httpx_mock, method):
    httpx_mock.add_response(json={"memories": None})
    assert (await _call(async_client, method)).memories == []


@pytest.mark.parametrize("body", [{}, {"memories": []}, {"memories": [{"memory_id": "m1"}]}])
def test_valid_observe_bodies_still_parse(sync_client, httpx_mock, body):
    httpx_mock.add_response(json=body)
    assert sync_client.observe(MESSAGES).memories == body.get("memories", [])
