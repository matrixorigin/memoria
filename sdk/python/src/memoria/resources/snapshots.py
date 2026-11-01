"""snapshots resource — sync and async."""

from __future__ import annotations

import builtins
from typing import TYPE_CHECKING, Any
from urllib.parse import quote

from ..exceptions import MemoriaValidationError
from ..models import Snapshot

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


def _snapshot_path(name: str, suffix: str = "") -> str:
    # A snapshot selector is one literal segment, never URL navigation or a query.
    encoded = quote(name, safe="")
    if name in {".", ".."}:
        encoded = "%2E" * len(name)
    return f"/v1/snapshots/{encoded}{suffix}"


class SnapshotsResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def create(self, name: str, *, description: str | None = None) -> Snapshot:
        body = _strip_none({"name": name, "description": description})
        data = self._client._request("POST", "/v1/snapshots", json=body)
        return Snapshot.from_dict(data)

    def list(self, *, limit: int = 20, offset: int = 0) -> list[Snapshot]:
        params = {"limit": limit, "offset": offset}
        data = self._client._request("GET", "/v1/snapshots", params=params)
        # Server may return {"snapshots": [...], "has_more": bool} or a plain list
        # An empty body (204) or a null field would otherwise be iterated as None.
        items: builtins.list[Any]
        if isinstance(data, dict):
            items = data.get("snapshots") or data.get("items") or []
        elif isinstance(data, builtins.list):
            items = data
        else:
            items = []
        return [Snapshot.from_dict(s) for s in items]

    def rollback(self, name: str) -> None:
        self._client._request("POST", _snapshot_path(name, "/rollback"), json={})

    def delete(
        self,
        name: str | None = None,
        *,
        names: builtins.list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> None:
        """Delete snapshot(s).

        Single form:  delete(name="snap1")
        Bulk form:    delete(names=["s1","s2"]) | delete(prefix="pre_") | delete(older_than="…")

        Ambiguous failures are not retried by default: a snapshot can be
        recreated with the same name before the client retries.
        """
        selectors = sum(
            [
                1 if name is not None else 0,
                1 if names is not None else 0,
                1 if prefix is not None else 0,
                1 if older_than is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "delete: must provide name, names, prefix, or older_than"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "delete: name, names, prefix, older_than are mutually exclusive"
            )
        if name is not None:
            # A retry after a lost response could delete a replacement snapshot.
            self._client._request("DELETE", _snapshot_path(name), idempotent=False)
        else:
            body = _strip_none({"names": names, "prefix": prefix, "older_than": older_than})
            self._client._request("POST", "/v1/snapshots/delete", json=body)


class AsyncSnapshotsResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def create(self, name: str, *, description: str | None = None) -> Snapshot:
        body = _strip_none({"name": name, "description": description})
        data = await self._client._arequest("POST", "/v1/snapshots", json=body)
        return Snapshot.from_dict(data)

    async def list(self, *, limit: int = 20, offset: int = 0) -> list[Snapshot]:
        params = {"limit": limit, "offset": offset}
        data = await self._client._arequest("GET", "/v1/snapshots", params=params)
        # An empty body (204) or a null field would otherwise be iterated as None.
        items: builtins.list[Any]
        if isinstance(data, dict):
            items = data.get("snapshots") or data.get("items") or []
        elif isinstance(data, builtins.list):
            items = data
        else:
            items = []
        return [Snapshot.from_dict(s) for s in items]

    async def rollback(self, name: str) -> None:
        await self._client._arequest("POST", _snapshot_path(name, "/rollback"), json={})

    async def delete(
        self,
        name: str | None = None,
        *,
        names: builtins.list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> None:
        """Delete snapshots; ambiguous failures are not retried by default."""
        selectors = sum(
            [
                1 if name is not None else 0,
                1 if names is not None else 0,
                1 if prefix is not None else 0,
                1 if older_than is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "delete: must provide name, names, prefix, or older_than"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "delete: name, names, prefix, older_than are mutually exclusive"
            )
        if name is not None:
            # A retry after a lost response could delete a replacement snapshot.
            await self._client._arequest("DELETE", _snapshot_path(name), idempotent=False)
        else:
            body = _strip_none({"names": names, "prefix": prefix, "older_than": older_than})
            await self._client._arequest("POST", "/v1/snapshots/delete", json=body)
