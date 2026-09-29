# Memory (Memoria) for OpenClaw

This package turns [MatrixOrigin Memoria](https://github.com/matrixorigin/Memoria) into an installable OpenClaw `memory` plugin.

## Architecture

| Backend | Transport | Binary needed | Use case |
|---------|-----------|---------------|----------|
| `embedded` | Rust `memoria` CLI → local MatrixOne DB | Yes | Self-hosted / local dev |
| `api` | Direct HTTP → Memoria REST API | No | Cloud OpenClaw, any environment |

```
api mode:
  Plugin → MemoriaHttpTransport → fetch() → Memoria REST API

embedded mode:
  Plugin → MemoriaMcpSession → spawn("memoria mcp") → local MatrixOne DB
```

- Durable memory lives in MatrixOne
- Snapshots, rollback, branches, merge, diff, governance, reflect, and entity extraction are handled by Memoria
- In `embedded` mode the plugin shells out to the `memoria` binary via MCP stdio
- In `api` mode the plugin calls the Memoria REST API directly — no binary needed
- OpenClaw keeps its own tool and hook surface; the storage backend is Memoria

## Quick Start

### Prerequisites

```bash
openclaw --version
# -> OpenClaw vX.Y.Z (CLI is installed and in PATH)
```

### Cloud Setup (Recommended)

Cloud mode connects directly to the Memoria REST API. No binary download needed.

```bash
# 1. Install plugin
openclaw plugins install @matrixorigin/thememoria

# 2. Setup cloud backend (also enables the plugin)
openclaw memoria setup \
  --mode cloud \
  --api-url <MEMORIA_API_URL> \
  --api-key <MEMORIA_API_KEY>

# 3. Verify
openclaw memoria health
# Expected: "status": "ok"
```

If the npm package is not yet published, install from source:

```bash
mkdir -p ~/.openclaw/plugins-src
git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria
openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
```

Then continue with step 2 above.

### Local Setup (Embedded)

Local mode runs the Rust `memoria` binary against a self-hosted MatrixOne instance.

```bash
# 1. Install memoria CLI
curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin

# 2. Install plugin
openclaw plugins install @matrixorigin/thememoria

# 3. Setup local backend (also enables the plugin, quick start with defaults)
openclaw memoria setup \
  --mode local \
  --install-memoria \
  --embedding-api-key <EMBEDDING_API_KEY>

# 4. Verify
openclaw memoria health
# Expected: "status": "ok"
```

For advanced local configuration:

```bash
openclaw memoria setup \
  --mode local \
  --db-url <MATRIXONE_DSN> \
  --embedding-provider <provider> \
  --embedding-model <model> \
  --embedding-api-key <embedding_key> \
  --embedding-dim <dim> \
  --install-memoria \
  --memoria-bin ~/.local/bin/memoria
```

## Agent Install Prompt

Use this prompt to have an OpenClaw agent install the plugin automatically.
Replace `<MEMORIA_API_URL>` and `<MEMORIA_API_KEY>` with your credentials.

```text
Install the Memoria memory plugin for my OpenClaw in cloud mode.

Credentials:
- API URL: <MEMORIA_API_URL>
- API Key: <MEMORIA_API_KEY>

Run these steps in order. Stop and report if any step fails.

1) Install plugin:
   openclaw plugins install @matrixorigin/thememoria

2) Setup cloud backend (this also enables the plugin):
   openclaw memoria setup --mode cloud --api-url <MEMORIA_API_URL> --api-key <MEMORIA_API_KEY>

3) Verify:
   openclaw memoria health
   Expected: "status": "ok"

Rules:
- Show every command you run and its full raw output
- Do not summarize or hide errors
- If a step fails, classify the error (network / auth / config / missing-binary) and suggest the exact fix command
- Do NOT use `openclaw memory` commands — those are built-in file memory, not Memoria
```

## Environment Variables (Local/Embedded Only)

> Cloud users: skip this section. Cloud mode only needs `--api-url` and `--api-key`.

| Variable | Description | Default |
|----------|-------------|---------|
| `MEMORIA_DB_URL` | MatrixOne DSN | `mysql://root:111@127.0.0.1:6001/memoria` |
| `MEMORIA_EMBEDDING_PROVIDER` | Embedding service | `openai` |
| `MEMORIA_EMBEDDING_MODEL` | Model name | `text-embedding-3-small` |
| `MEMORIA_EMBEDDING_API_KEY` | Required unless provider is `local` | — |
| `MEMORIA_EMBEDDING_BASE_URL` | Optional for OpenAI, required for gateways | — |
| `MEMORIA_EMBEDDING_DIM` | Must match model before first startup | — |
| `MEMORIA_LLM_API_KEY` | Optional, for `autoObserve` and LLM-backed tools | — |
| `MEMORIA_LLM_BASE_URL` | Optional OpenAI-compatible base URL | — |
| `MEMORIA_LLM_MODEL` | Model for auto-observe and reflection | `gpt-4o-mini` |
| `MEMORIA_EXECUTABLE` | Explicit path to `memoria` binary | — |
| `MEMORIA_RELEASE_TAG` | Rust Memoria release tag to install | installer default |

## Tool Surface

The plugin exposes these tools to OpenClaw:

- `memory_search`, `memory_get`, `memory_store`, `memory_retrieve`, `memory_recall`
- `memory_list`, `memory_stats`, `memory_profile`, `memory_correct`, `memory_purge`, `memory_forget`, `memory_health`
- `memory_observe`, `memory_governance`, `memory_consolidate`, `memory_reflect`, `memory_extract_entities`, `memory_link_entities`, `memory_rebuild_index`, `memory_capabilities`
- `memory_snapshot`, `memory_snapshots`, `memory_rollback`
- `memory_branch`, `memory_branches`, `memory_checkout`, `memory_branch_delete`, `memory_merge`, `memory_diff`
- Compatibility CLI aliases under `openclaw ltm ...`

## Common Pitfalls

**`openclaw memoria` vs `openclaw memory`:** This plugin uses `openclaw memoria`. The `openclaw memory` namespace is OpenClaw's built-in file memory — a different system entirely.

**macOS `sh` vs `bash`:** The installer script uses bash syntax. If piping, use `bash -s --`, not `sh -s --`.

**Explicit memory mode is default (`autoObserve=false`):** The plugin does not auto-write memories from conversation turns. Writes happen when the agent explicitly calls tools like `memory_store`. To enable auto-capture, set `MEMORIA_AUTO_OBSERVE=true` and provide `MEMORIA_LLM_API_KEY` + `MEMORIA_LLM_MODEL`.

**Old schema vs new runtime:** If upgrading from an older Memoria setup, use a fresh database name in `MEMORIA_DB_URL` to avoid schema drift.

## Verification

| Level | Check | Command | Pass |
|---|---|---|---|
| 1. Plugin loaded | OpenClaw recognizes plugin | `openclaw plugins list` | `thememoria` listed and enabled |
| 2. Backend reachable | Memoria connectivity | `openclaw memoria health` | `status: ok` |
| 3. Memory persisted | Store → retrieve round-trip | `openclaw memoria stats` | Non-zero memory count after a write |

Additional diagnostics:

```bash
openclaw memoria capabilities   # config/plugin check (no live backend needed)
openclaw memoria verify          # deeper diagnostic
openclaw ltm list --limit 10     # list recent memories
```

Notes:
- `openclaw memoria setup` is the recommended onboarding command
- `openclaw memoria connect` remains available as the lower-level config-only variant (no `--install-memoria` support)
- `setup`/`connect` will merge `thememoria` into `plugins.allow` to satisfy OpenClaw allow-list policy
- OpenClaw may print "Restart the gateway" after `plugins install` — this is unnecessary for CLI commands like `setup` and `health`

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/plugins/openclaw/scripts/uninstall-openclaw-memoria.sh | \
  bash -s --
```

Removes the plugin entry, tool policy additions, managed skills, and the default managed checkout path.

## Compatibility Notes

- `memory_get` is resolved from recent tool results plus a bounded scan (Rust MCP has no direct get-by-id)
- `memory_stats` is derived from available MCP outputs (inactive-memory and entity totals not currently available)
- `memory_entities` is not exposed (no matching Rust MCP tool)
- Old `mysql+pymysql://...` DSNs are normalized to `mysql://...` automatically
- Schema drift from older Memoria stacks can cause runtime errors, use a fresh DB name to avoid

### Success Criteria

| Level | Check | Command | Pass |
|---|---|---|---|
| 1. Plugin loaded | OpenClaw recognizes plugin | `openclaw plugins list` | `memory-memoria` is listed and enabled |
| 2. Backend reachable | Memoria can reach configured backend | `openclaw memoria health` | returns `status: ok` |
| 3. Memory persisted | Store -> retrieve round-trip works | `openclaw memoria stats` + `openclaw ltm list --limit 10` | non-zero memory appears after a write |

Before the smoke check, confirm the CLIs you are about to use are the ones you expect:

```bash
openclaw --version
openclaw memoria --help
```

After install:

```bash
openclaw memoria capabilities
openclaw memoria stats
openclaw ltm list --limit 10
```

Notes:

- `openclaw memoria capabilities` is a config/plugin check and does not require a live Memoria backend
- `openclaw memoria stats` and `openclaw ltm list` require the configured backend to be reachable, in embedded mode that means MatrixOne must be up and the embedding config must be valid
- OpenClaw reserves `openclaw memory` for its built-in file memory, so this plugin uses `openclaw memoria` and the compatibility alias `openclaw ltm`
- `openclaw memoria setup` is the recommended onboarding command for cloud/local setup
- `openclaw memoria connect` remains available as the lower-level config-only entrypoint (no `--install-memoria` support)
- `setup/connect` will merge `memory-memoria` into `plugins.allow` to satisfy OpenClaw allow-list policy
- on fresh install without explicit backend config, `openclaw plugins enable` logs a one-time hint with cloud (recommended), local (optional), and `openclaw memoria setup --help`
- OpenClaw may print "Restart the gateway" after `plugins install` or `plugins enable`, this is unnecessary for CLI commands like `setup` and `health`, but harmless if you do restart
- `openclaw memoria install` is optional local bootstrap/repair (runtime + config rewrite)
- `openclaw memoria health` is the primary quick connectivity check
- Some released OpenClaw builds may not expose `openclaw memoria verify` yet even if the plugin source or README mentions it. If `verify` is unavailable, use `openclaw memoria health`, `openclaw memoria capabilities`, `openclaw memoria stats`, and `openclaw ltm list --limit 10` instead.

If `openclaw memoria setup` (or `connect`) is missing:

```bash
openclaw plugins update memory-memoria
openclaw plugins enable memory-memoria
openclaw memoria --help
```

If OpenClaw `status` shows memory as unavailable but the plugin appears installed, verify the plugin directly before assuming Memoria is broken:

```bash
openclaw plugins list
openclaw memoria health
openclaw memoria capabilities
openclaw ltm stats
openclaw ltm list --limit 10
```

If `openclaw memoria health` returns `status: ok` and `openclaw ltm list` returns memories, the Memoria backend is healthy. In that situation, a stale or mismatched OpenClaw gateway memory probe may be the source of the status warning rather than the Memoria plugin itself.

Low-level fallback:

```bash
node scripts/verify_plugin_install.mjs \
  --openclaw-bin "$(which openclaw)" \
  --memoria-bin "$(which memoria)"
```

## Development

- `openclaw/client.ts` talks to Rust Memoria over MCP stdio (embedded) or HTTP (api)
- Plugin manifest and config use `memoriaExecutable` for embedded mode
- Installer/uninstaller are pure shell + Node, no Python dependency
- No bundled Python runtime


## Publishing to npm

Version is managed automatically by CI. **Do not manually edit the version in `package.json`.**

The workflow queries npm for the latest published version and auto-bumps patch by default.

```bash
# Patch release (e.g. 0.4.3 → 0.4.4) — most common
gh workflow run release-npm.yml --repo matrixorigin/Memoria

# Explicit version for minor/major releases
gh workflow run release-npm.yml --repo matrixorigin/Memoria -f version=0.5.0
```

After publishing, users can upgrade with:

```bash
openclaw plugins install @matrixorigin/thememoria
```
