# syntax=docker/dockerfile:1

# --- Build stage: compile the release binary against musl so it runs on the
# Alpine runtime image.
FROM rust:1-alpine AS build
WORKDIR /build

# Dependency-caching layer: build the full dependency graph from the
# manifests using stub sources, so later source edits do not recompile the
# ~400 crates. The stubs are replaced by the real sources in the next layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'pub fn unused() {}' > src/lib.rs \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/deps/mcp_obsidian* target/release/mcp-obsidian

COPY src ./src
RUN cargo build --release --locked

# --- Runtime stage.
#
# The plan originally called for a distroless/static runtime, but the server
# supervises `ob` (the Node 22+ `obsidian-headless` CLI) as a child process,
# so the image must contain a Node runtime. node:22-alpine is the smallest
# image that provides both Node and the musl-linked server binary.
FROM node:22-alpine AS runtime

# `ob` is spawned by the sync supervisor at startup, not baked into the
# entrypoint, so it just needs to be on PATH.
RUN npm install --global obsidian-headless \
 && npm cache clean --force

COPY --from=build /build/target/release/mcp-obsidian /usr/local/bin/mcp-obsidian

# `ob` stores its auth token and per-vault sync config under
# $XDG_CONFIG_HOME/obsidian-headless (mode 0600). Pointing it at /config
# keeps those credentials on a mounted volume instead of inside the
# container, so login survives recreation.
ENV XDG_CONFIG_HOME=/config

# Sensible in-container defaults; AUTH_TOKEN still has to be provided.
# The listener must bind all interfaces to be reachable through the
# published port; it remains token-gated.
ENV VAULT_PATH=/vault \
    BIND_ADDR=0.0.0.0:8080

# node:22-alpine ships a non-root `node` user (UID 1000). Bind mounts on the
# host must be owned by — or writable as — that UID.
USER node
WORKDIR /vault

EXPOSE 8080

# The server handles SIGTERM itself (graceful HTTP shutdown, then the sync
# supervisor and watcher are stopped). Run with `--init` / compose `init: true`
# so the container runtime reaps any orphaned descendants.
ENTRYPOINT ["/usr/local/bin/mcp-obsidian"]
