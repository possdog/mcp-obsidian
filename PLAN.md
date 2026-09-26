# mcp-obsidian — Project Plan

A Rust MCP server that exposes tools for reading and writing an Obsidian vault,
designed to run on a remote server (VPS) and connect to any MCP-compatible
client — including clients without local filesystem access (e.g. Vibe Work).
Vault sync between the server and other devices is handled by
[Obsidian Headless](https://obsidian.md/help/headless) (`ob` CLI).

## Goals

- Single static Rust binary; low memory footprint for low-spec servers.
- Full read/write access to vault notes, with safety guarantees.
- Remote deployment over streamable HTTP with authentication.
- Generic design: useful for any Obsidian vault, not tailored to a particular
  folder taxonomy or frontmatter schema.

## Non-goals (v1)

- Obsidian plugin execution, canvas file editing, or app automation.
- Reimplementing Obsidian Sync — `ob` owns all sync and encryption.
- Serving vault content over the web for human browsing.
- stdio transport (planned as a follow-up; the design keeps the handler
  transport-agnostic).

## Key external facts

- **Obsidian Headless** (`obsidian-headless` npm package, `ob` binary, Node 22+,
  open beta) syncs a vault into a local directory. Commands:
  - `ob login` (interactive; `OBSIDIAN_AUTH_TOKEN` env var for headless auth)
  - `ob sync-setup --vault <id-or-name> [--path <local>]`
  - `ob sync [--path <local>] [--continuous]`
  - `ob sync-status [--path <local>]`
  - `ob sync-config`, `ob sync-unlink`, `ob sync-list-remote/local`
  - Credentials/encryption are managed entirely by `ob`; this server never
    sees them. Login and setup happen out-of-band on the host.
  - Do not run desktop Sync and Headless Sync on the same device.
- **rmcp** (official Rust MCP SDK, Tier 1, v3.x): tokio-based; `#[tool]` macros
  generate tools and JSON schemas from Rust functions; streamable HTTP
  transport is axum-based (`StreamableHttpService`).
- **tantivy**: mmap-based full-text index (BM25, boolean/phrase queries);
  near-zero resident RAM on the read path; bounded writer memory arena;
  incremental updates via delete-by-term + re-add.

## Architecture

```
VPS
├── mcp-obsidian (single Rust binary)
│   ├── config     clap + env configuration
│   ├── vault      path-sandboxed file I/O, atomic writes, trash deletes
│   ├── markdown   frontmatter, wikilinks, tags, link graph
│   ├── search     tantivy index + notify-driven incremental re-indexing
│   ├── sync       supervisor for `ob sync --continuous`, on-demand sync tools
│   └── server     rmcp tools over streamable HTTP + bearer-token auth
└── ~/vault/       plain files, synced by `ob` (E2EE handled by Obsidian Sync)
```

The server reads and writes the vault directory directly, so tool calls have
near-zero latency. The `ob` daemon runs as a supervised child process; remote
changes it pulls produce filesystem events that the same `notify` watcher uses
for search re-indexing — one event pipeline for remote pulls and local writes.

### Modules

- **config** — clap (derive) with env-var fallbacks for every option.
- **vault** — all filesystem access goes through here. Paths are resolved
  against the vault root and rejected if they escape it. Atomic writes via
  temp-file + rename. Deletes move notes into the vault's `.trash`. The
  `.obsidian` config directory is excluded from all operations.
- **markdown** — parses frontmatter (YAML) with unknown-field passthrough;
  wikilinks (`[[note]]`, `[[note|alias]]`, `[[note#heading]]`, embeds
  `![[note]]`), body tags, frontmatter tags. Resolves wikilinks to vault
  paths. Computes the link graph for backlinks/outlinks. Link rewriting on
  `move_note` updates both wikilinks and standard markdown links.
- **search** — tantivy index stored in a configurable directory.
  Schema (fixed fields):
  - `path` (fast + stored) — vault-relative, unique doc key
  - `title` (text) — first H1 or filename
  - `aliases` (text) — from frontmatter
  - `tags` (text, indexed as individual terms) — frontmatter + body
  - `frontmatter_raw` (text) — raw YAML blob, so arbitrary per-note fields
    remain searchable even though they are not schema fields
  - `body` (text, stored) — note content
  Index writer: 1 thread, ~10 MB arena. `notify` watcher debounces events and
  re-indexes changed notes (delete-by-path-term + re-add). Schema changes
  trigger a full rebuild (admin path, not an MCP tool).
- **sync** — spawns `ob sync --continuous --path <vault>` at startup,
  restarts it with backoff on exit, logs its output. `sync_status` runs
  `ob sync-status` on demand; `sync_now` runs a one-shot `ob sync`.
- **server** — rmcp `#[tool_router]` handler; axum streamable HTTP listener
  with a bearer-token auth middleware (constant-time comparison, `AUTH_TOKEN`
  env). All tools return size-capped output.

## Configuration

| Env var | Default | Purpose |
|---|---|---|
| `VAULT_PATH` | (required) | Vault root directory |
| `INDEX_DIR` | `<vault-parent>/.mcp-obsidian-index/<vault-name>` | Tantivy index location. Kept outside the vault: Obsidian Sync replicates dot-folders, so an in-vault index would sync to every device. |
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP listen address |
| `AUTH_TOKEN` | (required for HTTP) | Bearer token for MCP clients |
| `OB_BIN` | `ob` | Path to the Obsidian Headless binary |
| `DAILY_NOTES_DIR` | auto-detect (`Daily Notes`, `Daily`, `Journal`) | Daily notes folder |
| `DAILY_NOTE_FORMAT` | `YYYY-MM-DD.md` | Daily note filename format |

## MCP tools (v1, 14 tools)

| Tool | Kind | Description |
|---|---|---|
| `list_notes` | read | Recursive listing with size/mtime; optional path prefix filter |
| `read_note` | read | Full note content incl. frontmatter; optional heading selection |
| `search_notes` | read | Tantivy query (boolean/phrase) with optional path filter and limit |
| `get_backlinks` | read | Notes linking to a given note (via link graph) |
| `get_outlinks` | read | Links a given note points to |
| `list_tags` | read | Tag inventory with counts |
| `write_note` | write | Create or overwrite; frontmatter accepted separately |
| `patch_note` | write | append / prepend / insert-at-heading / replace-section |
| `move_note` | write | Rename/move; rewrites wikilinks and markdown links |
| `delete_note` | write | Move to vault `.trash` (recoverable) |
| `daily_note` | read+write | Read/today/create/append by date |
| `sync_status` | sync | Run `ob sync-status`, report result |
| `sync_now` | sync | Run one-shot `ob sync`, report result |

## Safety model

- **Path sandboxing**: every path is resolved against the vault root;
  traversal (`..`), absolute paths, and symlink escapes are rejected.
- **Config exclusion**: the `.obsidian` directory is invisible to all tools.
- **Atomic writes**: temp file in the same directory + rename, so a crash
  mid-write never leaves truncated notes for `ob` to sync.
- **Frontmatter preservation**: patches never mangle existing frontmatter.
- **Output caps**: tool responses are size-capped to protect client context
  windows.
- **Trash over hard delete**: `delete_note` moves to `.trash` (never `rm`).

## Testing strategy

- Unit tests: markdown parsing (wikilink variants, frontmatter passthrough,
  tags), vault path sandboxing (traversal, absolute, symlink, `.obsidian`).
- Integration tests: temp vault fixture; stub `ob` shell script for the sync
  supervisor; in-process rmcp client for tool round-trips (create → read →
  patch → move → delete → search).
- `cargo clippy` and `cargo fmt` must be clean at every phase.

## Build phases

1. **Scaffold + config + vault module** — cargo project, dependency set,
   config, sandboxed vault I/O with tests.
2. **Markdown + read tools** — frontmatter/wikilink/tag parsing, link graph,
   `list_notes`, `read_note`, `get_backlinks`, `get_outlinks`, `list_tags`.
3. **Sync supervisor** — child process management, restart with backoff,
   `sync_status` / `sync_now`.
4. **HTTP transport + auth** — rmcp server over streamable HTTP, bearer
   token middleware, end-to-end smoke test.
5. **Search** — tantivy index, notify watcher, `search_notes`.
6. **Write tools** — `write_note`, `patch_note`, `move_note` (link rewrite),
   `delete_note`, `daily_note`.
7. **Deployment** — Dockerfile (multi-stage, distroless/static), systemd unit,
   README with VPS setup instructions (incl. out-of-band `ob login`).

## Deployment sketch (phase 7)

```sh
# On the VPS, once, out-of-band:
npm install -g obsidian-headless
ob login                      # or OBSIDIAN_AUTH_TOKEN
ob sync-setup --vault "My Vault" --path ~/vault

# The server supervises the sync daemon from then on:
mcp-obsidian --vault-path ~/vault --auth-token "$TOKEN"
```

Clients (Vibe Work, etc.) connect over HTTPS with the bearer token. TLS is
terminated by a reverse proxy (Caddy/nginx) in front of the HTTP listener.

## Open items / risks

- rmcp v3.x exact API surface to be verified against crates.io docs (and the
  rustdoc MCP, which works once the crates are in this project).
- Obsidian Headless is an open beta; CLI output parsing for `sync_status`
  should be tolerant to format changes (fall back to raw output).
- E2EE vaults: password is `ob`'s concern at setup time; nothing needed here.
- Vault-wide link-graph scans are O(n) per backlink query; acceptable at
  typical vault sizes (<10k notes). Revisit if large vaults complain.
