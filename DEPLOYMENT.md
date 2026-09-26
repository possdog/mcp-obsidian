# DEPLOYMENT.md

> This file guides AI agents deploying mcp-obsidian to a fresh **Debian 13
> (trixie) VPS**. Target setup: systemd + bare binary behind a **shared
> Caddy reverse proxy that also fronts other services on the same machine**.
> For the Docker path, read `README.md` ("Deploying with Docker")
> instead — do not duplicate that here.
>
> **Verification status:** all repo-side commands (`cargo build --release
> --locked`, `cargo test`, the `401` auth behavior of the HTTP endpoint) were
> run on the dev machine and pass. The Debian/VPS commands follow `README.md`
> and `deploy/`, plus the official docs of NodeSource and Caddy; they are
> **unverified on a live Debian 13 host**. Fix this file if reality differs.

## Preconditions

Confirm all of these before starting; do not improvise substitutes:

- Root or passwordless sudo on the VPS. Run everything from the repo root.
- An Obsidian Sync subscription and the exact remote vault name.
- A DNS A/AAAA record for the public hostname (e.g. `mcp.example.com`)
  pointing at the VPS — Caddy cannot issue a Let's Encrypt cert without it.
- Caddy may already be installed and serving other services on this machine.
  Never assume a clean slate: check (`systemctl status caddy --no-pager`,
  `command -v caddy`) before installing anything, and never replace an
  existing `/etc/caddy/Caddyfile` wholesale — other sites live in it.
- A human is reachable for the two interactive steps: `ob login` (email,
  password, MFA) and `ob sync-setup` (E2EE vault password, if encrypted).
  Never attempt to script credential entry.

## 1. Toolchain

The project needs Rust 1.85+ (edition 2024). Do not rely on the distro
`rustc`; install via rustup so the version is pinned and current:

```sh
apt-get update && apt-get install -y build-essential curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

`ob` needs Node.js 22+. Check before installing anything:

```sh
node --version 2>/dev/null || echo "no node"
```

If that prints a version >= 22, skip to the next step. Otherwise install
Node 22 from NodeSource (Debian's own `nodejs` package is typically older):

```sh
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt-get install -y nodejs
```

## 2. Service user, `ob` CLI, vault binding

```sh
npm install -g obsidian-headless
useradd --system --create-home --home-dir /home/obsidian obsidian
mkdir -p /srv/mcp-obsidian/vault
chown obsidian:obsidian /srv/mcp-obsidian/vault
```

Hand off to the human for the interactive login (run as the service user —
the auth token must live in that user's `~/.config/obsidian-headless`, not
root's):

```sh
sudo -u obsidian ob login
sudo -u obsidian ob sync-setup --vault "<vault name>" --path /srv/mcp-obsidian/vault
```

Then pre-populate the vault before first server start, so the search index
builds over real content:

```sh
sudo -u obsidian ob sync --path /srv/mcp-obsidian/vault
```

> Never run desktop Obsidian Sync on this same machine — one sync method
> per device.

## 3. Build and install the server

```sh
cargo build --release --locked
install -m 0755 target/release/mcp-obsidian /usr/local/bin/
```

The first build compiles ~400 crates; allow several minutes on a small VPS.
Incremental rebuilds are seconds. Success is a clean exit with the binary at
`target/release/mcp-obsidian` (this command was verified on the dev machine).

## 4. Configure and start the unit

The env-file example already defaults to the paths used above; only
`AUTH_TOKEN` needs filling in:

```sh
mkdir /etc/mcp-obsidian
install -m 600 -o root -g root deploy/mcp-obsidian.env.example /etc/mcp-obsidian/env
sed -i "s|^AUTH_TOKEN=.*|AUTH_TOKEN=$(openssl rand -hex 32)|" /etc/mcp-obsidian/env
mkdir -p /var/lib/mcp-obsidian/index
chown -R obsidian:obsidian /var/lib/mcp-obsidian
install -m 644 deploy/mcp-obsidian.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now mcp-obsidian
```

Rules while configuring:

- **Never** print, log, or commit the token. It lives only in
  `/etc/mcp-obsidian/env` (mode 600, root-owned). If you need to reference
  it later, read it from that file, not from shell history.
- The unit runs with `ProtectSystem=strict`: everything outside
  `ReadWritePaths` is read-only. If you change `VAULT_PATH` or `INDEX_DIR`
  in the env file, update `ReadWritePaths` in the unit to match, or the
  server will fail with filesystem errors.
- Keep `BIND_ADDR=127.0.0.1:8080`. Only change it (and then also set
  `ALLOWED_HOSTS`) if there is no reverse proxy — the listener is plain
  HTTP and must not be exposed directly.
- Keep `INDEX_DIR` outside the vault. Obsidian Sync replicates dot-folders,
  so an in-vault index would sync to every device on every write. If the
  vault sits at a filesystem root, `INDEX_DIR` is mandatory (the server
  exits with "cannot resolve index directory" otherwise).

## 5. Shared Caddy reverse proxy with TLS

Caddy is the single TLS terminator for multiple services on this machine;
this deployment adds one site to it and must not disturb the others.

If Caddy is not yet installed, install it from its official apt repo (Debian
does not ship it). If it is already running, skip the install entirely:

```sh
command -v caddy || {
    apt-get install -y debian-keyring debian-archive-keyring apt-transport-https
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | tee /etc/apt/sources.list.d/caddy-stable.list
    apt-get update && apt-get install -y caddy
}
```

**Add** a site block to the existing `/etc/caddy/Caddyfile` — read the file
first, back it up (`cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.bak.$(date +%s)`),
keep every other site block untouched, and append (use the real
hostname):

```caddyfile
mcp.example.com {
    reverse_proxy 127.0.0.1:8080 {
        flush_interval -1
    }
}
```

A dedicated subdomain per service is the default layout. If this machine
instead serves everything on one hostname by path, put a path-matched
block inside that site (Caddy's `handle` blocks are mutually exclusive, so
ordering is safe):

```caddyfile
example.com {
    handle /mcp* {
        reverse_proxy 127.0.0.1:8080 {
            flush_interval -1
        }
    }
    # existing handle blocks for other services stay as they are
}
```

`flush_interval -1` is not optional polish: the MCP streamable-HTTP
transport uses long-lived server-sent-event responses, and any response
buffering breaks it. Do not strip it when merging into an existing block.

Then validate the whole file and reload. `reload` is graceful and does not
drop the other services' connections; never `restart` Caddy for a config
change:

```sh
caddy validate --config /etc/caddy/Caddyfile && systemctl reload caddy
```

If validation fails, the old config is still running — fix the Caddyfile
before reloading; do not leave the machine in a state where a future Caddy
restart would fail on a broken config.

## 6. Verify

Check the unit first:

```sh
systemctl status mcp-obsidian --no-pager
journalctl -u mcp-obsidian -n 50 --no-pager
```

Expected log lines on a healthy start: `search index built`, then
`mcp-obsidian started` (with `vault`, `index_dir`, `bind` fields). A clean
exit code 1 with `AUTH_TOKEN is required for the HTTP listener` means the
env file was not read or the token is empty — the server refuses to run
without it by design; fix the config, do not work around it.

Then probe the endpoint locally. Without a token, every POST must get `401`:

```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8080/mcp \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
# expect: 401
```

With the token, the same request returns `200` and opens an SSE stream, so
`curl` will hang — bound it with `--max-time 3`:

```sh
curl -s -o /dev/null -w '%{http_code}\n' --max-time 3 -X POST http://127.0.0.1:8080/mcp \
  -H "authorization: Bearer $(sed -n 's/^AUTH_TOKEN=//p' /etc/mcp-obsidian/env)" \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
# expect: 200
```

Finally the same no-token probe over HTTPS against the public hostname
(must also be `401`; if TLS fails, check DNS and `journalctl -u caddy`):

```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://mcp.example.com/mcp \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
```

The deployment is done when all three probes match (`401`, `200`, `401`) and
the other services on this machine still respond normally (probe one of
their URLs — the Caddy reload must have been a no-op for them). Hand the
client config (URL `https://mcp.example.com/mcp`, header
`Authorization: Bearer <token>`) to the human — do not paste the token
into chat or files outside `/etc/mcp-obsidian/env`.

## Troubleshooting

- **404 or hang behind Caddy:** response buffering is on, or
  `flush_interval -1` was dropped while merging the site block into the
  shared Caddyfile.
- **Other services broke after the Caddy edit:** the reload picked up an
  accidental edit to their site blocks. Diff against the `Caddyfile.bak.*`
  backup made in step 5 and revert the unrelated hunks.
- **`caddy validate` fails after your edit:** fix it immediately; the old
  config is still live, but the next Caddy restart would fail on a broken
  file and take down every service on the machine.
- **Unit starts, then exits with fs errors:** `ReadWritePaths` in the unit
  does not cover `VAULT_PATH`/`INDEX_DIR`, or those dirs are not owned by
  `obsidian`.
- **`ob` sync errors in `journalctl`:** re-run
  `sudo -u obsidian ob sync-status` as the service user; token and per-vault
  config are per-user, so running `ob` as root proves nothing.
- **`ob` not found by the server:** it is looked up on PATH (`OB_BIN` env to
  override); `npm -g` installs to `/usr/bin/ob` or `/usr/local/bin/ob`,
  both fine.
- If a fix required changing paths, commands, or behavior described here,
  update this file — it must not drift from reality.
