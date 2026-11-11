#!/usr/bin/env bash
# Server-layer evals (evals/server/) against a container image: the same
# scenario files that run over stdio and HTTP in `cargo test`, run against
# the image started hardened (read-only root filesystem, no capabilities,
# 1 GiB / 1 CPU / 64 PIDs), one container per server configuration.
#
# Usage: scripts/container-eval.sh
#   IMAGE=pscat-mcp:smoke   image to test
#   SKIP_BUILD=1            reuse an existing image (otherwise it is built)
#
# Needs: docker and a Rust toolchain. The test itself reads
# PSCAT_MCP_EVAL_IMAGE, not IMAGE, so a stray IMAGE in the environment (the
# container workflow sets one for the published name) never starts a
# container by accident.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-pscat-mcp:smoke}"

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  echo ".... building $IMAGE"
  DOCKER_BUILDKIT=1 docker build -t "$IMAGE" "$ROOT"
fi

cd "$ROOT"
PSCAT_MCP_EVAL_IMAGE="$IMAGE" exec cargo test --locked --test eval_server \
  scenarios_in_the_container_image -- --nocapture
