#!/usr/bin/env bash
# Agent-layer evals: real agents (Claude Code headless, optionally Codex)
# against this server over stdio and HTTP. Local and manual only, never CI.
# Claude usage comes only from the claude.ai subscription login; see
# docs/LOCAL_TESTING.md, "Automated agent evals".
#
#   scripts/agent-eval.sh --list
#   scripts/agent-eval.sh --case first-prompt --transport stdio
#   scripts/agent-eval.sh --dry-run          # guard pre-flight only
#   scripts/agent-eval.sh --test             # offline unit tests, no model
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${1:-}" == "--test" ]]; then
  exec python3 -I -m unittest discover -s "$root/evals/agent" -t "$root/evals/agent"
fi
if [[ ! -x "$root/target/release/pscat-mcp-server" ]]; then
  echo "building the release server (cargo build --release)" >&2
  (cd "$root" && cargo build --release --locked)
fi
exec python3 -I "$root/evals/agent/run.py" "$@"
