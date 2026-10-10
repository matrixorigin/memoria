# Memoria Python SDK — Design Document

**Version:** v0.2  
**Date:** 2026-05-25  
**Author:** loveRhythm1990

---

## 1. Background and Goals

Today, Python integration with Memoria is done by calling the REST API directly (raw HTTP). There is no officially maintained Python client library. This leads to:

- Every integrator handling auth, error mapping, pagination, and retries on their own
- Client code drifting away from REST API documentation
- Customer demand for a pip-installable SDK with versioning and documentation

**Goals:**

1. Provide a thin Python HTTP client covering Memoria REST API core paths
2. Manage versions independently via `pyproject.toml`, publishable to PyPI
3. Place code in `sdk/python/` subdirectory of the Memoria main repository
4. Keep SDK semver independent from Memoria engine version; declare compatibility via a matrix

**Non-goals:**

- Not a Python binding to the Rust engine (no in-process calls)
- Does not replace MCP (MCP remains the preferred path for IDE agents; SDK targets application code)
- v1 does not cover admin / master-key-only endpoints

---

## 2. Repository Location and Directory Structure

Located as a subdirectory of the Memoria main repository:

```
Memoria/
├── memoria/                  # Rust workspace (unchanged)
├── plugins/                  # Existing plugins (unchanged)
├── sdk/
│   └── python/
│       ├── pyproject.toml    # Package metadata, dependencies
│       ├── README.md         # Quick start for users
│       ├── CHANGELOG.md      # Version history
│       ├── docs/
│       │   └── python_sdk_design.md    # This document
│       ├── src/
│       │   └── memoria/
│       │       ├── __init__.py       # Exports MemoriaClient / AsyncMemoriaClient
│       │       ├── client.py         # MemoriaClient + AsyncMemoriaClient entry points
│       │       ├── _http.py          # _HttpTransport shared base (URL/errors/parsing)
│       │       ├── models.py         # dataclass response models (zero extra deps)
│       │       ├── exceptions.py     # Exception hierarchy
│       │       └── resources/
│       │           ├── memories.py   # MemoriesResource + AsyncMemoriesResource
│       │           ├── snapshots.py  # SnapshotsResource + AsyncSnapshotsResource
│       │           ├── branches.py   # BranchesResource + AsyncBranchesResource
│       │           ├── profile.py    # ProfileResource + AsyncProfileResource
│       │           └── governance.py # GovernanceResource + AsyncGovernanceResource
│       └── tests/
│           ├── conftest.py           # Shared fixtures (sync + async, mock server)
│           ├── unit/                 # Unit tests (no running API required)
│           │   ├── test_memories.py
│           │   ├── test_memories_async.py
│           │   ├── test_snapshots.py
│           │   ├── test_branches.py
│           │   ├── test_governance.py
│           │   └── test_errors.py
│           └── integration/          # Integration tests (requires `make up`)
│               ├── README.md
│               └── test_e2e.py       # Covers sync and async paths
└── ...
```

**Release tag format:** `python-sdk-v1.0.0` (distinct from Rust engine tags)

---

## 3. Client API Design

### 3.1 MemoriaClient / AsyncMemoriaClient Initialization

```python
# Sync client (scripts, offline processing, non-async frameworks)
from memoria import MemoriaClient

client = MemoriaClient(
    base_url="http://localhost:8100",   # Required — any memoria serve instance
    api_key="sk-...",                   # Required — Bearer token (sole auth credential)
    timeout=30.0,                       # Optional, seconds, default 30
    max_retries=3,                      # Optional, retries for 5xx / network errors
)
client.ping()  # Returns True / raises MemoriaConnectionError

# Async client (FastAPI, asyncio scripts, async agent frameworks)
from memoria import AsyncMemoriaClient

async_client = AsyncMemoriaClient(
    base_url="http://localhost:8100",
    api_key="sk-...",
    timeout=30.0,
    max_retries=3,
)
await async_client.ping()
```

Both clients expose identical interfaces; the only difference is that `AsyncMemoriaClient` methods require `await`. For private deployments, sync and async versions ship together in the same wheel — no extra install step.

#### Client Lifecycle (Connection Management)

`httpx` maintains a connection pool. Close explicitly when done to avoid leaks:

```python
# Recommended: context manager (auto-close)
with MemoriaClient(base_url=..., api_key=...) as client:
    mem = client.memories.store(content="...")

async with AsyncMemoriaClient(base_url=..., api_key=...) as client:
    mem = await client.memories.store(content="...")

# Manual close (long-lived singleton, e.g. app-global client)
client = MemoriaClient(...)
# ... use ...
client.close()

async_client = AsyncMemoriaClient(...)
# ... use ...
await async_client.aclose()
```

#### Authentication

The Python SDK only needs an API Key — the sole auth credential, no other headers required:

```
Authorization: Bearer sk-xxxxxxxx
```

**How API keys work:**

- Server SHA-256-hashes the key and looks up `mem_api_keys`
- Automatically resolves the corresponding `user_id` (callers do not pass `user_id` manually)
- Lookup results cached for 5 minutes to avoid DB hits on every request
- Keys may have `expires_at`; expired keys fail auth

**The SDK does NOT need:**

- `X-User-Id` header (MCP proxy scenario only; not required for direct REST)
- Master key (admin-only; SDK should not handle it)
- Session token / OAuth / refresh token (Memoria has no such mechanism)

### 3.2 memories Resource (Core — v1 Required)

```python
# Store
mem = client.memories.store(
    content="...",
    memory_type="semantic",   # semantic|profile|procedural|working|tool_result|episodic
    session_id=None,          # Optional
    trust_tier=None,          # Optional: T1|T2|T3|T4 (T1 = highest confidence, T4 = unverified)
)

# Batch store (max 100 items, max 32 KiB per content)
mems = client.memories.store_batch([
    {"content": "...", "memory_type": "semantic"},
    {"content": "...", "memory_type": "profile"},
])
# Request body field name on server: "memories" (not "items")

# Retrieve (hybrid vector + fulltext)
# Returns RetrieveResult (items list + optional explain)
result = client.memories.retrieve(
    query="...",
    top_k=5,              # Default 5, max 100
    session_id=None,      # Optional — boosts session context when set
    session_scope=None,   # prefer (default) | only; requires session_id when set
    explain=False,        # False|True|"verbose"|"analyze"
)
# result.items: list[Memory]
# result.explain: dict | None (present when explain != false)

# Search (no session weighting)
result = client.memories.search(
    query="...",
    top_k=10,             # Default 10
    session_id=None,
    session_scope=None,
    explain=False,
)

# List (paginated)
# Default limit=100, max 500; cursor is next_cursor from previous page
page = client.memories.list(
    limit=100,
    cursor=None,        # Pagination cursor (memory_id hex format)
    memory_type=None,
    session_id=None,
    trust_tier=None,    # T1|T2|T3|T4
    branch=None,        # Optional one-shot active branch override
)
# page.items: list[Memory]
# page.next_cursor: str | None (None = last page)

# Correct (by ID)
# Note: correct() creates a NEW memory with a new ID; the original ID is superseded
mem = client.memories.correct(id="...", new_content="...", reason="...")

# Correct (by semantic query — finds closest matching entry)
mem = client.memories.correct_by_query(query="...", new_content="...", reason="...")

# Delete (single)
client.memories.delete(id="...", reason="...")

# Bulk purge (select one selector — mutually exclusive)
# Option A: by memory ID list
result = client.memories.purge(
    memory_ids=["id1", "id2"],
    reason="task complete",
)
# Option B: by topic keyword
result = client.memories.purge(
    topic="debug session",
    reason="cleanup",
)
# Option C: by session_id (optionally with memory_types filter)
result = client.memories.purge(
    session_id="sess_abc",
    memory_types=["working"],    # memory_types must be used with session_id
    reason="session ended",
)
# result.purged: int
# result.snapshot_name: str (auto snapshot name)

# Feedback (affects retrieval ranking)
client.memories.feedback(id="...", signal="useful", context="...")
# signal: useful | irrelevant | outdated | wrong
```

### 3.3 observe (Session Observation — Auto Memory Extraction)

```python
client.observe(
    messages=[
        {"role": "user", "content": "..."},
        {"role": "assistant", "content": "..."},
    ],
    session_id=None,   # Optional
)
```

REST-only endpoint — no corresponding MCP tool, but included in v1 SDK.

### 3.4 profile

```python
profile = client.profile.me()
# Returns Profile dataclass:
#   profile.user_id: str
#   profile.profile: str  (text summary from profile-type memories)
#   profile.stats:   dict (memory counts etc.)
```

### 3.5 snapshots (v1)

```python
snap = client.snapshots.create(name="before_cleanup", description="...")
snaps = client.snapshots.list(limit=20, offset=0)
client.snapshots.rollback(name="before_cleanup")

# Delete — two REST paths, unified in delete() method
# Single: DELETE /v1/snapshots/{name}
client.snapshots.delete(name="before_cleanup")
# Bulk / prefix / date: POST /v1/snapshots/delete
# (MCP memory_snapshot_delete uses this path too)
client.snapshots.delete(names=["snap1", "snap2"])
client.snapshots.delete(prefix="pre_")
client.snapshots.delete(older_than="2026-01-01")   # ISO date format
```

> **Note:** Server normalizes snapshot/branch names (dashes → underscores). Use underscores in names to avoid mismatch.

### 3.6 branches (v1)

```python
client.branches.create(name="experiment_1")
branches = client.branches.list()
# Server returns {"branches": [...], "result": "..."} — SDK unwraps automatically
client.branches.checkout(name="experiment_1")

# diff: high-level stats (no pagination) — GET /v1/branches/{name}/diff
diff = client.branches.diff(name="experiment_1")

# diff_items: per-item changes (paginated) — GET /v1/branches/{name}/diff-items
items = client.branches.diff_items(name="experiment_1", limit=50)

# merge: default strategy="accept" (accept|replace|append)
# merge is fully disabled in group mode (403)
client.branches.merge(name="experiment_1", strategy="accept")
client.branches.delete(name="experiment_1")

# branches.apply: selective apply — promote specific entries from branch to main
# Corresponds to MCP memory_apply / POST /v1/branches/{name}/apply
# Returns ApplyResult dataclass
result = client.branches.apply(
    name="experiment_1",
    adds=["mem_id_1", "mem_id_2"],
    removes=["mem_id_old"],
    updates=[{"old_id": "x", "new_id": "y"}],
    accept_branch_conflicts=[],
)
# result.applied_adds / result.skipped_adds etc. — see ApplyResult dataclass

# branches.pick: advanced selective apply with selectors and dry_run
# Corresponds to MCP memory_pick / POST /v1/branches/{name}/pick
result = client.branches.pick(
    name="experiment_1",
    selector={"type": "key_list", "keys": ["mem_id_1"]},
    strategy="fail",    # fail|skip|accept, default fail (safest)
    target="main",
    dry_run=None,       # Pass {} for preview without writing
)
```

### 3.7 governance (v1 — Aligned with MCP Tools)

Three methods map 1:1 to MCP `memory_governance` / `memory_consolidate` / `memory_reflect`:

```python
result = client.governance.run(force=False)
# result.skipped == True  → on cooldown (1-hour cooldown)
# result.skipped == False → executed; quarantined / cleaned_stale populated

result = client.governance.consolidate(force=False)
# result.skipped == True  → on cooldown (30-minute cooldown)
# result.skipped == False → conflicts_detected / promoted / demoted populated

result = client.governance.reflect(force=False, mode="auto")
# mode="auto" (default):
#   Subject to 2-hour cooldown; returns result.skipped == True when cooling down
#   On success (with LLM): result.scenes_created / result.candidates_found populated
# mode="candidates":
#   ⚠️ Not subject to cooldown; always returns immediately, result.skipped always False
#   result.candidates: list (raw candidate clusters for caller to process)
# mode="internal" (returns 503 when LLM not configured)

# Bypass cooldown (use with caution)
client.governance.run(force=True)
```

### 3.8 Active Branch and Group Mode

#### Active Branch (Server-Maintained)

Memoria maintains an "active branch" per user/group on the server, initially `main`. After checkout, all requests without `branch=` use the active branch:

```python
client.branches.checkout(name="experiment_1")
# Subsequent store/retrieve/correct default to experiment_1
```

The `branch=` parameter overrides the active branch for a single request without changing server state:

```python
client.memories.store(content="...", branch="hotfix")  # One-shot only
```

#### Group Mode

When an API key is bound to a `group_id` in the DB, the key enters group mode:

- `scope_id = group_id`; all operations share the group database
- `merge` is fully disabled (403)

**Write restrictions in group mode (important):**

Multi-member group (member count > 1):

- `main` branch is read-only; writes return 403
- `governance.run()` disabled (403 — would modify main)

Solo owner group (owner is the only active member):

- Direct writes to `main` allowed — same as personal mode
- `governance.run()` also allowed
- Server auto-detects via `group_main_write_allowed_for_solo_owner`; SDK needs no special handling

**Standard workflow in multi-member group:**

```python
# 1. Create your own branch (once, then reuse)
client.branches.create(name="alice_work")
client.branches.checkout(name="alice_work")

# 2. Write normally (to alice_work branch)
client.memories.store(content="...")

# 3. Promote selected entries to main via pick or apply
client.branches.pick(
    name="alice_work",
    selector={"type": "key_list", "keys": ["mem_id_1"]},
)
```

> ⚠️ In multi-member group, calling `store` / `correct` / `purge` on `main` returns 403. Catch `MemoriaForbiddenError` and prompt the user to checkout their branch first.

#### No Special Group Mode Wrapper in SDK v1

The SDK does not auto-detect group mode or manage branches automatically. `MemoriaForbiddenError` is sufficient; callers decide next steps.

---

## 4. Data Models

Use standard library `dataclasses` — no Pydantic (compatibility first, zero extra dependencies):

```python
from dataclasses import dataclass, field
from datetime import datetime

@dataclass
class Memory:
    memory_id: str
    content: str
    memory_type: str        # semantic|profile|procedural|working|tool_result|episodic
    trust_tier: str         # T1|T2|T3|T4
    initial_confidence: float
    is_active: bool
    user_id: str
    author_id: str | None = None        # Original author in group mode; None in personal mode
    session_id: str | None = None
    observed_at: datetime | None = None
    created_at: datetime | None = None
    retrieval_score: float | None = None  # Present on retrieve; None on list()

@dataclass
class MemoryPage:
    items: list[Memory]
    next_cursor: str | None = None

@dataclass
class PurgeResult:
    purged: int
    snapshot_name: str | None = None
    warning: str | None = None    # e.g. too many memories, auto-snapshot failure

@dataclass
class Snapshot:
    name: str
    created_at: datetime
    description: str | None = None

@dataclass
class Branch:
    name: str
    active: bool = False
    created_at: datetime | None = None  # main may lack this field in list()

@dataclass
class ApplyResult:
    applied_adds: list[str] = field(default_factory=list)
    applied_updates: list[str] = field(default_factory=list)
    applied_removes: list[str] = field(default_factory=list)
    applied_conflicts: list[str] = field(default_factory=list)
    skipped_adds: list[str] = field(default_factory=list)
    skipped_updates: list[str] = field(default_factory=list)
    skipped_removes: list[str] = field(default_factory=list)
    skipped_conflicts: list[str] = field(default_factory=list)

@dataclass
class Profile:
    user_id: str
    profile: str           # Text summary from profile memories
    stats: dict            # Memory counts etc. (passthrough; structure may change)

@dataclass
class GovernanceResult:
    skipped: bool = False
    cooldown_remaining_s: int | None = None
    # governance results
    quarantined: int = 0
    cleaned_stale: int = 0
    orphan_graph_cleaned: int = 0
    # consolidate results
    status: str | None = None
    conflicts_detected: int = 0
    orphaned_scenes: int = 0
    promoted: int = 0
    demoted: int = 0
    warnings: list[str] = field(default_factory=list)
    decision_count: int = 0
    # reflect results (with LLM)
    scenes_created: int = 0
    candidates_found: int = 0
    # reflect mode="candidates" or without LLM
    candidates: list = field(default_factory=list)

@dataclass
class RetrieveResult:
    items: list[Memory]
    explain: dict | None = None   # Present when explain != false
```

**Design principles:**

- Parse response JSON via factory methods like `Memory.from_dict(d)`; ignore unknown fields
- Date fields parsed to `datetime` objects (not strings)
- Optional fields have defaults; avoid `Any`
- No `pip install pydantic` — reduces dependency conflict risk for customers

---

## 5. Error Handling

### Exception Hierarchy

```
MemoriaError                         # Base class for all SDK exceptions
├── MemoriaConnectionError           # Network unreachable, timeout
├── MemoriaAPIError                  # HTTP 4xx/5xx
│   ├── MemoriaAuthError             # 401 (invalid token / rate limit exceeded)
│   ├── MemoriaForbiddenError        # 403 (group multi-member write to main rejected)
│   ├── MemoriaNotFoundError         # 404
│   ├── MemoriaUnprocessableError    # 422 (server-side validation failure)
│   └── MemoriaServerError           # 500
└── MemoriaValidationError           # Local validation failure (request not sent)
```

`MemoriaAPIError` carries:

- `status_code: int`
- `detail: str` (from response `{"detail": "..."}`)

### 422 Unprocessable Entity

422 is server-side validation failure — distinct from SDK local validation (`MemoriaValidationError`).

Common triggers:

| Endpoint | Triggers |
|----------|----------|
| `store` / `store_batch` | Empty content; content > 32 KiB; invalid `memory_type`; invalid `trust_tier` |
| `store_batch` | More than 100 items |
| `retrieve` / `search` / `correct_by_query` | `session_scope` set but `session_id` missing |
| `purge` | `memory_types` set but `session_id` missing |
| `list` | Invalid `trust_tier` |

SDK raises `MemoriaUnprocessableError` with `detail`. Usually a caller bug — do not retry.

### 401 and Rate Limiting

Memoria's rate limiting is implemented in the auth layer (`rate_limit.rs`). When exceeded, `validate_api_key()` returns `None` → auth failure → **401**.

Therefore 401 may mean:

1. Invalid / expired API key
2. Request count exceeded sliding window limit for this key

The SDK cannot distinguish these from status code alone. Response `detail` may help, but Memoria currently does not provide distinct messages for rate limit vs invalid key. In practice:

- Repeated 401 with a confirmed-valid key → likely rate limit
- SDK always raises `MemoriaAuthError` for 401; caller decides whether to retry

### Rate Limit Mechanism

Memoria has a global, per-API-key, in-process sliding window rate limiter:

| Property | Value |
|----------|-------|
| Granularity | Per API key hash; all endpoints share one counter |
| Default | 1000 requests per 60 seconds per key |
| Config | Env var `MEMORIA_RATE_LIMIT_AUTH_KEYS=max,window_secs` |
| Storage | In-process memory; counters reset on restart |
| Response on exceed | HTTP **401** (not 429) |

`store_batch` and single `store` share the same quota — no separate batch quota. SDK does not implement client-side rate limiting, but docs should warn about batch call quota consumption.

### 403 Forbidden

In group mode, writing to `main` returns 403:

```
"main is read-only in group mode; create or checkout a branch, then use selective apply"
```

Personal mode users won't see this. SDK exposes `MemoriaForbiddenError` for group-mode developers to detect and switch branches.

### Retry Policy

Retry eligibility depends on whether repeating the request can create something.
Idempotent methods (GET/HEAD/PUT/DELETE/OPTIONS/TRACE) are safe to repeat;
non-idempotent ones (POST/PATCH) are not, and no status code or transport error
proves the server did *not* already commit the write — a gateway can return 504
while the upstream keeps going and commits, and a connection can drop after the
write but before the response. Without an idempotency key there is no way to
retry such a write safely, so by default the SDK does not.

Eligibility is per *operation*, not purely per HTTP verb: `memories.correct()`
uses `PUT /v1/memories/{id}/correct` but is **not** idempotent — the server
mints a replacement record and supersedes the original, so a replay either 404s
on the already-superseded memory or creates a second replacement. It is
classified with the non-idempotent column below.

| Condition | Idempotent (GET/PUT/DELETE/…) | Non-idempotent (POST/PATCH, `memories.correct`) |
|-----------|-------------------------------|------------------------------|
| 500 | Yes — exponential backoff (`max_retries=3`) | No |
| 502 / 503 / 504 | Yes | No — opt in with `retry_unsafe_writes=True` |
| `ConnectError` (never reached the server) | Yes | Yes |
| Other transport errors (timeout, dropped connection, disconnect before response, proxy failure) | Yes | No — opt in with `retry_unsafe_writes=True` |
| 401 / 403 / 422 / other 4xx | No | No |

Every `httpx.TransportError` — including `RemoteProtocolError` ("server
disconnected without sending a response"), which is a sibling of `NetworkError`
rather than a subclass — is surfaced as `MemoriaConnectionError`, with the
original exception preserved as `__cause__`.

`retry_unsafe_writes=True` trades at-most-once for resilience: a write whose
response was lost may be committed twice. Note that `memories.store` is
additionally protected server-side by near-duplicate detection, which supersedes
rather than duplicates identical content; other write endpoints are not.

---

## 6. HTTP Transport Implementation

- Uses `httpx` — sync (`httpx.Client`) and async (`httpx.AsyncClient`)
- All requests include by default:
  - `Authorization: Bearer <api_key>`
  - `Content-Type: application/json`
  - `User-Agent: memoria-python/<version>`
- Connect timeout and read timeout configured separately
- Non-JSON success responses (e.g. `/health` returning plain `ok`) handled gracefully

### Shared Sync / Async Architecture

URL construction, validation, error mapping, and dataclass parsing live in a shared base class. Only `_request()` differs between sync and async:

```python
class _HttpTransport:
    def _build(self, method, path, **kw): ...   # URL, headers — shared
    def _parse(self, resp): ...                  # Response parsing, error mapping

class MemoriaClient(_HttpTransport):
    _http = httpx.Client(...)
    def _request(self, method, path, **kw):
        return self._parse(self._http.request(...))

class AsyncMemoriaClient(_HttpTransport):
    _http = httpx.AsyncClient(...)
    async def _request(self, method, path, **kw):
        return self._parse(await self._http.request(...))
```

Resource layer (`memories.py`, `branches.py`, etc.) maintains symmetric sync/async classes:

```python
class MemoriesResource:
    def store(self, content, ...):
        return self._client._request("POST", "/v1/memories", ...)

class AsyncMemoriesResource:
    async def store(self, content, ...):
        return await self._client._request("POST", "/v1/memories", ...)
```

Method bodies are identical except for `async def` / `await`. Models, exceptions, and URL constants are not duplicated.

Reference: `plugins/openclaw/openclaw/http-client.ts` dispatch structure — Python side returns dataclass models directly, not MCP-wrapped format.

---

## 7. v1 vs v1.1 Coverage

### v1.0 (MVP — Current Delivery)

| Method | Endpoint | SDK Method |
|--------|----------|------------|
| POST | `/v1/memories` | `store` |
| POST | `/v1/memories/batch` | `store_batch` |
| POST | `/v1/memories/retrieve` | `retrieve` |
| POST | `/v1/memories/search` | `search` |
| GET | `/v1/memories` | `list` |
| PUT | `/v1/memories/{id}/correct` | `correct` |
| POST | `/v1/memories/correct` | `correct_by_query` |
| DELETE | `/v1/memories/{id}` | `delete` |
| POST | `/v1/memories/purge` | `purge` |
| POST | `/v1/memories/{id}/feedback` | `feedback` |
| POST | `/v1/observe` | `observe` |
| GET | `/v1/profiles/me` | `profile.me` |
| POST | `/v1/snapshots` | `snapshots.create` |
| GET | `/v1/snapshots` | `snapshots.list` |
| POST | `/v1/snapshots/{name}/rollback` | `snapshots.rollback` |
| DELETE | `/v1/snapshots/{name}` | `snapshots.delete(name=...)` |
| POST | `/v1/snapshots/delete` | `snapshots.delete(names/prefix/older_than)` |
| POST | `/v1/branches` | `branches.create` |
| GET | `/v1/branches` | `branches.list` |
| POST | `/v1/branches/{name}/checkout` | `branches.checkout` |
| GET | `/v1/branches/{name}/diff` | `branches.diff` |
| GET | `/v1/branches/{name}/diff-items` | `branches.diff_items` |
| POST | `/v1/branches/{name}/merge` | `branches.merge` |
| DELETE | `/v1/branches/{name}` | `branches.delete` |
| POST | `/v1/branches/{name}/apply` | `branches.apply` |
| POST | `/v1/branches/{name}/pick` | `branches.pick` |
| POST | `/v1/governance` | `governance.run` |
| POST | `/v1/consolidate` | `governance.consolidate` |
| POST | `/v1/reflect` | `governance.reflect` |
| GET | `/health` | `ping` |

### v1.1 (Future)

- `GET /v1/memories/{id}` — `memories.get`
- `GET /v1/feedback/stats` — `feedback.stats`
- `GET /v1/retrieval-params` — `retrieval_params.get`
- `POST /v1/retrieval-params/tune` — `retrieval_params.tune`
- `GET /v1/entities` — `entities.list`
- `POST /v1/extract-entities` — `entities.extract`
- `POST /v1/extract-entities/link` — `entities.link`
- `POST /v1/sessions/{id}/summary` — `sessions.summarize`
- `GET /v1/tasks/{id}` — `tasks.get`
- `GET /v1/snapshots/{name}` — `snapshots.get`
- `GET /v1/snapshots/{name}/diff` — `snapshots.diff`

### Not Covered (Admin / Master-Key Endpoints)

- `/auth/keys` — SDK does not wrap key management (self-hosted: call with master key directly; cloud users use dashboard)
- `/admin/*` — requires master key
- `/admin/plugins/*`

---

## 8. Packaging and Release

### 8.1 pyproject.toml Key Fields

```toml
[project]
name = "memoria-client"
version = "1.0.0"
requires-python = ">=3.10"
dependencies = [
    "httpx>=0.27",
]
# Note: no pydantic — response models use stdlib dataclasses

[project.optional-dependencies]
dev = ["pytest", "pytest-httpx", "pytest-asyncio", "anyio[trio]", "ruff", "mypy"]
```

`pyproject.toml` follows PyPI standards from day one. v1 prioritizes wheel delivery but format is PyPI-ready — `uv publish` works when ready.

### 8.2 Build Artifacts

- `py3-none-any.whl` — pure Python, cross-platform single file (v1 primary deliverable)
- `memoria_client-1.0.0.tar.gz` — source distribution (generated alongside, backup)

### 8.3 v1 Release Strategy (Wheel First, PyPI Compatible)

v1 primary delivery: **GitHub Release wheel**

- No need to claim PyPI package name upfront
- Customers download via fixed URL; version controlled
- Private / offline customers use the same wheel

**Installation (v1):**

```bash
# Option A: Direct GitHub Release URL (requires network + GitHub access)
pip install https://github.com/matrixorigin/Memoria/releases/download/python-sdk-v1.0.0/memoria_client-1.0.0-py3-none-any.whl

# Option B: Local install (private / offline)
pip install ./memoria_client-1.0.0-py3-none-any.whl

# Option C (future v1.x): After PyPI release
pip install memoria-client==1.0.0
```

All three install the same wheel with identical behavior.

**PyPI compatibility guarantees (required even in v1):**

| Requirement | Status |
|-------------|--------|
| Build tool: `python -m build` (produces `.whl` + `.tar.gz`) | ✅ |
| PyPI-compliant structure (`src` layout + `pyproject.toml`) | ✅ |
| Complete wheel metadata (name/version/requires-python/deps) | ✅ |
| CI validation: `twine check dist/*` passes (even without upload) | ✅ |

When ready for PyPI: `uv publish` or `twine upload dist/*`

### 8.4 Offline Bundle (Private Customers)

Add to root Makefile or `sdk/python/Makefile`:

```makefile
sdk-bundle:
    cd sdk/python && \
    python -m build --wheel && \
    pip download dist/*.whl -d dist/bundle/ && \
    echo "Offline bundle: sdk/python/dist/bundle/"
```

Customer installs from bundle:

```bash
pip install --no-index --find-links=./bundle memoria-client==1.0.0
```

### 8.5 CI/CD

**Lint / Type check (each PR, scoped to `sdk/python/`):**

- `ruff check src/`
- `mypy src/`
- `twine check dist/*` (after build; no upload)

> PR CI workflow not yet implemented — recommended as separate `sdk-python-ci.yml`.

**Release workflow** (`.github/workflows/sdk-python-release.yml`)

| Property | Value |
|----------|-------|
| Trigger | Push tag `python-sdk-v*` |
| v1 target | GitHub Release wheel only — no PyPI step |

**Flow:**

1. `actions/checkout`
2. `actions/setup-python@v5` (python-version: `"3.10"`)
3. **Verify tag matches `pyproject.toml` version** (fail fast on mismatch)
4. `pip install build twine`
5. `cd sdk/python && python -m build`
   - Output: `dist/memoria_client-x.y.z-py3-none-any.whl`, `dist/memoria_client-x.y.z.tar.gz`
6. `twine check dist/*`
7. `pytest tests/unit/` (no running API required)
8. `softprops/action-gh-release@v2` — create Release + upload `.whl` + `.tar.gz`

**Workflow excerpt:**

```yaml
on:
  push:
    tags: ["python-sdk-v*"]

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.10"
      - name: Verify tag matches package version
        run: |
          TAG="${GITHUB_REF_NAME#python-sdk-v}"
          VERSION=$(python -c "import tomllib; print(tomllib.load(open('sdk/python/pyproject.toml','rb'))['project']['version'])")
          test "$TAG" = "$VERSION"
      - run: pip install build twine
      - run: cd sdk/python && python -m build
      - run: twine check sdk/python/dist/*
      - run: cd sdk/python && python -m pytest tests/unit/ -v
      - name: Create GitHub Release and upload wheel
        uses: softprops/action-gh-release@v2
        with:
          tag_name: ${{ github.ref_name }}
          generate_release_notes: true
          files: |
            sdk/python/dist/*.whl
            sdk/python/dist/*.tar.gz

# PyPI — add when ready:
# - uses: pypa/gh-action-pypi-publish@release/v1
#   with:
#     packages-dir: sdk/python/dist/
#     password: ${{ secrets.PYPI_API_TOKEN }}
```

---

## 9. Testing Strategy

Two test categories:

### 1. Unit Tests (`tests/unit/` — No Real Service)

- `pytest-httpx` mocks HTTP responses (sync and async clients)
- Covers: request construction, response parsing, error mapping
- Covers: retry logic (mock 5xx → success)
- Covers: 401 handling (invalid token / rate limit both return 401 → `MemoriaAuthError`)
- Async tests use `pytest-asyncio`; fixtures and mocks shared with sync tests

### 2. Integration Tests (`tests/integration/` — Requires Running Memoria)

- Local: `make python-sdk-test` from repo root
- Uses docker-compose Memoria instance
- End-to-end: store → retrieve → correct → purge
- Snapshots, branches create/rollback/list

#### API Key Generation Flow

Memoria auth has two layers:

```
master key (configured via env at startup)
  └─ POST /auth/keys (requires master key)
        └─ returns raw_key ("sk-..." Bearer token)
              └─ used for all normal API calls
```

**Step 1 — Configure master key in docker-compose / conftest**

```yaml
environment:
  MASTER_KEY: test-master-key-for-ci
```

**Step 2 — Create test API key in conftest via master key**

```python
@pytest.fixture(scope="session")
def api_key(base_url):
    import httpx
    master_key = os.environ["MEMORIA_MASTER_KEY"]
    user_id = f"pytest_{uuid.uuid4().hex[:8]}"
    resp = httpx.post(
        f"{base_url}/auth/keys",
        headers={"Authorization": f"Bearer {master_key}"},
        json={"user_id": user_id, "name": "pytest-sdk-test"},
    )
    resp.raise_for_status()
    raw_key = resp.json()["raw_key"]
    yield raw_key
```

**Step 3 — Initialize MemoriaClient with raw_key**

```python
@pytest.fixture(scope="session")
def client(base_url, api_key):
    return MemoriaClient(base_url=base_url, api_key=api_key)
```

**Isolation strategy:**

- Per-test isolation: fixture scope `"function"`, new `user_id` + API key each test
- Session scope reuse: cheaper, but prefix content with unique IDs per test

**Notes:**

- Master key exists only in test environments; never commit to git — inject via GitHub Secret / CI env
- If Memoria has no master key configured, `/auth/keys` is unavailable; integration tests require `MASTER_KEY`

#### Local Run (`make python-sdk-test`)

Root Makefile includes `.env` and exports all variables. Targets:

```makefile
# Integration tests (requires `make up` first) — unit + integration
python-sdk-test: check-env
    @API_PORT=$${API_PORT:-8100}; \
    curl -sf --noproxy localhost http://localhost:$$API_PORT/health \
        > /dev/null || \
    (echo "❌ Memoria API not running — run: make up"; exit 1)
    @cd sdk/python && \
        MEMORIA_BASE_URL=$(MEMORIA_BASE_URL) \
        MEMORIA_MASTER_KEY=$${MEMORIA_MASTER_KEY} \
        python -m pytest tests/unit/ tests/integration/ -v

# Unit tests only — no API required
python-sdk-test-unit:
    @cd sdk/python && python -m pytest tests/unit/ -v
```

**Local workflow:**

1. `make up` — start MatrixOne + API
2. `make python-sdk-test` — health check + full test suite

Or: `make python-sdk-test-unit` — unit tests only, no API needed

**Prerequisites:**

- `MEMORIA_MASTER_KEY` in `.env` (auto-exported by Makefile)
- Python env with pytest and dev deps (`uv sync` / `pip install -e ".[dev]"`)
- Valid `EMBEDDING_API_KEY` + `EMBEDDING_BASE_URL` for embedding-dependent tests (otherwise skipped)

#### Embedding Impact on Integration Tests

If the API container has no valid `EMBEDDING_API_KEY`, vector retrieval degrades to fulltext-only (no error, reduced precision).

| Test type | Impact |
|-----------|--------|
| SDK correctness (request/response parsing) | Unaffected |
| Semantic ranking accuracy | Degraded to fulltext; tests assert "returns results" not "rank N is X" |

Integration tests use a `has_embedding` fixture — embedding-dependent tests are **skipped** (not failed) when embedding is unavailable.

Documented in `tests/integration/README.md`:

> Integration tests run against fulltext-only retrieval if EMBEDDING_API_KEY is not configured — semantic ranking is not tested in this environment.

**Testing principles:**

- Each Resource method: at least one happy path + one error path
- No 100% coverage goal — prioritize core paths and error mapping

---

## 10. Version Compatibility Matrix

| SDK Version | Supported Memoria API | Min Python |
|-------------|----------------------|------------|
| 1.0.x | >= 0.2.3 | 3.10 |
| 1.1.x | >= 0.2.3 | 3.10 |

Notes:

- Memoria API >= 0.2.3 supports Streamable HTTP MCP (same `/mcp` port)
- REST API paths `/v1/*` stable from 0.2.3 onward

---

## 11. Milestones and Effort Estimate

| Phase | Deliverable | Estimate |
|-------|-------------|----------|
| M1 Skeleton | Directory structure + pyproject.toml + `_HttpTransport` + `MemoriaClient` / `AsyncMemoriaClient` init + ping | 0.5 day |
| M2 Core API | All memories endpoints + error hierarchy + sync/async unit tests | 2 days |
| M3 Complete API | snapshots / branches / observe / profile / governance (sync+async) | 1.5 days |
| M4 Release Pipeline | CI workflow + GitHub Release wheel + README (PyPI deferred) | 0.5 day |
| M5 Documentation | Quick start + compatibility matrix + CHANGELOG | 0.5 day |
| **Total** | | **~5 days** |

Note: ~1 day more than sync-only (async Resource methods + pytest-asyncio fixtures; shared base class eliminates most duplication).

---

## 12. Decisions and Open Questions

### Q1. PyPI Package Name

⚠️ **Open — confirm before PyPI release:**

1. Check pypi.org for `memoria-client` / `memoria_client` availability
2. Distribution name (PyPI) vs import name (code) can differ:
   - `pip install memoria-client` → `import memoria`
   - Or: `pip install memoria-client` → `from memoria_client import MemoriaClient`

v1 delivers via GitHub Release wheel; PyPI path kept compatible but not actively pushed in v1.

### Q2. requires-python Lower Bound

✅ **Decided:** `>=3.10`

### Q3. Response Models

✅ **Decided:** Standard library `dataclasses` — no Pydantic. Zero extra dependencies, best compatibility.

### Q4. governance / consolidate / reflect in v1

✅ **Decided.** MCP exposes these three tools; SDK surface aligns with MCP.

**Cooldown behavior:**

During cooldown, API returns HTTP 200 (not error, not 429):

```json
{ "skipped": true, "cooldown_remaining_s": 3540 }
```

On success:

| Method | Response fields |
|--------|----------------|
| governance | `quarantined`, `cleaned_stale`, `orphan_graph_cleaned` |
| consolidate | `status`, `conflicts_detected`, `promoted`, `demoted`, ... |
| reflect (with LLM, mode != "candidates") | `scenes_created`, `candidates_found` |
| reflect mode="candidates" (no cooldown) | `candidates: [...]` |

SDK returns unified `GovernanceResult` dataclass:

```python
result = client.governance.run()
if result.skipped:
    print(f"On cooldown, wait {result.cooldown_remaining_s}s")
else:
    print(f"Cleaned {result.cleaned_stale} stale memories")
```

All three methods support `force=True` to bypass cooldown.

### Q5. Local Integration Test Command

✅ **Decided:** `make python-sdk-test` / `make python-sdk-test-unit`

- `make python-sdk-test` — integration tests (requires `make up`)
- `make python-sdk-test-unit` — unit tests only (no API)

CI: run `python-sdk-test-unit` on PR; optional integration job on tag release.

### Q6. GitHub CI Auto-Release Wheel

✅ **Decided.** Workflow triggered by tag `python-sdk-v*`, builds wheel and publishes to GitHub Releases. Tag version must match `pyproject.toml` version.
