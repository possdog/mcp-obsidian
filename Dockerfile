# syntax=docker/dockerfile:1

# --- Build stage: compile the release binary for the target platform.
#
# The stage runs natively on the build machine's own architecture
# (--platform=$BUILDPLATFORM) and cross-compiles to the requested
# TARGETPLATFORM with cargo-zigbuild, so building a linux/amd64 image from
# an arm64 Mac compiles natively instead of under QEMU (roughly 10x faster
# for the ~400-crate release build). zig supplies the cross C toolchain and
# musl libc headers — the dependency graph is not pure Rust (zstd-sys, via
# tantivy, builds C code), so a bare rustc target alone would not link.
#
# A plain `docker build` without --platform leaves TARGETPLATFORM equal to
# the build machine's own platform, so native builds keep working unchanged.
FROM --platform=$BUILDPLATFORM rust:1-alpine AS build
WORKDIR /build

# Toolchain layer: platform-independent, so switching --platform reuses the
# cache instead of reinstalling zig and recompiling cargo-zigbuild.
RUN apk add --no-cache zig \
 && cargo install cargo-zigbuild --locked

ARG TARGETPLATFORM
# Map the platform to a musl target triple (musl so the binary runs on the
# Alpine runtime image). ARG values cannot be computed inside a Dockerfile,
# so the triple is persisted to a file that the later layers read back.
RUN case "$TARGETPLATFORM" in \
        linux/amd64) echo x86_64-unknown-linux-musl  > /triple ;; \
        linux/arm64) echo aarch64-unknown-linux-musl > /triple ;; \
        *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac \
 && rustup target add "$(cat /triple)"

# Dependency-caching layer: build the full dependency graph from the
# manifests using stub sources, so later source edits do not recompile the
# ~400 crates. The stubs are replaced by the real sources in the next layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'pub fn unused() {}' > src/lib.rs \
 && echo 'fn main() {}' > src/main.rs \
 && cargo zigbuild --release --locked --target "$(cat /triple)" \
 && rm -rf src \
           target/$(cat /triple)/release/deps/mcp_obsidian* \
           target/$(cat /triple)/release/deps/libmcp_obsidian* \
           target/$(cat /triple)/release/mcp-obsidian

COPY src ./src
# COPY preserves the original mtimes of the real sources, which are older
# than the fingerprints recorded during the stub build; cargo would then
# wrongly consider the stubbed lib rlib fresh and link the bin against an
# empty lib. Touching the sources forces the lib and bin to rebuild.
RUN touch src/lib.rs src/main.rs \
 && cargo zigbuild --release --locked --target "$(cat /triple)" \
 && cp target/$(cat /triple)/release/mcp-obsidian /build/mcp-obsidian

# --- Runtime stage.
#
# The plan originally called for a distroless/static runtime, but the server
# supervises `ob` (the Node 22+ `obsidian-headless` CLI) as a child process,
# so the image must contain a Node runtime. node:22-alpine is the smallest
# image that provides both Node and the musl-linked server binary.
#
# FROM without --platform resolves to TARGETPLATFORM, so this base image is
# pulled for the target arch even though the build stage above ran on the
# build machine's arch. That means the `npm install` below executes through
# QEMU emulation when cross-building; register the handler once with
#   docker run --privileged tonistiigi/binfmt --install amd64
# (the `ob` package is small and pure JS, so the emulated step is cheap).
FROM node:22-alpine AS runtime

# `ob` is spawned by the sync supervisor at startup, not baked into the
# entrypoint, so it just needs to be on PATH.
RUN npm install --global obsidian-headless \
 && npm cache clean --force

# Search-index default location; owned by the runtime user so the server
# can create and write it.
RUN mkdir /index && chown node:node /index

COPY --from=build /build/mcp-obsidian /usr/local/bin/mcp-obsidian

# `ob` stores its auth token and per-vault sync config under
# $XDG_CONFIG_HOME/obsidian-headless (mode 0600). Pointing it at /config
# keeps those credentials on a mounted volume instead of inside the
# container, so login survives recreation.
ENV XDG_CONFIG_HOME=/config

# Sensible in-container defaults; AUTH_TOKEN still has to be provided.
# - The listener must bind all interfaces to be reachable through the
#   published port; it remains token-gated.
# - The vault is mounted at /vault, so the index-dir default (a sibling of
#   the vault) would land on the unwritable container root. /index is
#   pre-created below; mount a volume there to persist the index across
#   container recreation (the server rebuilds it at startup otherwise).
ENV VAULT_PATH=/vault \
    INDEX_DIR=/index \
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
