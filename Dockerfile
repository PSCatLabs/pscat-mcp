# syntax=docker/dockerfile:1.7
#
# pscat-mcp-server container image.
#
#   docker build -t pscat-mcp .
#   docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
#     -p 127.0.0.1:8080:8080 -e PSCAT_MCP_AUTH_TOKENS=... pscat-mcp
#
# The runtime image is distroless (no shell, no package manager) and runs as
# the non-root user 65532. The server needs no writable filesystem: every
# render runs in a child process of the same binary (`render-worker`) that
# reads its job from stdin, writes its result to stdout, and has all
# PostScript file access disabled. `--read-only` works without any tmpfs.
#
# Platform: this builds for the host architecture (arm64 on Apple Silicon).
# Cloud Run needs linux/amd64. The `Container image` GitHub Actions workflow
# builds that and publishes ghcr.io/pscatlabs/pscat-mcp. Locally, use
# `docker buildx build --platform linux/amd64 .`.
#
# Health: GET /healthz (unauthenticated) returns 200 when the server is up.
# There is no Docker HEALTHCHECK because the image has no shell or curl;
# Cloud Run / Kubernetes should probe /healthz over HTTP instead.

ARG RUST_IMAGE=rust:1.96-slim-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot

# --------------------------------------------------------------------------
# Builder
# --------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS builder
WORKDIR /app

# The slim Rust image already has gcc, libc6-dev and ca-certificates, which
# is all the dependency tree needs (no OpenSSL / pkg-config: there is no TLS
# client in the server). Cargo fetches the pscat git dependency with its
# built-in libgit2, so the git CLI is not required either.
ENV CARGO_TERM_COLOR=never \
    CARGO_NET_RETRY=5

# 1. Dependency layer: fetch every crate (including the pinned pscat git
#    revision) from the manifest and lockfile alone, so source edits do not
#    re-download anything. Stub targets satisfy the manifest's [lib]/[[bin]].
COPY Cargo.toml Cargo.lock build.rs ./
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    mkdir -p src && touch src/lib.rs src/main.rs \
    && cargo fetch --locked \
    && rm -rf src

# 2. Build. The target directory is a cache mount, so incremental rebuilds
#    reuse compiled dependencies; the binary is copied out of it because a
#    cache mount is not part of the image.
#    Everything .dockerignore lets through (manifest, build.rs, src/, tests/).
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked --bin pscat-mcp-server \
    && install -D -m 0755 target/release/pscat-mcp-server /out/pscat-mcp-server \
    && /out/pscat-mcp-server --version

# --------------------------------------------------------------------------
# Runtime
# --------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime

LABEL org.opencontainers.image.title="pscat-mcp-server" \
      org.opencontainers.image.description="MCP server that renders PostScript with the PSCat interpreter" \
      org.opencontainers.image.source="https://github.com/PSCatLabs/pscat-mcp" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

COPY --from=builder /out/pscat-mcp-server /usr/local/bin/pscat-mcp-server

# Container defaults. Cloud Run overrides PORT; everything else is set with
# `-e` / `--set-env-vars` (see .env.example). Auth tokens are NOT baked in:
# without PSCAT_MCP_AUTH_TOKENS or PSCAT_MCP_AUTH_TOKEN_FILE the server
# refuses to start on a non-loopback address.
ENV PSCAT_MCP_BIND=0.0.0.0 \
    PORT=8080 \
    PSCAT_MCP_LOG_FORMAT=json

# distroless :nonroot already sets this; stated explicitly for clarity.
USER 65532:65532
WORKDIR /
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/pscat-mcp-server"]
CMD ["serve-http"]
