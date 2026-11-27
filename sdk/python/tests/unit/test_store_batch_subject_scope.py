"""Regression coverage for batch-level subject scope in both SDK clients."""

from __future__ import annotations

from typing import Any

import pytest

from memoria.exceptions import MemoriaResponseError, MemoriaValidationError
from memoria.resources.memories import AsyncMemoriesResource, MemoriesResource


class _RecordingTransport:
    def __init__(self) -> None:
        self.calls: list[tuple[str, str, dict[str, Any]]] = []

    def _request(self, method: str, path: str, *, json: dict[str, Any]) -> list[dict[str, Any]]:
        self.calls.append((method, path, json))
        return [
            {"memory_id": f"m{i}", "content": item["content"]}
            for i, item in enumerate(json["memories"], start=1)
        ]

    async def _arequest(
        self, method: str, path: str, *, json: dict[str, Any]
    ) -> list[dict[str, Any]]:
        return self._request(method, path, json=json)


@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
@pytest.mark.asyncio
async def test_batch_subject_default_preserves_per_item_override(asynchronous: bool) -> None:
    transport = _RecordingTransport()
    resource = AsyncMemoriesResource(transport) if asynchronous else MemoriesResource(transport)
    items = [
        {"content": "one"},
        {"content": "two", "subject_id": "override"},
    ]
    if asynchronous:
        saved = await resource.store_batch(items, subject_id="owner", branch="scenario")
    else:
        saved = resource.store_batch(items, subject_id="owner", branch="scenario")
    assert saved[0].memory_id == "m1"
    assert transport.calls == [
        (
            "POST",
            "/v1/memories/batch",
            {"memories": items, "subject_id": "owner", "branch": "scenario"},
        )
    ]
    assert items[0] == {"content": "one"}
    assert items[1]["subject_id"] == "override"


@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
@pytest.mark.asyncio
async def test_batch_default_payload_is_unchanged(asynchronous: bool) -> None:
    transport = _RecordingTransport()
    resource = AsyncMemoriesResource(transport) if asynchronous else MemoriesResource(transport)
    items = [{"content": "one"}]
    if asynchronous:
        await resource.store_batch(items)
    else:
        resource.store_batch(items)
    assert transport.calls == [("POST", "/v1/memories/batch", {"memories": items})]


@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
@pytest.mark.asyncio
async def test_batch_size_limit_still_rejected_before_request(asynchronous: bool) -> None:
    transport = _RecordingTransport()
    resource = AsyncMemoriesResource(transport) if asynchronous else MemoriesResource(transport)
    with pytest.raises(MemoriaValidationError, match="100"):
        if asynchronous:
            await resource.store_batch([{"content": "one"}] * 101, subject_id="owner")
        else:
            resource.store_batch([{"content": "one"}] * 101, subject_id="owner")
    assert transport.calls == []


@pytest.mark.parametrize("returned", [0, 1, 3])
@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
async def test_batch_rejects_incomplete_or_extra_results(
    returned, asynchronous, sync_client, async_client, httpx_mock
):
    items = [{"content": "first"}, {"content": "second"}]
    rows = [{"memory_id": str(i), "content": "saved"} for i in range(returned)]
    httpx_mock.add_response(status_code=201, json=rows)
    with pytest.raises(MemoriaResponseError, match="store_batch"):
        if asynchronous:
            await async_client.memories.store_batch(items)
        else:
            sync_client.memories.store_batch(items)
    assert len(httpx_mock.get_requests()) == 1


async def test_empty_batch_accepts_empty_receipt(async_client, httpx_mock):
    httpx_mock.add_response(status_code=201, json=[])
    assert await async_client.memories.store_batch([]) == []
