"""Memoria SDK response dataclasses.

All models are plain dataclasses — no Pydantic, no extra dependencies.
Each model exposes a ``from_dict`` classmethod that silently ignores unknown fields
so the SDK remains forward-compatible with future API additions.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from .exceptions import MemoriaResponseError


def _parse_dt(value: str | None) -> datetime | None:
    """Parse an ISO-8601 / RFC-3339 string to a timezone-aware datetime, or return None."""
    if not value or not isinstance(value, str):
        return None
    try:
        # Python 3.10 accepts only 3 or 6 fractional digits, while the API's
        # RFC 3339 timestamps can contain nanoseconds. Match datetime's newer
        # behavior: retain microseconds and truncate any finer precision.
        normalized = re.sub(
            r"^([0-9]{4}-[0-9]{2}-[0-9]{2}[T ][0-9]{2}:[0-9]{2}:[0-9]{2}\.)([0-9]+)",
            lambda match: match[1] + match[2][:6].ljust(6, "0"),
            value,
        )
        dt = datetime.fromisoformat(normalized.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt
    except ValueError:
        return None


def _require_object(d: Any, name: str) -> dict[str, Any]:
    if not isinstance(d, dict):
        raise MemoriaResponseError(
            f"Invalid {name} response: expected a JSON object, got {type(d).__name__}"
        )
    return d


def _require_int(value: Any, field_name: str, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        raise MemoriaResponseError(
            f"Invalid {name} response: '{field_name}' must be an integer"
        )
    try:
        return int(value)
    except (TypeError, ValueError, OverflowError) as exc:
        raise MemoriaResponseError(
            f"Invalid {name} response: '{field_name}' must be an integer"
        ) from exc


def _require_float(value: Any, field_name: str, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        raise MemoriaResponseError(
            f"Invalid {name} response: '{field_name}' must be a number"
        )
    try:
        return float(value)
    except (TypeError, ValueError) as exc:
        raise MemoriaResponseError(
            f"Invalid {name} response: '{field_name}' must be a number"
        ) from exc


def _optional_list(d: dict[str, Any], field_name: str, name: str) -> list[Any]:
    if field_name not in d:
        return []
    value = d[field_name]
    if not isinstance(value, list):
        raise MemoriaResponseError(
            f"Invalid {name} response: '{field_name}' must be a JSON array"
        )
    return list(value)


@dataclass
class Memory:
    """A single memory record returned by store / retrieve / search / list."""

    memory_id: str
    content: str
    memory_type: str          # semantic|profile|procedural|working|tool_result|episodic
    trust_tier: str           # T1|T2|T3|T4
    initial_confidence: float
    is_active: bool
    user_id: str
    author_id: str | None = None          # group mode only; None in personal mode
    session_id: str | None = None
    observed_at: datetime | None = None
    created_at: datetime | None = None
    retrieval_score: float | None = None  # populated by retrieve/search, None from list
    # New response fields are appended so existing positional construction keeps
    # the public dataclass field order released by earlier SDK versions.
    subject_id: str | None = None
    extra_metadata: dict[str, Any] | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Memory:
        d = _require_object(d, "memory")
        memory_id = d.get("memory_id")
        content = d.get("content")
        if not isinstance(memory_id, str) or not isinstance(content, str):
            raise MemoriaResponseError(
                "Invalid memory response: missing string 'memory_id' or 'content'"
            )
        raw_confidence = d.get("initial_confidence", 0.65)
        confidence = (
            0.65
            if raw_confidence is None
            else _require_float(raw_confidence, "initial_confidence", "memory")
        )
        memory_type = d.get("memory_type", "semantic")
        trust_tier = d.get("trust_tier", "T3")
        user_id = d.get("user_id", "")
        if not isinstance(memory_type, str):
            raise MemoriaResponseError("Invalid memory response: 'memory_type' must be a string")
        if not isinstance(trust_tier, str):
            raise MemoriaResponseError("Invalid memory response: 'trust_tier' must be a string")
        if not isinstance(user_id, str):
            raise MemoriaResponseError("Invalid memory response: 'user_id' must be a string")
        return cls(
            memory_id=memory_id,
            content=content,
            memory_type=memory_type,
            trust_tier=trust_tier,
            initial_confidence=confidence,
            is_active=bool(d.get("is_active", True)),
            user_id=user_id,
            author_id=d.get("author_id"),
            subject_id=d.get("subject_id"),
            session_id=d.get("session_id"),
            observed_at=_parse_dt(d.get("observed_at")),
            created_at=_parse_dt(d.get("created_at")),
            retrieval_score=d.get("retrieval_score"),
            extra_metadata=d.get("extra_metadata"),
        )


@dataclass
class MemoryPage:
    """Paginated list of memories."""

    items: list[Memory]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MemoryPage:
        # Missing or non-array items must not look like the end of pagination.
        if not isinstance(d, dict) or not isinstance(d.get("items"), list):
            raise MemoriaResponseError(
                "Invalid memory page: expected an object with an items array"
            )
        items = [Memory.from_dict(m) for m in d["items"]]
        return cls(items=items, next_cursor=d.get("next_cursor"))


@dataclass
class RetrieveResult:
    """Result of retrieve / search, optionally including explain metadata."""

    items: list[Memory]
    explain: dict[str, Any] | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> RetrieveResult:
        # REST returns either a plain array or {"results": [...], "explain": {...}}
        if isinstance(d, list):
            return cls(items=[Memory.from_dict(m) for m in d])
        if not isinstance(d, dict):
            raise MemoriaResponseError(
                "Invalid retrieve response: expected a JSON array or object, "
                f"got {type(d).__name__}"
            )
        results = d.get("results", d.get("items", []))
        if not isinstance(results, list):
            raise MemoriaResponseError(
                "Invalid retrieve response: expected 'results' or 'items' to be a JSON array"
            )
        explain = d.get("explain")
        if explain is not None and not isinstance(explain, dict):
            raise MemoriaResponseError(
                "Invalid retrieve response: expected 'explain' to be a JSON object or null"
            )
        return cls(
            items=[Memory.from_dict(m) for m in results],
            explain=explain,
        )


@dataclass
class PurgeResult:
    """Result of a bulk purge operation."""

    purged: int
    snapshot_name: str | None = None
    warning: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PurgeResult:
        d = _require_object(d, "purge")
        return cls(
            purged=_require_int(d.get("purged", 0), "purged", "purge"),
            snapshot_name=d.get("snapshot_name"),
            warning=d.get("warning"),
        )


@dataclass
class Snapshot:
    """A named snapshot of memory state."""

    name: str
    created_at: datetime | None = None
    description: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Snapshot:
        d = _require_object(d, "snapshot")
        name = d.get("name")
        if not isinstance(name, str) or not name:
            raise MemoriaResponseError(
                "Invalid snapshot response: missing non-empty string 'name'"
            )
        return cls(
            name=name,
            created_at=_parse_dt(d.get("created_at")),
            description=d.get("description"),
        )


@dataclass
class Branch:
    """A named memory branch."""

    name: str
    active: bool = False
    created_at: datetime | None = None  # main branch may lack this field

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Branch:
        d = _require_object(d, "branch")
        name = d.get("name")
        if not isinstance(name, str) or not name:
            raise MemoriaResponseError(
                "Invalid branch response: missing non-empty string 'name'"
            )
        return cls(
            name=name,
            active=bool(d.get("active", False)),
            created_at=_parse_dt(d.get("created_at")),
        )


@dataclass
class ApplyResult:
    """Result of branches.apply — which entries were applied vs skipped."""

    applied_adds: list[str] = field(default_factory=list)
    applied_updates: list[str] = field(default_factory=list)
    applied_removes: list[str] = field(default_factory=list)
    applied_conflicts: list[str] = field(default_factory=list)
    skipped_adds: list[str] = field(default_factory=list)
    skipped_updates: list[str] = field(default_factory=list)
    skipped_removes: list[str] = field(default_factory=list)
    skipped_conflicts: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ApplyResult:
        d = _require_object(d, "apply")
        return cls(
            applied_adds=_optional_list(d, "applied_adds", "apply"),
            applied_updates=_optional_list(d, "applied_updates", "apply"),
            applied_removes=_optional_list(d, "applied_removes", "apply"),
            applied_conflicts=_optional_list(d, "applied_conflicts", "apply"),
            skipped_adds=_optional_list(d, "skipped_adds", "apply"),
            skipped_updates=_optional_list(d, "skipped_updates", "apply"),
            skipped_removes=_optional_list(d, "skipped_removes", "apply"),
            skipped_conflicts=_optional_list(d, "skipped_conflicts", "apply"),
        )


@dataclass
class Profile:
    """User memory profile summary."""

    user_id: str
    profile: str          # concatenated text of all profile-type memories
    stats: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Profile:
        d = _require_object(d, "profile")
        user_id = d.get("user_id", "")
        profile = d.get("profile", "")
        stats = d.get("stats", {})
        if not isinstance(user_id, str):
            raise MemoriaResponseError("Invalid profile response: 'user_id' must be a string")
        if not isinstance(profile, str):
            raise MemoriaResponseError("Invalid profile response: 'profile' must be a string")
        if not isinstance(stats, dict):
            raise MemoriaResponseError("Invalid profile response: 'stats' must be a JSON object")
        return cls(
            user_id=user_id,
            profile=profile,
            stats=stats,
        )


@dataclass
class GovernanceResult:
    """Unified result for governance.run / consolidate / reflect.

    Check ``.skipped`` first; if True the operation was on cooldown.
    When ``skipped=False`` the relevant fields below are populated
    depending on which method was called.
    """

    skipped: bool = False
    cooldown_remaining_s: int | None = None

    # governance.run
    quarantined: int = 0
    cleaned_stale: int = 0
    orphan_graph_cleaned: int = 0

    # governance.consolidate
    status: str | None = None
    conflicts_detected: int = 0
    orphaned_scenes: int = 0
    promoted: int = 0
    demoted: int = 0
    warnings: list[str] = field(default_factory=list)
    decision_count: int = 0

    # governance.reflect (with LLM)
    scenes_created: int = 0
    candidates_found: int = 0
    # governance.reflect mode="candidates" or no LLM
    candidates: list[Any] = field(default_factory=list)

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> GovernanceResult:
        d = _require_object(d, "governance")
        raw_cooldown = d.get("cooldown_remaining_s")
        cooldown_remaining_s = (
            None
            if raw_cooldown is None
            else _require_int(raw_cooldown, "cooldown_remaining_s", "governance")
        )
        return cls(
            skipped=bool(d.get("skipped", False)),
            cooldown_remaining_s=cooldown_remaining_s,
            quarantined=_require_int(d.get("quarantined", 0), "quarantined", "governance"),
            cleaned_stale=_require_int(d.get("cleaned_stale", 0), "cleaned_stale", "governance"),
            orphan_graph_cleaned=_require_int(
                d.get("orphan_graph_cleaned", 0), "orphan_graph_cleaned", "governance"
            ),
            status=d.get("status"),
            conflicts_detected=_require_int(
                d.get("conflicts_detected", 0), "conflicts_detected", "governance"
            ),
            orphaned_scenes=_require_int(
                d.get("orphaned_scenes", 0), "orphaned_scenes", "governance"
            ),
            promoted=_require_int(d.get("promoted", 0), "promoted", "governance"),
            demoted=_require_int(d.get("demoted", 0), "demoted", "governance"),
            warnings=_optional_list(d, "warnings", "governance"),
            decision_count=_require_int(
                d.get("decision_count", 0), "decision_count", "governance"
            ),
            scenes_created=_require_int(
                d.get("scenes_created", 0), "scenes_created", "governance"
            ),
            candidates_found=_require_int(
                d.get("candidates_found", 0), "candidates_found", "governance"
            ),
            candidates=_optional_list(d, "candidates", "governance"),
        )


@dataclass
class ObserveResult:
    """Result of an observe() call."""

    memories: list[dict[str, Any]] = field(default_factory=list)
    warning: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ObserveResult:
        d = _require_object(d, "observe")
        memories = _optional_list(d, "memories", "observe")
        if any(not isinstance(item, dict) for item in memories):
            raise MemoriaResponseError(
                "Invalid observe response: 'memories' entries must be JSON objects"
            )
        return cls(
            memories=memories,
            warning=d.get("warning"),
        )
