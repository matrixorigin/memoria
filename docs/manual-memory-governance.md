# Operator-controlled memory lifecycle

These additive SQL-backed endpoints do not change existing delete, purge, correction,
or MCP contracts. They require the existing memory read/write authorization scopes
and always resolve the storage owner from the authenticated request.

| Endpoint | Effect |
| --- | --- |
| `POST /v1/memories/:id/disable` | Deactivate one active memory and retain an operator-disable marker. |
| `POST /v1/memories/:id/restore` | Restore only an operator-disabled, non-superseded memory. |
| `POST /v1/memories/:id/erase` | Physically delete one active or disabled, non-superseded row. |
| `GET /v1/memories/disabled` | List disabled records with keyset pagination. |

Mutation bodies accept optional `branch` and `subject_id`. Disable additionally
accepts a `reason` of at most 512 characters. An incorrect subject returns 404.
Supply an explicit branch to keep a client read/modify sequence pinned to the
same branch; when omitted, the existing active-branch resolution applies.

Listing accepts `branch`, `subject_id`, `limit` (1–500, default 100), and `cursor`.
Its response is `{ "items": [...], "next_cursor": null }`; pass the opaque
`next_cursor` unchanged with the same filters to continue. Ordering is descending
`updated_at`, then descending `memory_id`. Concurrent restores/disables can change
the listing; pagination is not a database snapshot.

The reserved `_memoria_disabled` metadata key cannot be supplied by clients or used
as a metadata filter. Responses strip reserved internal keys. Disabled-list records
instead expose `disabled_at` and an optional `disabled_reason`. Stale-inactive-row
cleanup excludes operator-disabled records so that cleanup cannot silently remove
the ability to restore them.

Lifecycle writes compare the previously read owner and revision timestamp in SQL.
Erase also checks the supersession link in the DELETE itself, so a correction
completed after the erase pre-read prevents deletion of its history row.
Conflicting states or concurrent changes return HTTP 409; clients must refetch,
not automatically replay mutations. Python sync/async SDK methods do not mark
these POSTs as idempotent retries.

Only main-table mutations update shared entity graph state and main-memory stats.
Mutations on a cloned branch must not deactivate main graph nodes or enqueue main
entity extraction jobs.

## Deliberate limits and review decisions

- Erase removes only the selected row. It is **not** a complete erasure across
  snapshots, backups, cloned branches, historical revisions, or idempotency receipts.
  A missing history target can therefore remain referenced until existing cleanup
  runs. Keep this API draft until the required deletion scope is agreed.
- Existing service edit logs are retained, but this is not a transactional external
  audit outbox. Applications requiring durable operation outcomes must maintain
  their own operation identity and reconciliation protocol.
- Extraction-side rejection matching belongs to the extraction application; this
  service exposes the complete paginated disabled set, not an extraction policy.
- No new table, column, schema offset, or database migration is introduced.

Real MatrixOne regressions are in `memoria-api/tests/manual_governance.rs`. Unit
tests and compiling these tests do not replace running them against MatrixOne.
