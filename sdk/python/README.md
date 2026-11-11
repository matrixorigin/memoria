# Memoria Python SDK

Official Python SDK for the [Memoria](https://github.com/matrixorigin/Memoria) memory engine.

## Installation

**v1 — GitHub Release wheel:**

```bash
pip install https://github.com/matrixorigin/Memoria/releases/download/python-sdk-v1.0.0/memoria_client-1.0.0-py3-none-any.whl
```

**Offline / air-gapped:**

```bash
pip install ./memoria_client-1.0.0-py3-none-any.whl
```

**From source (development):**

```bash
pip install -e ".[dev]"
```

## Quick Start

```python
from memoria import MemoriaClient

with MemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
    # Store a memory
    mem = client.memories.store(content="Prefers concise answers", memory_type="profile")

    # Retrieve relevant memories
    result = client.memories.retrieve(query="answer style", top_k=5)
    for item in result.items:
        print(item.content)

    # Observe a conversation turn (auto-extracts memories)
    client.observe(messages=[
        {"role": "user", "content": "What is the capital of France?"},
        {"role": "assistant", "content": "Paris."},
    ])
```

### Async usage

```python
from memoria import AsyncMemoriaClient

async with AsyncMemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
    await client.ping()
    mem = await client.memories.store(content="...", memory_type="semantic")
    result = await client.memories.retrieve(query="...")
```

### Retry configuration

Both clients accept `max_retries` (default: `3`), the number of retries after the
initial request. Set it to `0` to send a single attempt without retries. It must
be a non-negative integer; negative values, booleans, and non-integer values raise
`MemoriaValidationError` when the client is constructed, before an HTTP client is
allocated. This setting only limits attempts permitted by the existing retry
policy; it does not enable retries of unsafe writes.

## Resources

### memories

```python
# Store
mem = client.memories.store(content="...", memory_type="semantic")

# Batch store (max 100 items, max 32 KiB per item)
mems = client.memories.store_batch([{"content": "a"}, {"content": "b"}])

# Retrieve (hybrid vector + fulltext)
result = client.memories.retrieve(query="...", top_k=5)

# Search
result = client.memories.search(query="...", top_k=10)

# Pure MatrixOne full-text search with structured pre-filters (query max 4096 UTF-8 bytes)
result = client.memories.fulltext_search(
    "MatrixOne database",
    extra_metadata_filter={"scene": "incident"},
    subject_id="subject-123",
    session_id="session-123",
    limit=20,
)
# session_id is an exact filter: memories with session_id=None are excluded.
# Metadata preserves JSON type families: 2 may match 2.0, but not the string "2".

# List with pagination
page = client.memories.list(limit=100, cursor=None)
# page.next_cursor — pass as cursor= to get the next page

# Exact structured query (no vector/keyword retrieval)
page = client.memories.query(
    extra_metadata_filter={"scene": "incident", "rank": 2},
    memory_types=["semantic"],
    limit=100,
)

# Correct by ID
mem = client.memories.correct("mem_id", new_content="...", reason="...")

# Correct by semantic query
mem = client.memories.correct_by_query(query="old content", new_content="new content")

# Delete
client.memories.delete("mem_id", reason="done")

# Purge (choose one selector)
result = client.memories.purge(memory_ids=["id1", "id2"], reason="cleanup")
result = client.memories.purge(topic="debug session", reason="done")
result = client.memories.purge(session_id="sess_x", memory_types=["working"], reason="end")

# Feedback
client.memories.feedback("mem_id", signal="useful")  # useful|irrelevant|outdated|wrong
```

### snapshots

```python
snap = client.snapshots.create(name="before-cleanup")
snaps = client.snapshots.list(limit=20)
client.snapshots.rollback("before-cleanup")

client.snapshots.delete("before-cleanup")               # single
client.snapshots.delete(names=["s1", "s2"])             # multiple
client.snapshots.delete(prefix="pre_")                  # by prefix
client.snapshots.delete(older_than="2026-01-01")        # by date
```

### branches

```python
client.branches.create(name="experiment-1")
branches = client.branches.list()
client.branches.checkout(name="experiment-1")

diff = client.branches.diff("experiment-1")             # summary stats
items = client.branches.diff_items("experiment-1", limit=50)  # per-entry

client.branches.merge("experiment-1", strategy="accept")
client.branches.apply("experiment-1", adds=["mem_id_1"])
client.branches.pick("experiment-1", selector={"type": "key_list", "keys": ["mem_id_1"]})
client.branches.delete("experiment-1")
```

### governance

```python
result = client.governance.run()
if result.skipped:
    print(f"Cooldown: {result.cooldown_remaining_s}s remaining")
else:
    print(f"Cleaned {result.cleaned_stale} stale memories")

result = client.governance.consolidate()
result = client.governance.reflect(mode="auto")    # auto|candidates|internal
result = client.governance.reflect(mode="candidates")  # never on cooldown

# Bypass cooldown
client.governance.run(force=True)
```

## Error Handling

```python
from memoria import (
    MemoriaConnectionError,   # network unreachable / timeout
    MemoriaAPIError,          # base class for all HTTP error responses
    MemoriaAuthError,         # 401 — invalid key or rate-limit exceeded
    MemoriaForbiddenError,    # 403 — e.g. write to main in multi-member group mode
    MemoriaNotFoundError,     # 404
    MemoriaUnprocessableError,# 422 — server-side validation (empty content, bad type, etc.)
    MemoriaServerError,       # 5xx
    MemoriaValidationError,   # local validation (request not sent)
    MemoriaResponseError,     # malformed successful memory page response
)

try:
    mem = client.memories.store(content="")
except MemoriaUnprocessableError as e:
    print(f"Validation failed: {e.detail}")
except MemoriaAuthError:
    print("Check your API key, or you may have hit the rate limit")
```

`ping()` raises `MemoriaConnectionError` for network failures and `MemoriaAPIError` (or a
subclass) for HTTP error responses — callers can distinguish the two:

```python
from memoria import MemoriaAPIError, MemoriaConnectionError

try:
    client.ping()
except MemoriaConnectionError:
    print("Cannot reach the server")
except MemoriaAPIError as e:
    print(f"Server returned HTTP {e.status_code}: {e.detail}")
```

## Compatibility Matrix

| SDK version | Memoria API | Python |
|-------------|-------------|--------|
| 1.0.x       | >= 0.2.3    | >= 3.10 |

## Development

```bash
cd sdk/python
pip install -e ".[dev]"
pytest tests/unit/ -v       # unit tests (no API needed)
make python-sdk-test        # full integration tests (needs make up)
```
