"""branches resource — sync and async."""

from __future__ import annotations

import builtins
from typing import TYPE_CHECKING, Any
from urllib.parse import quote

from ..exceptions import MemoriaConnectionError, MemoriaResponseError
from ..models import ApplyResult, Branch

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


def _branch_path(name: str, suffix: str = "") -> str:
    # Display names are data, not URL syntax. Preserve literal percent signs
    # and dot-only names that httpx would otherwise normalize as navigation.
    encoded = quote(name, safe="")
    if name in {".", ".."}:
        encoded = "%2E" * len(name)
    return f"/v1/branches/{encoded}{suffix}"


def _branch_creation_response(data: Any, name: str) -> Branch:
    """Reject malformed success responses rather than reporting a phantom branch."""
    if name and isinstance(data, dict) and "error" not in data:
        if "name" in data:
            if data["name"] == name and "result" not in data:
                return Branch.from_dict(data)
        elif data.get("result") == f"Created branch '{name}'":
            return Branch(name=name)
    raise MemoriaConnectionError("Invalid branch creation response")


def _parse_branch_list(data: Any) -> builtins.list[Branch]:
    # Server returns {"branches": [...], "result": "..."}, not a plain list
    if isinstance(data, dict):
        items = data.get("branches", [])
    else:
        items = data
    if items is None:
        return []
    if not isinstance(items, builtins.list):
        raise MemoriaResponseError(
            f"Invalid branch list response: expected a JSON array, got {type(items).__name__}"
        )
    return [Branch.from_dict(b) for b in items]


def _require_dict_response(data: Any, operation: str) -> dict[str, Any]:
    if not isinstance(data, dict):
        raise MemoriaResponseError(
            f"Invalid branch {operation} response: expected a JSON object, "
            f"got {type(data).__name__}"
        )
    return data


class BranchesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def create(self, name: str) -> Branch:
        data = self._client._request("POST", "/v1/branches", json={"name": name})
        return _branch_creation_response(data, name)

    def list(self) -> list[Branch]:
        data = self._client._request("GET", "/v1/branches")
        return _parse_branch_list(data)

    def checkout(self, name: str) -> None:
        self._client._request("POST", _branch_path(name, "/checkout"), json={})

    def diff(self, name: str) -> dict[str, Any]:
        """Return high-level diff statistics (no pagination)."""
        data = self._client._request("GET", _branch_path(name, "/diff"))
        return _require_dict_response(data, "diff")

    def diff_items(
        self,
        name: str,
        *,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> dict[str, Any]:
        """Return per-entry diff with optional pagination."""
        params = _strip_none({"limit": limit, "cursor": cursor})
        data = self._client._request("GET", _branch_path(name, "/diff-items"), params=params)
        return _require_dict_response(data, "diff_items")

    def merge(self, name: str, *, strategy: str = "accept") -> None:
        self._client._request("POST", _branch_path(name, "/merge"), json={"strategy": strategy})

    def delete(self, name: str) -> None:
        """Delete a branch; ambiguous failures are not retried by default.

        A later branch may reuse the name, so retrying could delete it instead.
        """
        # A different branch may be created with the same name after deletion.
        self._client._request("DELETE", _branch_path(name), idempotent=False)

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
        data = self._client._request("POST", _branch_path(name, "/apply"), json=body)
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
        data = self._client._request("POST", _branch_path(name, "/pick"), json=body)
        return _require_dict_response(data, "pick")


class AsyncBranchesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def create(self, name: str) -> Branch:
        data = await self._client._arequest("POST", "/v1/branches", json={"name": name})
        return _branch_creation_response(data, name)

    async def list(self) -> list[Branch]:
        data = await self._client._arequest("GET", "/v1/branches")
        return _parse_branch_list(data)

    async def checkout(self, name: str) -> None:
        await self._client._arequest("POST", _branch_path(name, "/checkout"), json={})

    async def diff(self, name: str) -> dict[str, Any]:
        data = await self._client._arequest("GET", _branch_path(name, "/diff"))
        return _require_dict_response(data, "diff")

    async def diff_items(
        self,
        name: str,
        *,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> dict[str, Any]:
        params = _strip_none({"limit": limit, "cursor": cursor})
        data = await self._client._arequest(
            "GET", _branch_path(name, "/diff-items"), params=params
        )
        return _require_dict_response(data, "diff_items")

    async def merge(self, name: str, *, strategy: str = "accept") -> None:
        await self._client._arequest(
            "POST", _branch_path(name, "/merge"), json={"strategy": strategy}
        )

    async def delete(self, name: str) -> None:
        """Delete a branch; ambiguous failures are not retried by default."""
        # A retry after a lost response could delete a replacement branch.
        await self._client._arequest("DELETE", _branch_path(name), idempotent=False)

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
        data = await self._client._arequest("POST", _branch_path(name, "/apply"), json=body)
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
        data = await self._client._arequest("POST", _branch_path(name, "/pick"), json=body)
        return _require_dict_response(data, "pick")
