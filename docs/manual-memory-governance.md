# Operator-controlled memory lifecycle

These SQL-backed endpoints leave existing delete, purge and MCP APIs in place.
Correction now rejects a stale/inactive pre-read rather than publishing a replacement
after a concurrent disable. They require the existing memory read/write authorization scopes
and always resolve the storage owner from the authenticated request.

| Endpoint | Effect |
| --- | --- |
| `POST /v1/memories/:id/disable` | Deactivate one active memory and retain an operator-disable marker. |
| `POST /v1/memories/:id/restore` | Restore only an operator-disabled, non-superseded memory. |
| `POST /v1/memories/:id/erase` | Remove one non-superseded row's content. An idempotent row retains a content-free receipt; other rows are physically deleted. |
| `GET /v1/memories/disabled` | List disabled records with keyset pagination. |

All mutations require `Content-Type: application/json` and a JSON object (`{}` is
valid). Bodies accept optional `branch` and `subject_id`. Disable additionally
accepts a `reason` of at most 512 characters; restore and erase reject `reason`
instead of silently ignoring it. An incorrect subject returns 404. An explicitly
blank/whitespace-only subject returns 422, including on the disabled listing; it
never means an unfenced operation.
Supply an explicit branch to keep a client read/modify sequence pinned to the
same branch; when omitted, the existing active-branch resolution applies.
The write target is resolved and pinned once. Multi-member group keys cannot
mutate shared main through these endpoints, regardless of their current checkout;
they may explicitly target a non-main branch without changing checkout. The
existing solo-owner exception is unchanged. Selective apply skips operator-disabled
records and erasure receipts in either table (adds, removes, updates and conflicts);
it is not an alternative restore/erase endpoint.

Listing accepts `branch`, `subject_id`, `limit` (1–500, default 100), and `cursor`.
Its response is `{ "items": [...], "next_cursor": null }`; pass the opaque
`next_cursor` unchanged with the same filters to continue. Ordering is descending
`updated_at`, then descending `memory_id`. Concurrent restores/disables can change
the listing; pagination is not a database snapshot.

The reserved `_memoria_disabled` metadata key cannot be supplied by clients or used
as a metadata filter. Responses strip reserved internal keys. Disabled-list records
instead expose `disabled_at` and an optional `disabled_reason`. Stale-inactive-row
cleanup and tool-result TTL cleanup exclude operator-disabled records so they cannot remove
the ability to restore them.
Hygiene's cleanable inactive count excludes those records and erasure receipts.

Lifecycle writes compare the previously read owner and revision timestamp in SQL.
Erase also checks the supersession link in the SQL write itself, so a correction
completed after the erase pre-read prevents deletion of its history row.
Conflicting states or concurrent changes return HTTP 409; clients must refetch,
not automatically replay mutations. Python sync/async SDK methods do not mark
these POSTs as idempotent retries.

Only main-table mutations update shared entity graph state and main-memory stats.
Mutations on a cloned branch must not deactivate main graph nodes or enqueue main
entity extraction jobs.
Restoring main reactivates its existing, non-superseded graph nodes only while the
backing main memory is still active. Disable and
restore return the persisted revision timestamp to service callers.

For an idempotent row, erase clears its content, embedding, business metadata,
source events, session, author and subject. The inactive row retains its owner/ID
and ordinary system columns (type, tier, confidence, counters and timestamps);
its metadata contains only the request hash and internal erasure marker.
Replaying that key returns 409, including
after stale/TTL cleanup. Disabled-key replays also return a typed 409. New keys
still represent new writes: this is not a content-wide rejection policy.

## Deliberate limits and review decisions

- Erase accepts active, operator-disabled and ordinary soft-deleted rows, but not
  superseded revisions or already-erased receipts. It affects only the selected
  current-branch row. It is **not** a complete erasure across snapshots, backups,
  cloned branches or historical revisions. An older revision can retain its full
  text and remain readable through history; existing main-table broken-chain cleanup
  does not clean cloned-branch history. An idempotency tombstone can also keep a
  history chain linked. Do not use `/erase` to promise a complete compliance deletion.
- Edit-log payloads can retain full original content (for example `inject`). Shared
  graph nodes are deactivated, not content/embedding-redacted; snapshots, backups and
  older revisions are not redacted. Keep this API draft until the required deletion
  scope across these stores is agreed.
- Restore retains the original `observed_at`, confidence and trust tier. It does not
  reset age or exempt an active memory from normal quarantine, working-memory archive
  or tool-result TTL. A restored old/low-confidence row may therefore be removed by
  the next governance run; restore is not a fresh retention period.
- Existing service edit logs are retained, but this is not a transactional external
  audit outbox. Applications requiring durable operation outcomes must maintain
  their own operation identity and reconciliation protocol.
- Extraction-side rejection matching belongs to the extraction application; this
  service exposes the complete paginated disabled set, not an extraction policy.
- No new table, column, schema offset, or database migration is introduced.
- Other pre-existing writes using the active-branch middleware shortcut and
  non-correction supersede paths still need a separate concurrency/authorization
  audit. The new handler target check is not a claim that every historical write
  route was hardened in this PR.

Real MatrixOne regressions are in `memoria-api/tests/manual_governance.rs`. Unit
tests and compiling these tests do not replace running them against MatrixOne.
