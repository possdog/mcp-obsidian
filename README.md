# mcp-obsidian

> 🤖 This code was written by a human-led AI agent.

An [MCP](https://modelcontextprotocol.io) server that gives AI agents read and
write access to an [Obsidian](https://obsidian.md) vault, designed to run on a
remote server (VPS) and serve any MCP-compatible client — including clients
without access to your filesystem, such as Vibe Work.

Vault sync between the server and your other devices is handled by
[Obsidian Headless](https://obsidian.md/help/headless) (the `ob` CLI), which
the server runs as a supervised child process:

```
MCP client ──HTTPS + bearer token──> reverse proxy ──> mcp-obsidian
                                                            │
                                              reads/writes  │ supervises
                                                    ▼        ▼
                                                 ~/vault   ob sync --continuous
                                                    ▲
                                                 Obsidian Sync
                                                    │
                                     your desktop / phone / tablet
```

The server reads and writes the vault directory directly, so tool calls are
fast, and writes are pushed out by `ob` within seconds.

## Tools

| Tool | Kind | Description |
|---|---|---|
| `list_notes` | read | List all files sorted by path, with size/mtime; optional prefix filter and limit |
| `read_note` | read | Full note content incl. frontmatter; optional heading-section read |
| `search_notes` | read | Full-text query (boolean/phrase) with optional path filter and limit |
| `get_backlinks` | read | Notes linking to a given note |
| `get_outlinks` | read | Links a given note points to |
| `list_tags` | read | Tag inventory with counts |
| `write_note` | write | Create or overwrite a whole note; frontmatter accepted separately |
| `patch_note` | write | append / prepend / insert-at-heading / replace-section (schema-enum `operation`) |
| `move_note` | write | Rename/move; rewrites wikilinks and markdown links |
| `delete_note` | write | Move to the vault's `.trash` (recoverable, never unlinked) |
| `daily_note` | read+write | get / create / append by `YYYY-MM-DD` date (schema-enum `action`) |
| `sync_status` | sync | Run `ob sync-status`, report result |
| `sync_now` | sync | Run a one-shot `ob sync`, report result |

The tool set is designed for AI-agent callers:

- **One path contract**: every tool that addresses an existing note accepts a
  vault-relative path or a bare note name, with the `.md` extension optional.
- **Self-correcting errors**: a missing note names the discovery tools
  (`list_notes` / `search_notes`); a missing heading lists the note's actual
  headings; an invalid enum argument names the valid variants.
- **Constrained arguments**: `patch_note`'s `operation` and `daily_note`'s
  `action` are JSON-schema enums, so clients pick from the valid values.
- **Bounded output**: tool output is capped at 256 KiB; `list_notes` and
  `search_notes` take a `limit`, and listings are path-sorted, so large vaults
  can be paged with `prefix`.

## Requirements

- An [Obsidian Sync](https://obsidian.md/sync) subscription (Headless Sync is
  a client for it).
- Node.js 22+ for the `ob` CLI.
- Rust 1.85+ to build the server (or the Docker image).

## Configuration

Every option is available as a CLI flag and as an environment variable, so
the binary runs with zero arguments under systemd or Docker.

| Env var | Flag | Default | Purpose |
|---|---|---|---|
| `VAULT_PATH` | `--vault-path` | (required) | Vault root directory |
| `INDEX_DIR` | `--index-dir` | `<vault-parent>/.mcp-obsidian-index/<vault-name>` | Search index location. Kept outside the vault: Obsidian Sync replicates dot-folders, so an in-vault index would sync to every device on every write. |
| `BIND_ADDR` | `--bind-addr` | `127.0.0.1:8080` | HTTP listen address |
| `AUTH_TOKEN` | `--auth-token` | (required) | Bearer token for MCP clients. The server refuses to start without it. |
| `OB_BIN` | `--ob-bin` | `ob` | Path to the Obsidian Headless binary |
| `ALLOWED_HOSTS` | `--allowed-hosts` | unset | Comma-separated allowed `Host` header values (DNS-rebinding guard). See [Security](#security). |
| `DAILY_NOTES_DIR` | `--daily-notes-dir` | auto-detect | Daily notes folder (falls back to `.obsidian/daily-notes.json`, then `Daily Notes` / `Daily` / `Journal`) |
| `DAILY_NOTE_FORMAT` | `--daily-note-format` | `YYYY-MM-DD.md` | Daily note filename format |
| `RUST_LOG` | — | `info` | Log filter (`tracing` EnvFilter syntax) |

## Deploying on a VPS (systemd)

For a step-by-step runbook (Debian 13, agent-oriented, with verification
probes), see [DEPLOYMENT.md](DEPLOYMENT.md). Obsidian Headless login and vault setup happen **out-of-band** — the server
never sees your Obsidian credentials, only `ob` does.

```sh
# 1. Install the headless CLI
npm install -g obsidian-headless

# 2. Authenticate (interactive; prompts for email, password, MFA)
ob login

# 3. Create a dedicated user; `ob` keeps its auth token in this user's home
sudo useradd --create-home --system obsidian

# 4. Bind your remote vault to a local directory
sudo mkdir -p /srv/mcp-obsidian/vault
sudo -u obsidian ob sync-setup --vault "My Vault" --path /srv/mcp-obsidian/vault
#    (prompts for the E2EE password if the vault is end-to-end encrypted)

# 5. Build and install the server
cargo build --release --locked
sudo install -m 0755 target/release/mcp-obsidian /usr/local/bin/

# 6. Install the systemd unit and environment file
sudo mkdir /etc/mcp-obsidian
sudo cp deploy/mcp-obsidian.env.example /etc/mcp-obsidian/env
sudo chmod 600 /etc/mcp-obsidian/env
#    edit /etc/mcp-obsidian/env: set AUTH_TOKEN (openssl rand -hex 32),
#    VAULT_PATH, and paths to match; adjust ReadWritePaths in
#    deploy/mcp-obsidian.service if your paths differ
sudo cp deploy/mcp-obsidian.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now mcp-obsidian
```

When the service starts, it supervises `ob sync --continuous`, which pulls
the vault content; `sync_status` reports progress from any client. You can
also run `sudo -u obsidian ob sync --path /srv/mcp-obsidian/vault` once
beforehand to pre-populate.

> Do not run desktop Obsidian Sync and Headless Sync on the same device —
> only one sync method per machine.

## Deploying with Docker

The image bundles the server binary and the `ob` CLI; the runtime stage is
`node:22-alpine` because the supervised `ob` process needs Node 22+.

The build stage always runs natively on the build machine's own
architecture and cross-compiles with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (a rust
target plus zig as the cross C toolchain), so targeting another
architecture stays fast: the ~400-crate release build is never emulated.

### Cross-building for another architecture

From an arm64 Mac, producing a linux/amd64 image for an x86_64 server:

```sh
# One-time setup for a Homebrew Docker CLI + colima daemon:
brew install docker-buildx        # the buildx plugin is not bundled
colima start
docker run --privileged tonistiigi/binfmt --install amd64  # QEMU handlers

# Build; --load tags the result into the local daemon.
docker buildx build --platform linux/amd64 -t mcp-obsidian:amd64 --load .

# Ship to the server without a registry...
docker save mcp-obsidian:amd64 | gzip | ssh <server> 'gunzip | docker load'
# ...or push to a registry and `docker pull` on the server instead.
```

Only the runtime stage's `npm install` executes under QEMU when
cross-building (its base image is the target arch). `obsidian-headless`
is small and pure JS, so that step is cheap. A plain `docker build` with
no `--platform` still produces a native image for the build machine's
own architecture.

```sh
docker build -t mcp-obsidian .

# Prepare host directories owned by UID 1000 (the container's `node` user).
# `index` holds the search index: the image defaults INDEX_DIR to /index,
# and a volume there persists it across container recreation (the server
# fully rebuilds it at startup otherwise).
mkdir vault config index && sudo chown -R 1000:1000 vault config index

# One-off interactive setup, using the image itself as the `ob` CLI.
# Credentials land in ./config (mounted at /config) and persist.
docker run -it --rm \
  -v "$PWD/vault:/vault" -v "$PWD/config:/config" \
  --entrypoint ob mcp-obsidian login

docker run -it --rm \
  -v "$PWD/vault:/vault" -v "$PWD/config:/config" \
  --entrypoint ob mcp-obsidian sync-setup --vault "My Vault" --path /vault

docker run -d --init --name mcp-obsidian --restart unless-stopped \
  -v "$PWD/vault:/vault" -v "$PWD/config:/config" -v "$PWD/index:/index" \
  -e AUTH_TOKEN="$(openssl rand -hex 32)" \
  -p 127.0.0.1:8080:8080 \
  mcp-obsidian
```

Alternative to the interactive login: run `ob login` on any machine, copy the
resulting `auth_token` file, and pass it as `OBSIDIAN_AUTH_TOKEN` instead —
`ob` reads that environment variable in preference to its token file.

## Reverse proxy and TLS

The listener is plain HTTP and expects a TLS-terminating reverse proxy in
front. The MCP streamable-HTTP transport uses long-lived server-sent-event
responses, so disable response buffering.

Caddy (flushes streaming responses; automatic TLS via Let's Encrypt):

```caddyfile
mcp.example.com {
    reverse_proxy 127.0.0.1:8080 {
        flush_interval -1
    }
}
```

nginx:

```nginx
location /mcp {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_buffering off;      # SSE responses must not be buffered
    proxy_read_timeout 1h;    # long-lived streams
}
```

## Connecting a client

The MCP endpoint is `/mcp` and requires a bearer token:

```
URL:             https://mcp.example.com/mcp
Authorization:   Bearer <AUTH_TOKEN>
```

In Vibe Work, add a remote MCP server with that URL and token. For
configuration-file-based clients:

```json
{
  "mcpServers": {
    "obsidian": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer <AUTH_TOKEN>" }
    }
  }
}
```

## Security

- **Bearer token is mandatory.** The server refuses to start without
  `AUTH_TOKEN`; failed requests get `401` with `WWW-Authenticate: Bearer`.
  Comparison is constant-time. Generate a strong token
  (`openssl rand -hex 32`) and serve over TLS only.
- **`ALLOWED_HOSTS`** is rmcp's DNS-rebinding guard: it validates the
  `Host` header of incoming requests. It is off by default because the
  intended deployment (loopback listener behind a reverse proxy,
  token-gated) does not need it. Set it to your public hostname(s) when
  you expose the listener directly or want the extra check.
- **Path sandboxing**: every tool path is resolved against the vault root;
  traversal (`..`), absolute paths, symlink escapes, and dot-folders
  (including `.obsidian`) are rejected.
- **Atomic writes**: notes are written to a temp file and renamed, so a
  crash mid-write never leaves truncated notes for `ob` to sync.
- **Trash over hard delete**: `delete_note` moves notes to the vault's
  `.trash`; nothing is ever unlinked.
- **Output caps**: tool responses are capped at 256 KiB to protect the
  client's context window.

## Development

```sh
cargo test            # unit tests + end-to-end HTTP round trips
cargo clippy          # must be clean
cargo fmt             # run before wrapping up
```

See [PLAN.md](PLAN.md) for the full architecture and design decisions.
