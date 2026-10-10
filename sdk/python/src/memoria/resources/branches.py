"""branches resource — sync and async."""

from __future__ import annotations

import builtins
from typing import TYPE_CHECKING, Any, cast

from ..models import ApplyResult, Branch

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


class BranchesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def create(self, name: str) -> Branch:
        data = self._client._request("POST", "/v1/branches", json={"name": name})
        # Server may return {"result": "Created branch 'name'"} instead of a Branch object
        if isinstance(data, dict) and "name" not in data:
            return Branch(name=name)
        return Branch.from_dict(data)

    def list(self) -> list[Branch]:
        data = self._client._request("GET", "/v1/branches")
        # Server returns {"branches": [...], "result": "..."}, not a plain list
        if isinstance(data, dict):
            items = data.get("branches", [])
        else:
            items = data
        return [Branch.from_dict(b) for b in items]

    def checkout(self, name: str) -> None:
        self._client._request("POST", f"/v1/branches/{name}/checkout", json={})

    def diff(self, name: str) -> dict[str, Any]:
        """Return high-level diff statistics (no pagination)."""
        return cast(dict[str, Any], self._client._request("GET", f"/v1/branches/{name}/diff"))

    def diff_items(
        self,
        name: str,
        *,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> dict[str, Any]:
        """Return per-entry diff with optional pagination."""
        params = _strip_none({"limit": limit, "cursor": cursor})
        return cast(
            dict[str, Any],
            self._client._request("GET", f"/v1/branches/{name}/diff-items", params=params),
        )

    def merge(self, name: str, *, strategy: str = "accept") -> None:
        self._client._request("POST", f"/v1/branches/{name}/merge", json={"strategy": strategy})

    def delete(self, name: str) -> None:
        self._client._request("DELETE", f"/v1/branches/{name}")

    def apply(
        self,
        name: str,
        *,
        adds: builtins.list[str] | None = None,
        removes: builtins.list[str] | None = None,
        updates: builtins.list[dict[str, str]] | None = None,
        accept_branch_conflicts: builtins.list[str] | None = None,
    ) -> ApplyResult:
        body = _strip_none(
            {
                "adds": adds,
                "removes": removes,
                "updates": updates,
                "accept_branch_conflicts": accept_branch_conflicts,
            }
        )
        data = self._client._request("POST", f"/v1/branches/{name}/apply", json=body)
        return ApplyResult.from_dict(data)

    def pick(
        self,
        name: str,
        *,
        selector: dict[str, Any],
        strategy: str = "fail",
        target: str = "main",
        dry_run: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        body = _strip_none(
            {
                "selector": selector,
                "strategy": strategy,
                "target": target,
                "dry_run": dry_run,
            }
        )
        return cast(
            dict[str, Any],
            self._client._request("POST", f"/v1/branches/{name}/pick", json=body),
        )


class AsyncBranchesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def create(self, name: str) -> Branch:
        data = await self._client._arequest("POST", "/v1/branches", json={"name": name})
        if isinstance(data, dict) and "name" not in data:
            return Branch(name=name)
        return Branch.from_dict(data)

    async def list(self) -> list[Branch]:
        data = await self._client._arequest("GET", "/v1/branches")
        # Server returns {"branches": [...], "result": "..."}, not a plain list
        if isinstance(data, dict):
            items = data.get("branches", [])
        else:
            items = data
        return [Branch.from_dict(b) for b in items]

    async def checkout(self, name: str) -> None:
        await self._client._arequest("POST", f"/v1/branches/{name}/checkout", json={})

    async def diff(self, name: str) -> dict[str, Any]:
        return cast(
            dict[str, Any], await self._client._arequest("GET", f"/v1/branches/{name}/diff")
        )

    async def diff_items(
        self,
        name: str,
        *,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> dict[str, Any]:
        params = _strip_none({"limit": limit, "cursor": cursor})
        return cast(
            dict[str, Any],
            await self._client._arequest("GET", f"/v1/branches/{name}/diff-items", params=params),
        )

    async def merge(self, name: str, *, strategy: str = "accept") -> None:
        await self._client._arequest(
            "POST", f"/v1/branches/{name}/merge", json={"strategy": strategy}
        )

    async def delete(self, name: str) -> None:
        await self._client._arequest("DELETE", f"/v1/branches/{name}")

    async def apply(
        self,
        name: str,
        *,
        adds: builtins.list[str] | None = None,
        removes: builtins.list[str] | None = None,
        updates: builtins.list[dict[str, str]] | None = None,
        accept_branch_conflicts: builtins.list[str] | None = None,
    ) -> ApplyResult:
        body = _strip_none(
            {
                "adds": adds,
                "removes": removes,
                "updates": updates,
                "accept_branch_conflicts": accept_branch_conflicts,
            }
        )
        data = await self._client._arequest("POST", f"/v1/branches/{name}/apply", json=body)
        return ApplyResult.from_dict(data)

    async def pick(
        self,
        name: str,
        *,
        selector: dict[str, Any],
        strategy: str = "fail",
        target: str = "main",
        dry_run: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        body = _strip_none(
            {
                "selector": selector,
                "strategy": strategy,
                "target": target,
                "dry_run": dry_run,
            }
        )
        return cast(
            dict[str, Any],
            await self._client._arequest("POST", f"/v1/branches/{name}/pick", json=body),
        )
