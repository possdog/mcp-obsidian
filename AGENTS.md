# AGENTS.md

This file guides AI coding agents working in this repository.

## Project overview

An MCP server (Rust 2024 edition, Rust 1.85+) exposing an Obsidian vault to
remote MCP clients over streamable HTTP with bearer-token auth. Built on
`rmcp` 3.4 / `axum` 0.8 / `tantivy` 0.26. It supervises the Obsidian
Headless (`ob`) sync daemon as a child process and reads/writes the vault
directory directly. See `README.md` for deployment/security and `PLAN.md`
for architecture and design decisions.

## Commands

All verified from the repo root. Tests are fully self-contained — tempdir
vault fixtures and a stub `ob` shell script; no network, no real Obsidian.

```sh
cargo test                      # unit + integration; ~5s
cargo test --test http <name>   # one end-to-end HTTP test
cargo test vault::              # unit tests in one module (#[cfg(test)] at the file bottom)
cargo clippy --all-targets      # must be clean
cargo fmt                       # run before wrapping up
```

## Running locally

```sh
VAULT_PATH=/path/to/vault AUTH_TOKEN=dev-token cargo run
# health check: MCP POST to http://127.0.0.1:8080/mcp with Bearer header
```

The server refuses to start without both. Every config option exists as both
a CLI flag and an env var (`src/config.rs`, clap derive).

## Code conventions

- Unit tests live in `#[cfg(test)]` modules at the bottom of the same file;
  end-to-end tests in `tests/http.rs`, binary-level tests in
  `tests/main_binary.rs` (Unix-only: relies on exec bits and SIGTERM).
- Module errors are `thiserror` enums; production code does not `unwrap()`.
- Each module opens with a `//!` doc comment and comments explain "why",
  not just "what" — match that density.

## Safety model — never weaken these

- All vault paths go through the sandbox in `src/vault.rs`; never touch the
  vault with direct `std::fs` calls. Traversal, absolute paths, symlink
  escapes, and dot-folders (incl. `.obsidian`) are rejected there.
- Notes are written atomically (temp file + rename) so a crash can't leave
  truncated notes for sync to replicate.
- `delete_note` moves to the vault's `.trash`; nothing is ever unlinked.
- Keep the search index outside the vault (`INDEX_DIR`): Obsidian Sync
  replicates dot-folders, so an in-vault index syncs to every device.
- Tool output is capped at 256 KiB to protect client context windows.

## Boundaries

- Never commit secrets or real `AUTH_TOKEN` values.
- Leave commit/push to the user unless explicitly asked.

## Pointers

- `DEPLOYMENT.md` — step-by-step Debian 13 VPS deployment runbook (systemd +
  Caddy), written for agents; read it before any deploy task.
- `README.md` — full config table, systemd/Docker deployment, security notes.
- `PLAN.md` — architecture, safety model rationale, open items/risks.

If this file contradicts reality, fix the file.
