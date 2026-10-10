"""MemoriaClient (sync) and AsyncMemoriaClient (async).

Both clients expose identical resource attributes:
  .memories   — store / retrieve / search / list / correct / delete / purge / feedback
  .snapshots  — create / list / rollback / delete
  .branches   — create / list / checkout / diff / diff_items / merge / delete / apply / pick
  .profile    — me()
  .governance — run / consolidate / reflect

Top-level helpers:
  .ping()     — returns True or raises MemoriaConnectionError
  .observe()  — session observation (automatic memory extraction)
  .close() / .aclose()  — explicit connection pool teardown
  Context-manager support: `with` / `async with`
"""

from __future__ import annotations

from typing import Any

import httpx

from ._http import _DEFAULT_MAX_RETRIES, _DEFAULT_TIMEOUT, _build_headers, _HttpTransport
from .exceptions import MemoriaAPIError, MemoriaConnectionError
from .models import ObserveResult
from .resources.branches import AsyncBranchesResource, BranchesResource
from .resources.governance import AsyncGovernanceResource, GovernanceResource
from .resources.memories import AsyncMemoriesResource, MemoriesResource
from .resources.profile import AsyncProfileResource, ProfileResource
from .resources.snapshots import AsyncSnapshotsResource, SnapshotsResource


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


# ---------------------------------------------------------------------------
# Sync client
# ---------------------------------------------------------------------------


class MemoriaClient(_HttpTransport):
    """Synchronous Memoria REST API client.

    Usage::

        with MemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
            mem = client.memories.store(content="Hello, Memoria!")
            result = client.memories.retrieve(query="Hello")
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        timeout: float = _DEFAULT_TIMEOUT,
        max_retries: int = _DEFAULT_MAX_RETRIES,
        retry_unsafe_writes: bool = False,
    ) -> None:
        super().__init__(
            base_url,
            api_key,
            timeout=timeout,
            max_retries=max_retries,
            retry_unsafe_writes=retry_unsafe_writes,
        )
        self._http = httpx.Client(
            headers=_build_headers(api_key),
            timeout=httpx.Timeout(timeout),
        )
        self.memories = MemoriesResource(self)
        self.snapshots = SnapshotsResource(self)
        self.branches = BranchesResource(self)
        self.profile = ProfileResource(self)
        self.governance = GovernanceResource(self)

    # ------------------------------------------------------------------
    # Context-manager support
    # ------------------------------------------------------------------

    def __enter__(self) -> MemoriaClient:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        """Close the underlying httpx connection pool."""
        self._http.close()

    # ------------------------------------------------------------------
    # Top-level helpers
    # ------------------------------------------------------------------

    def ping(self) -> bool:
        """Return True if the Memoria API is reachable.

        Raises:
            MemoriaConnectionError: network unreachable or timeout.
            MemoriaAPIError: server responded with an HTTP error (e.g. 401, 404).
        """
        try:
            self._request("GET", "/health")
            return True
        except (MemoriaConnectionError, MemoriaAPIError):
            raise
        except Exception as exc:
            raise MemoriaConnectionError(str(exc)) from exc

    def observe(
        self,
        messages: list[dict[str, str]],
        *,
        session_id: str | None = None,
    ) -> ObserveResult:
        """Submit a conversation turn for automatic memory extraction."""
        body = _strip_none({"messages": messages, "session_id": session_id})
        data = self._request("POST", "/v1/observe", json=body)
        return ObserveResult.from_dict(data or {})


# ---------------------------------------------------------------------------
# Async client
# ---------------------------------------------------------------------------


class AsyncMemoriaClient(_HttpTransport):
    """Asynchronous Memoria REST API client.

    Usage::

        async with AsyncMemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
            mem = await client.memories.store(content="Hello, Memoria!")
            result = await client.memories.retrieve(query="Hello")
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        timeout: float = _DEFAULT_TIMEOUT,
        max_retries: int = _DEFAULT_MAX_RETRIES,
        retry_unsafe_writes: bool = False,
    ) -> None:
        super().__init__(
            base_url,
            api_key,
            timeout=timeout,
            max_retries=max_retries,
            retry_unsafe_writes=retry_unsafe_writes,
        )
        self._ahttp = httpx.AsyncClient(
            headers=_build_headers(api_key),
            timeout=httpx.Timeout(timeout),
        )
        self.memories = AsyncMemoriesResource(self)
        self.snapshots = AsyncSnapshotsResource(self)
        self.branches = AsyncBranchesResource(self)
        self.profile = AsyncProfileResource(self)
        self.governance = AsyncGovernanceResource(self)

    # ------------------------------------------------------------------
    # Context-manager support
    # ------------------------------------------------------------------

    async def __aenter__(self) -> AsyncMemoriaClient:
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        """Close the underlying httpx async connection pool."""
        await self._ahttp.aclose()

    # ------------------------------------------------------------------
    # Top-level helpers
    # ------------------------------------------------------------------

    async def ping(self) -> bool:
        """Return True if the Memoria API is reachable.

        Raises:
            MemoriaConnectionError: network unreachable or timeout.
            MemoriaAPIError: server responded with an HTTP error (e.g. 401, 404).
        """
        try:
            await self._arequest("GET", "/health")
            return True
        except (MemoriaConnectionError, MemoriaAPIError):
            raise
        except Exception as exc:
            raise MemoriaConnectionError(str(exc)) from exc

    async def observe(
        self,
        messages: list[dict[str, str]],
        *,
        session_id: str | None = None,
    ) -> ObserveResult:
        """Submit a conversation turn for automatic memory extraction."""
        body = _strip_none({"messages": messages, "session_id": session_id})
        data = await self._arequest("POST", "/v1/observe", json=body)
        return ObserveResult.from_dict(data or {})
