"""Shared HTTP transport base for MemoriaClient and AsyncMemoriaClient.

Architecture:
  _HttpTransport   — URL building, header injection, error mapping, retry logic
      MemoriaClient       — wraps httpx.Client        (sync)
      AsyncMemoriaClient  — wraps httpx.AsyncClient   (async)

Only ``_request`` / ``_arequest`` differ between the two subclasses.
All business logic (URL, params, body, response parsing) lives in Resource classes
that call ``self._client._request(...)`` or ``await self._client._arequest(...)``.
"""

from __future__ import annotations

import time
from typing import Any

import httpx

from .exceptions import (
    MemoriaAPIError,
    MemoriaAuthError,
    MemoriaConnectionError,
    MemoriaForbiddenError,
    MemoriaNotFoundError,
    MemoriaServerError,
    MemoriaUnprocessableError,
)

try:
    from importlib.metadata import version as _pkg_version

    _VERSION: str = _pkg_version("memoria-client")
except Exception:
    _VERSION = "dev"

_DEFAULT_TIMEOUT = 30.0
_DEFAULT_MAX_RETRIES = 3

# For idempotent methods, repeating the request cannot create anything, so every
# 5xx that suggests a transient fault is retried.
#
# For non-idempotent methods there is no status code that proves the server did
# *not* process the request: a gateway can return 504 while the upstream keeps
# going and commits, and 502/503 can follow a commit if the upstream dies right
# after writing. Without an idempotency key there is no way to retry such a
# write safely, so by default we do not — the caller sees the error and decides.
# Callers who would rather risk a duplicate than surface a transient failure can
# opt in with ``retry_unsafe_writes=True``.
_RETRY_STATUS_SAFE = {500, 502, 503, 504}       # idempotent methods: GET/HEAD/PUT/DELETE
_RETRY_STATUS_UNSAFE = {502, 503, 504}           # non-idempotent methods, opt-in only

# HTTP methods where repeating the request is guaranteed not to cause side-effects.
_IDEMPOTENT_METHODS = {"GET", "HEAD", "PUT", "DELETE", "OPTIONS", "TRACE"}


def _is_idempotent(method: str, idempotent: bool | None) -> bool:
    """Whether repeating this request is safe.

    The HTTP verb is only a default: an endpoint can be non-idempotent despite
    using an idempotent-looking method, so callers may override. PUT
    /v1/memories/{id}/correct is the case in point — the server mints a new
    record and supersedes the old one, so a replay either 404s on the
    already-superseded memory or creates a second replacement.
    """
    if idempotent is not None:
        return idempotent
    return method.upper() in _IDEMPOTENT_METHODS


def _should_retry(
    method: str,
    status_code: int,
    retry_unsafe_writes: bool = False,
    idempotent: bool | None = None,
) -> bool:
    if _is_idempotent(method, idempotent):
        return status_code in _RETRY_STATUS_SAFE
    return retry_unsafe_writes and status_code in _RETRY_STATUS_UNSAFE


def _should_retry_network_error(
    method: str,
    exc: Exception,
    retry_unsafe_writes: bool = False,
    idempotent: bool | None = None,
) -> bool:
    """Failures before sending the request are safe to retry for any method.

    Connection establishment and pool acquisition failures cannot have sent the
    request. Other retryable failures require idempotency or explicit unsafe
    write opt-in: a read timeout, dropped connection or disconnect-before-response
    may mean the server processed the request and only the response was lost.
    """
    if isinstance(exc, (httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout)):
        return True
    retryable = isinstance(
        exc, (httpx.TimeoutException, httpx.NetworkError, httpx.ProtocolError, httpx.DecodingError)
    )
    if _is_idempotent(method, idempotent):
        return retryable
    return retry_unsafe_writes and retryable


def _build_headers(api_key: str) -> dict[str, str]:
    return {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
        "User-Agent": f"memoria-python/{_VERSION}",
    }


_HTTP_STATUS_TEXTS: dict[int, str] = {
    400: "Bad Request",
    401: "Unauthorized",
    403: "Forbidden",
    404: "Not Found",
    409: "Conflict",
    422: "Unprocessable Entity",
    429: "Too Many Requests",
    500: "Internal Server Error",
    502: "Bad Gateway",
    503: "Service Unavailable",
    504: "Gateway Timeout",
}


def _map_error(resp: httpx.Response) -> MemoriaAPIError:
    """Convert an error HTTP response to the appropriate exception subclass."""
    try:
        detail = resp.json().get("detail") or resp.text
    except Exception:
        detail = resp.text
    if not detail:
        detail = _HTTP_STATUS_TEXTS.get(resp.status_code, f"HTTP {resp.status_code}")
    sc = resp.status_code
    if sc == 401:
        return MemoriaAuthError(sc, detail)
    if sc == 403:
        return MemoriaForbiddenError(sc, detail)
    if sc == 404:
        return MemoriaNotFoundError(sc, detail)
    if sc == 422:
        return MemoriaUnprocessableError(sc, detail)
    if sc >= 500:
        return MemoriaServerError(sc, detail)
    return MemoriaAPIError(sc, detail)



def _backoff(attempt: int) -> float:
    """Exponential backoff: 0.5s, 1s, 2s, …"""
    return 0.5 * float(2**attempt)


class _HttpTransport:
    """Shared state and helpers. Subclasses provide the actual HTTP call."""

    def __init__(
        self,
        base_url: str,
        api_key: str,
        timeout: float = _DEFAULT_TIMEOUT,
        max_retries: int = _DEFAULT_MAX_RETRIES,
        retry_unsafe_writes: bool = False,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._timeout = timeout
        self._max_retries = max_retries
        self._retry_unsafe_writes = retry_unsafe_writes
        self._headers = _build_headers(api_key)

    def _url(self, path: str) -> str:
        return self._base_url + path

    # ------------------------------------------------------------------
    # Sync transport
    # ------------------------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        json: Any = None,
        idempotent: bool | None = None,
    ) -> Any:
        """Execute a synchronous HTTP request with retry logic.

        Retry policy:
          - idempotent methods (GET/HEAD/PUT/DELETE): 500/502/503/504 and any
            transport error are retried
          - non-idempotent methods (POST/PATCH): HTTP status and ambiguous
            transport failures are not retried without ``retry_unsafe_writes=True``
          - ConnectError/ConnectTimeout/PoolTimeout: retried within the configured
            budget even for writes; the request has not been sent
        """
        url = self._url(path)
        last_exc: Exception | None = None
        for attempt in range(self._max_retries + 1):
            try:
                resp = self._http.request(  # type: ignore[attr-defined]
                    method,
                    url,
                    params={k: v for k, v in (params or {}).items() if v is not None},
                    json=json,
                    timeout=self._timeout,
                )
            # DecodingError is outside TransportError: corrupt compressed response
            # bytes fail while HTTPX reads the body. The request may have committed,
            # so preserve the same idempotency/unsafe-write retry boundary.
            except (httpx.TransportError, httpx.DecodingError) as exc:
                last_exc = exc
                if attempt < self._max_retries and _should_retry_network_error(
                    method, exc, self._retry_unsafe_writes, idempotent
                ):
                    time.sleep(_backoff(attempt))
                    continue
                raise MemoriaConnectionError(str(exc)) from exc

            if resp.is_success:
                if resp.status_code == 204 or not resp.content:
                    return None
                content_type = resp.headers.get("content-type", "")
                if "json" in content_type:
                    return resp.json()
                try:
                    return resp.json()
                except Exception:
                    return resp.text

            if (
                _should_retry(method, resp.status_code, self._retry_unsafe_writes, idempotent)
                and attempt < self._max_retries
            ):
                time.sleep(_backoff(attempt))
                continue

            raise _map_error(resp)

        # should not reach here, but satisfy type checker
        if last_exc:
            raise MemoriaConnectionError(str(last_exc)) from last_exc
        raise MemoriaConnectionError("request failed after retries")

    # ------------------------------------------------------------------
    # Async transport
    # ------------------------------------------------------------------

    async def _arequest(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        json: Any = None,
        idempotent: bool | None = None,
    ) -> Any:
        """Execute an asynchronous HTTP request with retry logic.

        Same retry policy as _request — see its docstring for details.
        """
        import asyncio

        url = self._url(path)
        last_exc: Exception | None = None
        for attempt in range(self._max_retries + 1):
            try:
                resp = await self._ahttp.request(  # type: ignore[attr-defined]
                    method,
                    url,
                    params={k: v for k, v in (params or {}).items() if v is not None},
                    json=json,
                    timeout=self._timeout,
                )
            # See the sync path for compressed response decoding and replay safety.
            except (httpx.TransportError, httpx.DecodingError) as exc:
                last_exc = exc
                if attempt < self._max_retries and _should_retry_network_error(
                    method, exc, self._retry_unsafe_writes, idempotent
                ):
                    await asyncio.sleep(_backoff(attempt))
                    continue
                raise MemoriaConnectionError(str(exc)) from exc

            if resp.is_success:
                if resp.status_code == 204 or not resp.content:
                    return None
                content_type = resp.headers.get("content-type", "")
                if "json" in content_type:
                    return resp.json()
                try:
                    return resp.json()
                except Exception:
                    return resp.text

            if (
                _should_retry(method, resp.status_code, self._retry_unsafe_writes, idempotent)
                and attempt < self._max_retries
            ):
                await asyncio.sleep(_backoff(attempt))
                continue

            raise _map_error(resp)

        if last_exc:
            raise MemoriaConnectionError(str(last_exc)) from last_exc
        raise MemoriaConnectionError("request failed after retries")
