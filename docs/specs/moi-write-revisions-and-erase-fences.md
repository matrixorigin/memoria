# Conditional source revisions and suspended writes

This local integration adds two independent concurrency contracts. Neither
changes the soft-delete API into physical erasure.

## Source correction and metadata updates

`GET /v1/memories/{id}?branch=main` returns the persisted `updated_at` and an
opaque `revision`. Clients must forward both rather than reconstruct a hash.
The revision includes content, metadata, confidence, identity and active state,
but not retrieval scores or counters. It closes the gap left by older writers
using second-precision `NOW()`. A store acknowledgement constructed before a
database read may omit the revision; read the persisted memory before mutation.

`PUT /v1/memories/{id}/correct-attributed` requires `subject_id`, the new
`session_id`, `new_content`, `extra_metadata_patch`, `expected_updated_at` and
`expected_revision`. Optional `initial_confidence` must be finite and in [0, 1].
Omission uses the new curated revision's tier default, not the previous body's
assessment. The caller supplies new-source provenance or explicit null removals.
Normal manual corrections retain their existing inheritance semantics.

`PUT /v1/memories/{id}/metadata` operates on main, requires `subject_id`, the
current `session_id`, both expected version fields, and nonempty `extra_metadata`.
It cannot accept a content field. The patch merges top-level keys; null removes
a key. Reserved internal keys are rejected. Row locking, version comparison,
merge and the returned row are one transaction. Conflict is HTTP 409, a wrong
subject/session or absent target is 404. An unsupported old endpoint is an error,
not permission to fall back to a full/content rewrite.

## Suspended conflict writes

`POST /v1/memories/write-fence` captures generations for a nonempty `subject_id`,
`connector_id`, and `workflow_id` inside the authenticated scope. The returned
object must be retained with the suspended candidate. It is an internal write
precondition, not an authorization credential.

`POST /v1/memories/fenced` accepts `{ "memory": <StoreRequest>, "fence": <captured object> }`.
The memory requires an explicit branch and idempotency key; subject and metadata
connector/workflow must match the fence. Every write, including receipt replay,
checks all four generation rows under transaction locks. Stale/erasing ranges
return HTTP 409 with `code=memory_write_fence_stale`. There is no unfenced retry.
The existing API-key write scope and group-main write restrictions still apply.

Physical erasure changes the selected generation before deleting, and keeps the
range closed until successful completion. Locks order already-issued fenced
inserts before the erase; the independent write task is not abandoned when its
HTTP caller leaves. A failed erase retains its range closure; retrying the erase
can complete it. The final unlock includes this operation's generation so an old
operation cannot unlock a new one. Other ranges remain usable.

User schema version 3 adds `mem_write_fences`. Rows contain a hashed range key,
scope owner, random generation and closure state, never memory bodies. They are
not removed with memories; resetting them would authorize old requests again.
`user_id` preserves the existing user-scoped database migration contract.

Drain old unfenced review calls before enabling the upgraded pair. This contract
cannot retrospectively fence requests sent by an older MOI process without a
precondition. MOI conflict snapshots and audit content remain outside Memoria's
erase contract; invalidating a suspended write does not delete that history.
