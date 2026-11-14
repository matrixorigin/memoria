"""Snapshot listings must not silently substitute or discard response data."""

from __future__ import annotations

from typing import Any

import pytest

from memoria.exceptions import MemoriaConnectionError
from memoria.resources.snapshots import AsyncSnapshotsResource, SnapshotsResource


class StubTransport:
    def __init__(self, response: Any) -> None:
        self.response = response

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        assert method == "GET" and path == "/v1/snapshots"
        assert kwargs["params"] == {"limit": 20, "offset": 0}
        return self.response

    async def _arequest(self, method: str, path: str, **kwargs: Any) -> Any:
        return self._request(method, path, **kwargs)


VALID = [
    (None, []),  # 204 No Content
    ([], []),  # legacy list payload
    ({"snapshots": None}, []),  # historical null compatibility
    ({"snapshots": []}, []),
    ({"items": [{"name": "legacy"}]}, ["legacy"]),
    ({"snapshots": [{"name": "current"}]}, ["current"]),
    ({"snapshots": [], "items": [{"name": "stale"}]}, []),
]

BAD = [
    {"error": "upstream response changed"},
    {"snapshots": "not a list"},
    {"snapshots": {"name": "not a list"}},
    {"snapshots": [{"name": "fine"}, {"id": "missing-name"}]},
    {"snapshots": [{"name": 123}]},
    "not a snapshot list",
]


@pytest.mark.parametrize("payload,expected", VALID)
def test_snapshot_list_envelope_sync_valid(payload: Any, expected: list[str]) -> None:
    snapshots = SnapshotsResource(StubTransport(payload)).list()  # type: ignore[arg-type]
    assert [snapshot.name for snapshot in snapshots] == expected


@pytest.mark.parametrize("payload,expected", VALID)
@pytest.mark.asyncio
async def test_snapshot_list_envelope_async_valid(payload: Any, expected: list[str]) -> None:
    snapshots = await AsyncSnapshotsResource(StubTransport(payload)).list()  # type: ignore[arg-type]
    assert [snapshot.name for snapshot in snapshots] == expected


@pytest.mark.parametrize("payload", BAD)
def test_snapshot_list_envelope_sync_invalid(payload: Any) -> None:
    with pytest.raises(MemoriaConnectionError, match="snapshot"):
        SnapshotsResource(StubTransport(payload)).list()  # type: ignore[arg-type]


@pytest.mark.parametrize("payload", BAD)
@pytest.mark.asyncio
async def test_snapshot_list_envelope_async_invalid(payload: Any) -> None:
    with pytest.raises(MemoriaConnectionError, match="snapshot"):
        await AsyncSnapshotsResource(StubTransport(payload)).list()  # type: ignore[arg-type]
