"""memories resource — sync and async."""

from __future__ import annotations

import json
import math
from typing import TYPE_CHECKING, Any

from ..exceptions import MemoriaValidationError
from ..models import Memory, MemoryPage, PurgeResult, RetrieveResult

if TYPE_CHECKING:
    from .._http import _HttpTransport

# Public REST contract. Keep these values synchronized with the exported
# constants in `memoria-storage/src/store.rs`.
_EXTRA_METADATA_FILTER_MAX_FIELDS = 16
_EXTRA_METADATA_FILTER_MAX_KEY_BYTES = 64
_EXTRA_METADATA_FILTER_MAX_VALUE_BYTES = 1024
_FULLTEXT_QUERY_MAX_BYTES = 4096
_FULLTEXT_SEARCH_DEFAULT_LIMIT = 20
_FULLTEXT_SEARCH_MAX_LIMIT = 100

_MEMORY_TYPE_NAMES = {
    "semantic",
    "working",
    "episodic",
    "profile",
    "tool_result",
    "procedural",
}


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


def _normalize_query_memory_types(
    memory_types: list[str] | None,
) -> list[str] | None:
    if memory_types is None:
        return None
    if not isinstance(memory_types, list):
        raise MemoriaValidationError("query: memory_types must be a list")
    if not memory_types:
        raise MemoriaValidationError(
            "query: memory_types must contain at least one value when provided"
        )
    normalized: list[str] = []
    seen: set[str] = set()
    for value in memory_types:
        if not isinstance(value, str):
            raise MemoriaValidationError("query: memory_types entries must be strings")
        value = value.strip()
        if not value:
            raise MemoriaValidationError("query: memory_types entries must not be empty")
        if value not in _MEMORY_TYPE_NAMES:
            raise MemoriaValidationError(f"query: unknown memory type: {value}")
        if value not in seen:
            seen.add(value)
            normalized.append(value)
    return normalized or None


def _validate_structured_query(
    *,
    extra_metadata_filter: dict[str, Any] | None,
    subject_id: str | None,
    memory_types: list[str] | None,
    session_id: str | None,
    trust_tier: str | None,
    branch: str | None,
    limit: int,
) -> tuple[
    list[str] | None,
    str | None,
    str | None,
    str | None,
    str | None,
]:
    if type(limit) is not int:
        raise MemoriaValidationError("query: limit must be an integer")
    if limit < 1 or limit > 500:
        raise MemoriaValidationError("query: limit must be between 1 and 500")
    if extra_metadata_filter is not None and not isinstance(extra_metadata_filter, dict):
        raise MemoriaValidationError("query: extra_metadata_filter must be a dictionary")
    for name, value in [
        ("subject_id", subject_id),
        ("session_id", session_id),
        ("trust_tier", trust_tier),
        ("branch", branch),
    ]:
        if value is not None and (not isinstance(value, str) or not value.strip()):
            raise MemoriaValidationError(f"query: {name} must be a non-empty string when provided")
    subject_id = subject_id.strip() if subject_id is not None else None
    session_id = session_id.strip() if session_id is not None else None
    trust_tier = trust_tier.strip() if trust_tier is not None else None
    branch = branch.strip() if branch is not None else None
    memory_types = _normalize_query_memory_types(memory_types)
    has_selector = any(
        [
            bool(extra_metadata_filter),
            bool(subject_id and subject_id.strip()),
            bool(memory_types),
            bool(session_id and session_id.strip()),
            bool(trust_tier and trust_tier.strip()),
            bool(branch and branch.strip()),
        ]
    )
    if not has_selector:
        raise MemoriaValidationError("query: at least one filter selector is required")
    if extra_metadata_filter is not None:
        if len(extra_metadata_filter) > 16:
            raise MemoriaValidationError(
                "query: extra_metadata_filter must not contain more than 16 fields"
            )
        for key, value in extra_metadata_filter.items():
            if not isinstance(key, str):
                raise MemoriaValidationError("query: extra_metadata_filter keys must be strings")
            try:
                key_bytes = len(key.encode("utf-8"))
            except UnicodeEncodeError as error:
                raise MemoriaValidationError(
                    "query: extra_metadata_filter keys must be valid UTF-8"
                ) from error
            valid_key = (
                bool(key)
                and key_bytes <= 64
                and (key[0].isascii() and (key[0].isalpha() or key[0] == "_"))
                and all(char.isascii() and (char.isalnum() or char == "_") for char in key[1:])
            )
            if not valid_key:
                raise MemoriaValidationError(
                    "query: extra_metadata_filter keys must start with an ASCII letter or "
                    "underscore, contain only ASCII letters, digits, or underscore, and "
                    "must not exceed 64 bytes"
                )
            if not isinstance(value, (str, int, float, bool)):
                raise MemoriaValidationError(
                    "query: extra_metadata_filter values must be strings, numbers, or booleans"
                )
            if isinstance(value, float) and not math.isfinite(value):
                raise MemoriaValidationError(
                    "query: extra_metadata_filter numeric values must be finite"
                )
            try:
                encoded_value = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode(
                    "utf-8"
                )
            except (ValueError, UnicodeEncodeError) as error:
                raise MemoriaValidationError(
                    "query: extra_metadata_filter values must be valid JSON scalars "
                    "encoded as UTF-8"
                ) from error
            if len(encoded_value) > 1024:
                raise MemoriaValidationError(
                    "query: extra_metadata_filter values must not exceed 1024 bytes"
                )
    return memory_types, subject_id, session_id, trust_tier, branch


def _validate_fulltext_metadata_filter(
    extra_metadata_filter: dict[str, Any] | None,
) -> None:
    if extra_metadata_filter is None:
        return
    if not isinstance(extra_metadata_filter, dict):
        raise MemoriaValidationError("extra_metadata_filter must be a dictionary")
    if len(extra_metadata_filter) > _EXTRA_METADATA_FILTER_MAX_FIELDS:
        raise MemoriaValidationError(
            "extra_metadata_filter must not contain more than "
            f"{_EXTRA_METADATA_FILTER_MAX_FIELDS} fields"
        )
    for key, value in extra_metadata_filter.items():
        if not isinstance(key, str):
            raise MemoriaValidationError("extra_metadata_filter keys must be strings")
        try:
            key_bytes = len(key.encode("utf-8"))
        except UnicodeEncodeError as error:
            raise MemoriaValidationError(
                "extra_metadata_filter keys must be valid UTF-8"
            ) from error
        valid_key = (
            bool(key)
            and key_bytes <= _EXTRA_METADATA_FILTER_MAX_KEY_BYTES
            and (key[0].isascii() and (key[0].isalpha() or key[0] == "_"))
            and all(char.isascii() and (char.isalnum() or char == "_") for char in key[1:])
        )
        if not valid_key:
            raise MemoriaValidationError(
                "extra_metadata_filter keys must start with an ASCII letter or underscore "
                "and contain only ASCII letters, digits, or underscore, and must not "
                f"exceed {_EXTRA_METADATA_FILTER_MAX_KEY_BYTES} bytes"
            )
        if not isinstance(value, (str, int, float, bool)):
            raise MemoriaValidationError(
                "extra_metadata_filter values must be strings, numbers, or booleans"
            )
        if isinstance(value, float) and not math.isfinite(value):
            raise MemoriaValidationError("extra_metadata_filter numeric values must be finite")
        try:
            encoded_value = json.dumps(
                value, ensure_ascii=False, separators=(",", ":")
            ).encode("utf-8")
        except (ValueError, UnicodeEncodeError) as error:
            raise MemoriaValidationError(
                "extra_metadata_filter values must be valid JSON scalars encoded as UTF-8"
            ) from error
        if len(encoded_value) > _EXTRA_METADATA_FILTER_MAX_VALUE_BYTES:
            raise MemoriaValidationError(
                "extra_metadata_filter values must not exceed "
                f"{_EXTRA_METADATA_FILTER_MAX_VALUE_BYTES} bytes"
            )


def _normalize_fulltext_memory_types(
    memory_types: list[str] | None,
) -> list[str] | None:
    if memory_types is None:
        return None
    if not isinstance(memory_types, list):
        raise MemoriaValidationError("fulltext_search: memory_types must be a list")
    normalized: list[str] = []
    seen: set[str] = set()
    for value in memory_types:
        if not isinstance(value, str):
            raise MemoriaValidationError(
                "fulltext_search: memory_types entries must be strings"
            )
        value = value.strip()
        if not value:
            raise MemoriaValidationError(
                "fulltext_search: memory_types entries must not be empty when provided"
            )
        if value not in _MEMORY_TYPE_NAMES:
            raise MemoriaValidationError(f"fulltext_search: unknown memory type: {value}")
        if value not in seen:
            seen.add(value)
            normalized.append(value)
    return normalized or None


def _validate_fulltext_search(
    query: str,
    extra_metadata_filter: dict[str, Any] | None,
    limit: int,
    *,
    subject_id: str | None,
    session_id: str | None,
    trust_tier: str | None,
    branch: str | None,
    memory_types: list[str] | None,
) -> list[str] | None:
    if type(limit) is not int or limit < 1 or limit > _FULLTEXT_SEARCH_MAX_LIMIT:
        raise MemoriaValidationError(
            f"fulltext_search: limit must be between 1 and {_FULLTEXT_SEARCH_MAX_LIMIT}"
        )
    if not isinstance(query, str):
        raise MemoriaValidationError("fulltext_search: query must be a string")
    try:
        query_bytes = len(query.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise MemoriaValidationError(
            "fulltext_search: query must be valid UTF-8"
        ) from error
    if query_bytes > _FULLTEXT_QUERY_MAX_BYTES:
        raise MemoriaValidationError(
            f"fulltext_search: query must not exceed {_FULLTEXT_QUERY_MAX_BYTES} bytes"
        )
    if not query or not any(char.isalnum() or char == "_" for char in query):
        raise MemoriaValidationError(
            "fulltext_search: query must contain at least one Unicode letter, number, or underscore"
        )
    for name, value in [
        ("subject_id", subject_id),
        ("session_id", session_id),
        ("trust_tier", trust_tier),
        ("branch", branch),
    ]:
        if value is not None and (not isinstance(value, str) or not value.strip()):
            raise MemoriaValidationError(
                f"fulltext_search: {name} must be a non-empty string when provided"
            )
    try:
        _validate_fulltext_metadata_filter(extra_metadata_filter)
    except MemoriaValidationError as error:
        raise MemoriaValidationError(f"fulltext_search: {error}") from error
    return _normalize_fulltext_memory_types(memory_types)


class MemoriesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def store(
        self,
        content: str,
        *,
        memory_type: str = "semantic",
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none(
            {
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories", json=body)
        return Memory.from_dict(data)

    def store_batch(
        self,
        items: list[dict[str, Any]],
        *,
        branch: str | None = None,
    ) -> list[Memory]:
        if len(items) > 100:
            raise MemoriaValidationError("store_batch: items must not exceed 100")
        # Server field name is "memories", not "items"
        body: dict[str, Any] = {"memories": items}
        if branch is not None:
            body["branch"] = branch
        data = self._client._request("POST", "/v1/memories/batch", json=body)
        return [Memory.from_dict(m) for m in data]

    def retrieve(
        self,
        query: str,
        *,
        top_k: int = 5,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("retrieve: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/retrieve", json=body)
        return RetrieveResult.from_dict(data)

    def search(
        self,
        query: str,
        *,
        top_k: int = 10,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("search: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/search", json=body)
        return RetrieveResult.from_dict(data)

    def fulltext_search(
        self,
        query: str,
        *,
        extra_metadata_filter: dict[str, Any] | None = None,
        subject_id: str | None = None,
        memory_types: list[str] | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
        limit: int = _FULLTEXT_SEARCH_DEFAULT_LIMIT,
    ) -> RetrieveResult:
        """Run pure full-text search with exact structured pre-filters.

        ``session_id`` matches only that session; unlike retrieve/search session
        scoping, unscoped memories are not included. Metadata equality preserves
        JSON type families: number ``2`` may equal ``2.0`` but not string ``"2"``.
        """
        memory_types = _validate_fulltext_search(
            query,
            extra_metadata_filter,
            limit,
            subject_id=subject_id,
            session_id=session_id,
            trust_tier=trust_tier,
            branch=branch,
            memory_types=memory_types,
        )
        body = _strip_none(
            {
                "query": query,
                "extra_metadata_filter": extra_metadata_filter,
                "subject_id": subject_id,
                "memory_types": memory_types,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
                "limit": limit,
            }
        )
        data = self._client._request("POST", "/v1/memories/fulltext-search", json=body)
        return RetrieveResult.from_dict(data)

    def list(
        self,
        *,
        limit: int = 100,
        cursor: str | None = None,
        memory_type: str | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> MemoryPage:
        if limit > 500:
            raise MemoriaValidationError("list: limit must not exceed 500")
        params = _strip_none(
            {
                "limit": limit,
                "cursor": cursor,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = self._client._request("GET", "/v1/memories", params=params)
        return MemoryPage.from_dict(data)

    def query(
        self,
        *,
        extra_metadata_filter: dict[str, Any] | None = None,
        subject_id: str | None = None,
        memory_types: list[str] | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
        limit: int = 100,
        cursor: str | None = None,
    ) -> MemoryPage:
        """Run an exact structured query without vector or keyword retrieval."""
        memory_types, subject_id, session_id, trust_tier, branch = _validate_structured_query(
            extra_metadata_filter=extra_metadata_filter,
            subject_id=subject_id,
            memory_types=memory_types,
            session_id=session_id,
            trust_tier=trust_tier,
            branch=branch,
            limit=limit,
        )
        body = _strip_none(
            {
                "extra_metadata_filter": extra_metadata_filter,
                "subject_id": subject_id,
                "memory_types": memory_types,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
                "limit": limit,
                "cursor": cursor,
            }
        )
        data = self._client._request("POST", "/v1/memories/query", json=body)
        return MemoryPage.from_dict(data)

    def correct(
        self,
        id: str,
        *,
        new_content: str,
        reason: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none({"new_content": new_content, "reason": reason, "branch": branch})
        # PUT, but not idempotent: the server mints a replacement record and
        # supersedes the original, so a replay 404s or creates a second one.
        data = self._client._request(
            "PUT", f"/v1/memories/{id}/correct", json=body, idempotent=False
        )
        return Memory.from_dict(data)

    def correct_by_query(
        self,
        query: str,
        *,
        new_content: str,
        reason: str | None = None,
        session_id: str | None = None,
        session_scope: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("correct_by_query: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "new_content": new_content,
                "reason": reason,
                "session_id": session_id,
                "session_scope": session_scope,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/correct", json=body)
        return Memory.from_dict(data)

    def delete(
        self,
        id: str,
        *,
        reason: str | None = None,
        branch: str | None = None,
    ) -> None:
        params = _strip_none({"reason": reason, "branch": branch})
        self._client._request("DELETE", f"/v1/memories/{id}", params=params)

    def purge(
        self,
        *,
        memory_ids: list[str] | None = None,
        topic: str | None = None,
        session_id: str | None = None,
        memory_types: list[str] | None = None,
        reason: str | None = None,
        branch: str | None = None,
    ) -> PurgeResult:
        selectors = sum(
            [
                1 if memory_ids is not None else 0,
                1 if topic is not None else 0,
                1 if session_id is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "purge: must specify at least one of memory_ids, topic, or session_id"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "purge: memory_ids, topic, and session_id are mutually exclusive"
            )
        if memory_types is not None and session_id is None:
            raise MemoriaValidationError("purge: memory_types requires session_id")
        body = _strip_none(
            {
                "memory_ids": memory_ids,
                "topic": topic,
                "session_id": session_id,
                "memory_types": memory_types,
                "reason": reason,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/purge", json=body)
        return PurgeResult.from_dict(data)

    def feedback(
        self,
        id: str,
        *,
        signal: str,
        context: str | None = None,
    ) -> None:
        body = _strip_none({"signal": signal, "context": context})
        self._client._request("POST", f"/v1/memories/{id}/feedback", json=body)


class AsyncMemoriesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def store(
        self,
        content: str,
        *,
        memory_type: str = "semantic",
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none(
            {
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories", json=body)
        return Memory.from_dict(data)

    async def store_batch(
        self,
        items: list[dict[str, Any]],
        *,
        branch: str | None = None,
    ) -> list[Memory]:
        if len(items) > 100:
            raise MemoriaValidationError("store_batch: items must not exceed 100")
        # Server field name is "memories", not "items"
        body: dict[str, Any] = {"memories": items}
        if branch is not None:
            body["branch"] = branch
        data = await self._client._arequest("POST", "/v1/memories/batch", json=body)
        return [Memory.from_dict(m) for m in data]

    async def retrieve(
        self,
        query: str,
        *,
        top_k: int = 5,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("retrieve: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/retrieve", json=body)
        return RetrieveResult.from_dict(data)

    async def search(
        self,
        query: str,
        *,
        top_k: int = 10,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("search: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/search", json=body)
        return RetrieveResult.from_dict(data)

    async def fulltext_search(
        self,
        query: str,
        *,
        extra_metadata_filter: dict[str, Any] | None = None,
        subject_id: str | None = None,
        memory_types: list[str] | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
        limit: int = _FULLTEXT_SEARCH_DEFAULT_LIMIT,
    ) -> RetrieveResult:
        """Run pure full-text search with exact structured pre-filters.

        ``session_id`` matches only that session; unlike retrieve/search session
        scoping, unscoped memories are not included. Metadata equality preserves
        JSON type families: number ``2`` may equal ``2.0`` but not string ``"2"``.
        """
        memory_types = _validate_fulltext_search(
            query,
            extra_metadata_filter,
            limit,
            subject_id=subject_id,
            session_id=session_id,
            trust_tier=trust_tier,
            branch=branch,
            memory_types=memory_types,
        )
        body = _strip_none(
            {
                "query": query,
                "extra_metadata_filter": extra_metadata_filter,
                "subject_id": subject_id,
                "memory_types": memory_types,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
                "limit": limit,
            }
        )
        data = await self._client._arequest(
            "POST", "/v1/memories/fulltext-search", json=body
        )
        return RetrieveResult.from_dict(data)

    async def list(
        self,
        *,
        limit: int = 100,
        cursor: str | None = None,
        memory_type: str | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> MemoryPage:
        if limit > 500:
            raise MemoriaValidationError("list: limit must not exceed 500")
        params = _strip_none(
            {
                "limit": limit,
                "cursor": cursor,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = await self._client._arequest("GET", "/v1/memories", params=params)
        return MemoryPage.from_dict(data)

    async def query(
        self,
        *,
        extra_metadata_filter: dict[str, Any] | None = None,
        subject_id: str | None = None,
        memory_types: list[str] | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
        limit: int = 100,
        cursor: str | None = None,
    ) -> MemoryPage:
        """Run an exact structured query without vector or keyword retrieval."""
        memory_types, subject_id, session_id, trust_tier, branch = _validate_structured_query(
            extra_metadata_filter=extra_metadata_filter,
            subject_id=subject_id,
            memory_types=memory_types,
            session_id=session_id,
            trust_tier=trust_tier,
            branch=branch,
            limit=limit,
        )
        body = _strip_none(
            {
                "extra_metadata_filter": extra_metadata_filter,
                "subject_id": subject_id,
                "memory_types": memory_types,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
                "limit": limit,
                "cursor": cursor,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/query", json=body)
        return MemoryPage.from_dict(data)

    async def correct(
        self,
        id: str,
        *,
        new_content: str,
        reason: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none({"new_content": new_content, "reason": reason, "branch": branch})
        # PUT, but not idempotent — see the sync path.
        data = await self._client._arequest(
            "PUT", f"/v1/memories/{id}/correct", json=body, idempotent=False
        )
        return Memory.from_dict(data)

    async def correct_by_query(
        self,
        query: str,
        *,
        new_content: str,
        reason: str | None = None,
        session_id: str | None = None,
        session_scope: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("correct_by_query: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "new_content": new_content,
                "reason": reason,
                "session_id": session_id,
                "session_scope": session_scope,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/correct", json=body)
        return Memory.from_dict(data)

    async def delete(
        self,
        id: str,
        *,
        reason: str | None = None,
        branch: str | None = None,
    ) -> None:
        params = _strip_none({"reason": reason, "branch": branch})
        await self._client._arequest("DELETE", f"/v1/memories/{id}", params=params)

    async def purge(
        self,
        *,
        memory_ids: list[str] | None = None,
        topic: str | None = None,
        session_id: str | None = None,
        memory_types: list[str] | None = None,
        reason: str | None = None,
        branch: str | None = None,
    ) -> PurgeResult:
        selectors = sum(
            [
                1 if memory_ids is not None else 0,
                1 if topic is not None else 0,
                1 if session_id is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "purge: must specify at least one of memory_ids, topic, or session_id"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "purge: memory_ids, topic, and session_id are mutually exclusive"
            )
        if memory_types is not None and session_id is None:
            raise MemoriaValidationError("purge: memory_types requires session_id")
        body = _strip_none(
            {
                "memory_ids": memory_ids,
                "topic": topic,
                "session_id": session_id,
                "memory_types": memory_types,
                "reason": reason,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/purge", json=body)
        return PurgeResult.from_dict(data)

    async def feedback(
        self,
        id: str,
        *,
        signal: str,
        context: str | None = None,
    ) -> None:
        body = _strip_none({"signal": signal, "context": context})
        await self._client._arequest("POST", f"/v1/memories/{id}/feedback", json=body)
