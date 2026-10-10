# Changelog

## [Unreleased]

### Fixed
- Every `httpx.TransportError` now surfaces as `MemoriaConnectionError` with the original
  exception preserved as `__cause__`. `RemoteProtocolError` ("server disconnected without
  sending a response"), `LocalProtocolError` and `ProxyError` are siblings of `NetworkError`
  rather than subclasses, so they previously escaped the SDK exception hierarchy entirely.

### Changed
- **Breaking (behavioral):** non-idempotent requests (POST/PATCH) are no longer retried on
  502/503/504 or on transport errors other than `ConnectError`. None of those prove the server
  skipped the write — a gateway can return 504 while the upstream commits — so retrying without
  an idempotency key risked duplicate writes. Pass `retry_unsafe_writes=True` to either client
  to restore the previous behavior. Idempotent methods are unchanged, and `ConnectError` (where
  the request never reached the server) is still retried for every method.
  Retry eligibility is per operation rather than purely per HTTP verb: `memories.correct()`
  uses PUT but is treated as non-idempotent, because the server mints a replacement record and
  supersedes the original, so a replay either 404s or creates a second replacement.

### Added
- Sync and async `memories.query()` for exact structured filtering through the REST API,
  including scalar `extra_metadata`, subject, type, session, trust tier, branch, and pagination.
- Sync and async `memories.fulltext_search()` for pure MatrixOne full-text search with
  exact scalar `extra_metadata_filter` and fixed-field SQL pre-filters, without vector or
  graph retrieval. Session filtering is strict and the endpoint is intentionally not exposed
  by MCP.

### Fixed
- `ping()` no longer wraps `MemoriaAuthError` / `MemoriaNotFoundError` and other API errors
  into `MemoriaConnectionError`; callers can now distinguish network failures from API errors.
- `_map_error`: empty response body no longer produces duplicate status code in the error
  message (e.g. `"HTTP 404: HTTP 404"` → `"HTTP 404: Not Found"`).

## [1.0.0] - 2026-05-25

### Added
- Initial release of the Memoria Python SDK
- `MemoriaClient` (sync) and `AsyncMemoriaClient` (async) with identical interfaces
- Full memories resource: store, store_batch, retrieve, search, list, correct, correct_by_query,
  delete, purge, feedback
- observe endpoint for session memory extraction
- profile.me()
- snapshots: create, list, rollback, delete (single + bulk/prefix/date)
- branches: create, list, checkout, diff, diff_items, merge, delete, apply, pick
- governance: run, consolidate, reflect
- ping / health check
- Context-manager support (`with` / `async with`) for connection lifecycle
- Structured exception hierarchy: MemoriaAuthError, MemoriaForbiddenError,
  MemoriaNotFoundError, MemoriaUnprocessableError, MemoriaServerError, MemoriaConnectionError
- Exponential-backoff retry on 5xx and network errors (configurable max_retries)
- dataclasses response models — zero extra dependencies beyond httpx
