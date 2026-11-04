"""Memoria SDK response dataclasses.

All models are plain dataclasses — no Pydantic, no extra dependencies.
Each model exposes a ``from_dict`` classmethod that silently ignores unknown fields
so the SDK remains forward-compatible with future API additions.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any


def _parse_dt(value: str | None) -> datetime | None:
    """Parse an ISO-8601 / RFC-3339 string to a timezone-aware datetime, or return None."""
    if not value:
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt
    except ValueError:
        return None


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
        return cls(
            memory_id=d["memory_id"],
            content=d["content"],
            memory_type=d.get("memory_type", "semantic"),
            trust_tier=d.get("trust_tier", "T3"),
            initial_confidence=float(d.get("initial_confidence", 0.65)),
            is_active=bool(d.get("is_active", True)),
            user_id=d.get("user_id", ""),
            author_id=d.get("author_id"),
            subject_id=d.get("subject_id"),
            session_id=d.get("session_id"),
            observed_at=_parse_dt(d.get("observed_at")),
            created_at=_parse_dt(d.get("created_at")),
            retrieval_score=d.get("retrieval_score"),
            extra_metadata=d.get("extra_metadata"),
        )


@dataclass
class DisabledMemory:
    """A memory an operator disabled: out of retrieval, restorable."""

    memory: Memory
    disabled_at: datetime | None = None
    disabled_reason: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> DisabledMemory:
        return cls(
            memory=Memory.from_dict(d),
            disabled_at=_parse_dt(d.get("disabled_at")),
            disabled_reason=d.get("disabled_reason"),
        )


@dataclass
class DisabledMemoryPage:
    """A bounded page of disabled records; pass next_cursor to continue."""

    items: list[DisabledMemory]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> DisabledMemoryPage:
        return cls(
            items=[DisabledMemory.from_dict(item) for item in d["items"]],
            next_cursor=d.get("next_cursor"),
        )


@dataclass
class MemoryPage:
    """Paginated list of memories."""

    items: list[Memory]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MemoryPage:
        items = [Memory.from_dict(m) for m in d.get("items", [])]
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
        results = d.get("results", d.get("items", []))
        return cls(
            items=[Memory.from_dict(m) for m in results],
            explain=d.get("explain"),
        )


@dataclass
class PurgeResult:
    """Result of a bulk purge operation."""

    purged: int
    snapshot_name: str | None = None
    warning: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PurgeResult:
        return cls(
            purged=int(d.get("purged", 0)),
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
        return cls(
            name=d["name"],
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
        return cls(
            name=d["name"],
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
        return cls(
            applied_adds=list(d.get("applied_adds", [])),
            applied_updates=list(d.get("applied_updates", [])),
            applied_removes=list(d.get("applied_removes", [])),
            applied_conflicts=list(d.get("applied_conflicts", [])),
            skipped_adds=list(d.get("skipped_adds", [])),
            skipped_updates=list(d.get("skipped_updates", [])),
            skipped_removes=list(d.get("skipped_removes", [])),
            skipped_conflicts=list(d.get("skipped_conflicts", [])),
        )


@dataclass
class Profile:
    """User memory profile summary."""

    user_id: str
    profile: str          # concatenated text of all profile-type memories
    stats: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Profile:
        return cls(
            user_id=d.get("user_id", ""),
            profile=d.get("profile", ""),
            stats=d.get("stats", {}),
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
        return cls(
            skipped=bool(d.get("skipped", False)),
            cooldown_remaining_s=d.get("cooldown_remaining_s"),
            quarantined=int(d.get("quarantined", 0)),
            cleaned_stale=int(d.get("cleaned_stale", 0)),
            orphan_graph_cleaned=int(d.get("orphan_graph_cleaned", 0)),
            status=d.get("status"),
            conflicts_detected=int(d.get("conflicts_detected", 0)),
            orphaned_scenes=int(d.get("orphaned_scenes", 0)),
            promoted=int(d.get("promoted", 0)),
            demoted=int(d.get("demoted", 0)),
            warnings=list(d.get("warnings", [])),
            decision_count=int(d.get("decision_count", 0)),
            scenes_created=int(d.get("scenes_created", 0)),
            candidates_found=int(d.get("candidates_found", 0)),
            candidates=list(d.get("candidates", [])),
        )


@dataclass
class ObserveResult:
    """Result of an observe() call."""

    memories: list[dict[str, Any]] = field(default_factory=list)
    warning: str | None = None

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ObserveResult:
        return cls(
            memories=list(d.get("memories", [])),
            warning=d.get("warning"),
        )
