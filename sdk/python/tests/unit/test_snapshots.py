"""Unit tests for SnapshotsResource (sync)."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaValidationError
from memoria.models import Snapshot
from tests.conftest import API_KEY, BASE_URL, SNAPSHOT_STUB


@pytest.fixture
def client() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


def test_create_snapshot(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=SNAPSHOT_STUB)
    snap = client.snapshots.create(name="snap-1", description="test")
    assert isinstance(snap, Snapshot)
    assert snap.name == "snap-1"


def test_list_snapshots(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=[SNAPSHOT_STUB, SNAPSHOT_STUB])
    snaps = client.snapshots.list(limit=10)
    assert len(snaps) == 2
    assert all(isinstance(s, Snapshot) for s in snaps)


def test_rollback(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"status": "ok"})
    client.snapshots.rollback("snap-1")  # should not raise


def test_delete_single(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=204)
    client.snapshots.delete("snap-1")  # should not raise


def test_delete_bulk_names(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"deleted": 2})
    client.snapshots.delete(names=["snap-1", "snap-2"])


def test_delete_prefix(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"deleted": 5})
    client.snapshots.delete(prefix="pre_")


def test_delete_older_than(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"deleted": 3})
    client.snapshots.delete(older_than="2026-01-01")


def test_delete_no_selector_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="must provide"):
        client.snapshots.delete()


def test_delete_multiple_selectors_raises(client: MemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="mutually exclusive"):
        client.snapshots.delete("snap-1", prefix="pre_")


@pytest.mark.parametrize("prefix", ["", "   "])
def test_delete_empty_prefix_raises_without_request(
    httpx_mock: HTTPXMock, client: MemoriaClient, prefix: str
) -> None:
    with pytest.raises(MemoriaValidationError, match="prefix must not be empty"):
        client.snapshots.delete(prefix=prefix)
    assert httpx_mock.get_requests() == []


@pytest.mark.asyncio
@pytest.mark.parametrize("prefix", ["", "   "])
async def test_async_delete_empty_prefix_raises_without_request(
    httpx_mock: HTTPXMock, prefix: str
) -> None:
    async with AsyncMemoriaClient(
        base_url=BASE_URL, api_key=API_KEY, max_retries=0
    ) as client:
        with pytest.raises(MemoriaValidationError, match="prefix must not be empty"):
            await client.snapshots.delete(prefix=prefix)
    assert httpx_mock.get_requests() == []
