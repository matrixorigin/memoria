# Memoria memory provider for Hermes

Native Hermes memory provider, using the free Memoria Cloud by default. Users do not
need to deploy a database, embedding model, extraction model or local Memoria server.
Self-hosted users can change the API origin in the same plugin.

**Development preview, version 0.1.1.** Source is published on GitHub and can be
installed directly. The [Hermes catalog submission](https://github.com/NousResearch/hermes-agent/pull/135490)
is awaiting review; installation by name becomes available after catalog acceptance.
See [CLOUD_ACCEPTANCE.md](CLOUD_ACCEPTANCE.md) for the recorded Cloud API acceptance
scope and results. Self-hosted APIs must include the matching deduplicated-capture
route before using automatic capture with explicit-write exclusions.

## Installation

Requires Hermes ≥ 0.21.5 and its Python ≥ 3.11. The tested Hermes checkout is
`0240fa4a84123406a0e5e6e7262e5b772b43f0bd` (0.21.5, September 2026).
Hermes releases sharing a version number may have different plugin APIs; run
`hermes plugins validate` on the actual target installation.

Choose one of the following installation methods, then complete the shared
[setup steps](#configure-memoria).

### 1. Install from GitHub

Install the commit verified in the October 9, 2026 local CLI acceptance:

```sh
hermes plugins install 'https://github.com/matrixorigin/memoria#plugins/hermes' \
  --ref 3a5c5065b2820d56dd3d8ac8bb259a9424b1476e \
  --yes-deps --enable
```

The `#plugins/hermes` suffix selects the plugin directory inside the Memoria
repository. `--yes-deps` authorizes preparation of its declared Python dependencies;
`--enable` selects Memoria as the memory provider. Configure your API key next.

### 2. Install from the Hermes catalog (after acceptance)

Use this method only after the catalog PR is merged and the Memoria entry is
available in your Hermes catalog. Check for it with:

```sh
hermes plugins search memoria
```

Then install the reviewed catalog version:

```sh
hermes plugins install memoria --yes-deps --enable
```

The catalog supplies the reviewed commit automatically. If Memoria is not listed,
use the GitHub installation above instead.

### 3. Install from a local checkout (development)

Copy this directory into **the active profile's** `$HERMES_HOME/plugins/memoria`.
For a default profile whose home is `~/.hermes`, from the Memoria repository:

```sh
mkdir -p ~/.hermes/plugins
cp -R plugins/hermes ~/.hermes/plugins/memoria
hermes plugins validate ~/.hermes/plugins/memoria --install-deps
hermes plugins enable memoria
```

Do not overwrite an existing `plugins/memoria` directory without checking its contents.

### Configure Memoria

After installing with any of the methods above:

```sh
hermes memory setup memoria
hermes memory status
```

Log in at [Memoria](https://thememoria.ai), create/copy a memory-service API Key,
and paste it into setup. The website login session token
is not the API Key. Hermes stores `MEMORIA_API_KEY` in the active profile's secret
store; never put it in `memoria.json` or in a shell command.

Setup also asks whether to upload completed user/assistant turns for background
fact extraction. This is **off until opted in**. Auto recall and explicit memory
tools work without auto capture. Restart the current Hermes conversation after setup.

For an existing named profile, use `-p <profile-name>` on every Hermes command.
For example, install and configure the plugin in `memoria-acceptance`:

```sh
hermes -p memoria-acceptance plugins install 'https://github.com/matrixorigin/memoria#plugins/hermes' \
  --ref 3a5c5065b2820d56dd3d8ac8bb259a9424b1476e \
  --yes-deps --enable
hermes -p memoria-acceptance memory setup memoria
hermes -p memoria-acceptance memory status
```

For a local-copy installation into a named profile, also substitute its actual
home for every `~/.hermes` path above. Selecting the general plugin is not a
substitute for configuring the memory provider and its API key.

## What this preview implements

| Capability | Behavior |
| --- | --- |
| Recall | Scoped semantic retrieve, exact query/session cache, bounded context, background prewarming |
| Explicit tools | `memoria_search`, `memoria_store`, `memoria_update`, `memoria_forget`, `memoria_profile`, `memoria_feedback` |
| Capture | Only the completed user/assistant pair; no whole-history upload, tool results or media |
| Local outbox | SQLite pending records and receipts survive restarts; duplicate callbacks are suppressed |
| Session changes | New writes bind the new session; already queued events retain their original session |
| Write approval | External mutations/capture pause while Hermes `memory.write_approval` is enabled |
| Deployment | Default free Cloud, optional self-hosted API origin |

The preview supports local CLI/desktop contexts. Gateway platforms are disabled
at runtime, including read tools, until per-turn author mapping has been validated.
Non-primary contexts (subagent, cron, flush) do not automatically capture or run
mutation tools. Bot turns are not automatically captured.

Profile identity is a hash of the resolved profile home, stable across sessions and
working directories. Moving/renaming a profile directory creates a new logical subject.
Copying a profile does not replay the old subject's pending uploads. Requests explicitly
bind `subject_id`, session and branch; model tool arguments cannot override them.
Reads verify the returned subject. Update/delete/feedback verify the exact target
record before mutating it. Profile viewing lists at most 50 profile records and
accepts `cursor`/`limit` (1–50), and returns a continuation cursor. Over-budget
pages retain complete items and a cursor; a single oversized item retains its ID
and a marked content excerpt instead of dropping the whole result.

`subject_id` is a logical filter within the authenticated account's server-side scope,
not an authorization boundary. Separate API keys under one account may share that
scope. Use separate authorized scopes/accounts for stronger isolation. This preview
does not make preexisting unscoped memories visible automatically.

## Configuration

Non-secret settings live in `$HERMES_HOME/memoria.json`:

```json
{
  "api_url": "https://api.thememoria.ai",
  "branch": "main",
  "auto_recall": true,
  "auto_capture": false,
  "top_k": 5,
  "context_chars": 6000,
  "recall_timeout": 2.0,
  "request_timeout": 15.0,
  "max_capture_chars": 24000,
  "queue_capacity": 1000
}
```

Change `api_url` to an HTTPS origin for self-hosting, or HTTP on loopback for local
development, e.g. `http://localhost:8100`. Do not append `/v1`. Restart after edits.
All requests explicitly use `branch` without changing the account's shared checkout.
Feedback is limited to `main` because its API currently lacks a branch parameter.
Timeouts are HTTP IO timeouts; Hermes also bounds the external prefetch wait.
Cold-cache prefetch intentionally performs a synchronous query inside that host
worker. Defaults are 2 seconds for plugin IO versus 8 seconds for the tested host
wait; keep the plugin timeout below any custom host budget. IO timeouts are not
total wall-clock deadlines: a stalled/slowly streaming request can outlive the
host wait, and the host suppresses overlapping prefetch until it returns.

Recalled content is untrusted data, JSON encoded and bounded by `context_chars`.
Explicit tool arguments/results are bounded too.
Search results exceeding the 30,000-character tool budget are bounded per hit,
retaining each ID and a marked content excerpt when needed, so an oversized
leading record cannot hide later matches. Within-budget responses keep full records.
Hermes performs its provider
egress secret redaction before invoking the plugin. Auto capture strips this
provider's recalled-context wrapper and skips compaction summary messages. It
does not upload images, tool calls, tool results or system messages. Oversized
turns are skipped and reported, rather than silently truncated.
The tested Hermes host wraps provider output as “authoritative reference data”.
That outer guidance conflicts with this plugin's inner untrusted-data instruction;
the plugin cannot change the host wrapper and does not promise injection immunity.

## Capture failure recovery

### Explicit writes and automatic capture

When the completed turn includes successful `memoria_store` or `memoria_update`
tool results, capture forwards only their memory IDs as `exclude_memory_ids` to
`POST /v1/observe/deduplicated`. Tool output text is not uploaded. The server
resolves those IDs within the authenticated account, branch and subject, and tells
the extraction LLM to omit already-saved facts, including translations and
paraphrases, while retaining other new facts from the same turn. Exact repeated
content is also filtered in code, including matches after sensitivity redaction.
Skipped exact duplicates are omitted from the observe response's `memories` list
rather than returning an unpersisted candidate ID. Semantic exclusions depend on
the extraction model following the prompt; this is not a general exactly-once guarantee.
Excluded IDs also protect their original records during vector deduplication:
if the nearest record is excluded and its final content differs, capture inserts the candidate without
superseding that record or searching for another record to supersede. This keeps
distinct new facts, but can retain a duplicate if the model emits a paraphrase
despite the exclusion prompt. Successful store/update tools always return compact
receipts with the memory ID, subject and validated memory type (when provided),
without echoing content or metadata. These stay below the tested host's preview
size and per-result budget. If aggregate budget enforcement still persists a
receipt, capture accepts its complete JSON in the tested Hermes
`<persisted-output>` preview. Truncated previews are ignored, and capture never
opens the referenced file. Current-turn call correlation and subject checks
still apply to these receipts.
Inactive records still supply exclusion content after correction or deletion,
including when capture was queued before that change. Missing or out-of-scope
IDs are ignored without exposing their content or rejecting the whole turn.

This requires the matching Memoria API update: **deploy the server first, then
update this plugin**. A 404 without the new route's response marker produces
`capture_dedup_endpoint_unavailable`; check the deployed API version and proxy
routing, then manually retry the failed queue record. A marked business 404
(for example, a deleted branch) remains `not_found`. The plugin never retries
that turn against the old
plain observe route. With exclusions, a missing/failing LLM is an error rather
than a fallback to raw-message storage. Ordinary turns without successful explicit
writes continue to use `/v1/observe` with its existing behavior.
The old route intentionally also accepts exclusions, but retains its original
500 mapping for service errors. The dedicated route returns typed business errors
and marks handler responses with `X-Memoria-Observe-Deduplicated: 1`.
Extraction failures before any persistence return 503 with the additional
`X-Memoria-Observe-Error: extraction_unavailable` marker. Only this marked failure
is safe for automatic replay; other 5xx responses can have unknown write outcomes.

The host must include tool calls and results in its completed-turn `messages`
snapshot in OpenAI `tool_calls`/`tool` format (as the tested Hermes build does).
Anthropic `tool_result` blocks and synthetic user messages inserted within a turn
are not supported for exclusion detection. With no such transcript, the plugin
cannot infer whether an explicit write occurred. Earlier-turn results do not
exclude facts in later turns. Existing duplicates and old queued payloads are not
rewritten or deleted by this update.

State lives in `$HERMES_HOME/plugin-data/memoria/outbox.sqlite3`, outside the plugin
install directory, with private file/directory permissions. Pending/failed/uncertain
records contain the submitted text. Completed records retain only a fingerprint,
state and timestamps, not the text. Receipts persist to suppress historical replay;
back up and remove this state deliberately when removing a profile.

Connection establishment failures, HTTP 429 and marked pre-write extraction
failures stay `pending` with persisted
exponential backoff (1, 2, 4… seconds, capped at 300 seconds), without an attempt
limit. `Retry-After` can extend that delay, bounded to 24 hours. Due tasks resume
after restart. Authentication failures and other definite HTTP rejections
remain `failed` for inspection. Timeouts after sending, other HTTP 5xx, malformed success
responses and interrupted inflight submissions become `uncertain` and are **never
automatically replayed**. A crashed inflight submission is marked uncertain after
its 120-second lease expires. Unsent pending records resume with the same binding
when auto capture is enabled and approval is off.

Bindings include API origin, a credential hash, profile subject and branch. Rotating
the API Key, moving the profile or changing branch leaves old pending records dormant;
they are not sent with a different binding. Diagnostics show all bindings. A restored
old binding can resume its pending records. No API Key is persisted in this database.
Startup reports `other_bindings_need_attention` for unfinished work belonging to
other bindings. Capacity counts only `pending`/`inflight` rows in the current
binding; failed, uncertain and old-binding records require separate disk/state
management but cannot exhaust another binding's automatic queue allowance.

Transient SQLite contention backs off without terminating the capture worker.
After a remote response, an in-memory completion is retried against SQLite without
resending the request. Completion requires the matching claim token and inflight
state, so a late response cannot overwrite manual discard/retry or a later claim.
During shutdown, a completion still blocked by SQLite may remain inflight and
become uncertain after lease expiry. Upgrade with old plugin processes stopped;
the schema is migrated transactionally, including safe legacy connection/429 failures.

Inspect the queue (prints IDs/status, never captured text):

```sh
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes
```

After inspecting the Cloud memory store, explicitly requeue an event or discard it:

```sh
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes --retry <EVENT_ID> --acknowledge-duplicate-risk
python3 ~/.hermes/plugins/memoria/diagnostics.py --home ~/.hermes --discard <EVENT_ID>
```

The duplicate-risk flag is required for uncertain/unknown outcomes; definite
connection failures and rejected requests can be requeued without it. Retrying a
pending row resets its schedule. Retries can duplicate an unknown committed write.
This API accepts
`source_event_ids` on observe but does not currently persist event idempotency; the
plugin therefore **does not claim exactly-once capture**. When the current binding's
automatic queue is full, new captures are skipped with `capture_queue_full`.
Local callbacks and duplicate receipts use session/pair/history hashes;
when the host supplies no history, identical pairs in one session are treated as a
replay. Explicit stores remain available for genuinely repeated identical events.

`observe_raw_fallback` means the server reports storing raw messages because no
extraction LLM is configured. An extraction failure may also fall back to raw messages
without a separate API warning in the current backend. This is not a full conversation
archive. Checkpoint v2, strict compression archival, native MEMORY.md/USER.md mirroring,
session summaries and snapshot/branch-management tools are not implemented in this preview.

## Verify with your Cloud account

1. Ask Hermes to explicitly save a harmless preference with `memoria_store`.
2. Start `/new`, then search for it using `memoria_search`; verify automatic recall too.
3. Correct the exact returned ID with `memoria_update` and note its new ID.
4. Read the profile, send `useful` feedback if appropriate, then delete the test ID.
5. If auto capture is enabled, complete one turn and inspect the outbox for `done`.

Use a normal user API Key. These live checks create/delete test data and are
separate from the offline unit suite. The October 8 acceptance passed 13 checks
against the Cloud API through the real Hermes MemoryManager, including automatic
capture and two-profile isolation. No production admin/master key is needed.
The website Hermes onboarding should be exposed only after the release commit
and Git installation command are verified.

An opt-in repeatable acceptance script reads the key from a local file (either
`MEMORIA_API_KEY=...` dotenv syntax or one bare `sk-...` line), creates a disposable
subject, and cleans up its active test memories:

```sh
HERMES_SOURCE=/tmp/hermes-contract /tmp/memoria-hermes-tests/bin/python plugins/hermes/tests/cloud_smoke.py --key-file <LOCAL_KEY_FILE> --report /tmp/memoria-cloud-acceptance.json
```

The script does not persist or print the key and never deletes account-wide data.
It exercises lifecycle/tool hooks directly; a complete natural-language agent
conversation and gateway acceptance are separate checks.

## Development

Tests use a real, pinned Hermes checkout and HTTPX's in-process HTTP transport.
They exercise the actual MemoryManager/provider/secret-scope contracts; they do
not require an LLM or contact Cloud.

```sh
git clone https://github.com/NousResearch/hermes-agent /tmp/hermes-contract
git -C /tmp/hermes-contract checkout 0240fa4a84123406a0e5e6e7262e5b772b43f0bd
python3 -m venv /tmp/memoria-hermes-tests
/tmp/memoria-hermes-tests/bin/pip install 'httpx>=0.27,<1' pytest 'ruff==0.16.10' ruamel.yaml python-dotenv rich packaging
HERMES_SOURCE=/tmp/hermes-contract /tmp/memoria-hermes-tests/bin/python -m pytest plugins/hermes/tests -q
/tmp/memoria-hermes-tests/bin/ruff check plugins/hermes
/tmp/memoria-hermes-tests/bin/ruff format --check plugins/hermes
hermes plugins validate plugins/hermes --install-deps
```

The plugin intentionally uses the REST adapter directly, so distribution does not
depend on releasing new subject/branch arguments in `memoria-client`. Equivalent
SDK additions can be made separately without blocking this preview.

See [RELEASE.md](RELEASE.md) for the catalog submission process.
See [REVIEW_FIXES.md](REVIEW_FIXES.md) for the concurrency/retry review follow-up.
