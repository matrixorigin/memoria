"""Memoria SDK exception hierarchy."""

from __future__ import annotations


class MemoriaError(Exception):
    """Base class for all Memoria SDK exceptions."""


class MemoriaConnectionError(MemoriaError):
    """Network unreachable, connection refused, or timeout."""


class MemoriaResponseError(MemoriaError):
    """A successful HTTP response does not match the expected response shape."""


class MemoriaAPIError(MemoriaError):
    """HTTP 4xx / 5xx response from the Memoria API."""

    def __init__(self, status_code: int, detail: str) -> None:
        self.status_code = status_code
        self.detail = detail
        super().__init__(f"HTTP {status_code}: {detail}")


class MemoriaAuthError(MemoriaAPIError):
    """HTTP 401 — invalid / expired API key, or rate-limit exceeded (same status code)."""


class MemoriaForbiddenError(MemoriaAPIError):
    """HTTP 403 — operation not allowed (e.g. write to main in multi-member group mode)."""


class MemoriaNotFoundError(MemoriaAPIError):
    """HTTP 404 — requested resource does not exist."""


class MemoriaUnprocessableError(MemoriaAPIError):
    """HTTP 422 — server-side validation failed.

    Common causes:
    - content is empty or exceeds 32 KiB
    - invalid memory_type or trust_tier value
    - store_batch exceeds 100 items
    - session_scope set without session_id
    - memory_types set in purge without session_id
    """


class MemoriaServerError(MemoriaAPIError):
    """HTTP 5xx — unexpected server error."""


class MemoriaValidationError(MemoriaError):
    """Client-side validation failed before the request is sent."""
