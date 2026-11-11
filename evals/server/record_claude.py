#!/usr/bin/env python3
"""Record what Claude Code sends to a Streamable HTTP MCP server, as a replay
fixture. Local and manual only; never run in CI.

Starts ``serve-http --capture-file`` on a free loopback port with a throwaway
token, runs ONE tiny ``claude -p`` prompt against it that makes a single
tool call, then converts the capture with capture_to_fixture.py.

Claude usage comes only from the claude.ai subscription login, through the
same billing guard as the agent evals (evals/agent/guard.py, reused, not
reimplemented): the run is refused unless the login is a claude.ai
subscription, and is stopped if any event shows per-token billing.

    python3 -I evals/server/record_claude.py --out evals/server/fixtures/claude-code.json

Inspect the fixture by hand before committing it.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(REPO / "evals" / "agent"))
sys.path.insert(0, str(HERE))

import capture_to_fixture  # noqa: E402
import guard  # noqa: E402
import run as agent_run  # noqa: E402

PROMPT = "Call the get_renderer_info tool once, then reply with just its server_version value."


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--server", type=Path, default=REPO / "target" / "release" / "pscat-mcp-server")
    p.add_argument("--model", default=agent_run.DEFAULT_MODEL)
    p.add_argument("--out", type=Path, required=True, help="fixture file to write")
    p.add_argument("--keep-capture", type=Path, help="also copy the raw capture here")
    args = p.parse_args(argv)
    if not args.server.is_file():
        sys.exit(f"server binary not found: {args.server}")
    claude = shutil.which("claude") or "claude"

    try:
        auth = agent_run.claude_preflight(claude)  # guard: parent env + claude auth status
    except guard.GuardError as e:
        print(f"billing guard: {e}", file=sys.stderr)
        return 2
    version = agent_run.tool_version(claude).split()[0]
    print(f"billing guard pre-flight ok: {auth}; claude {version}")

    secret_dir = Path(tempfile.mkdtemp(prefix="pscat-record-"))
    os.chmod(secret_dir, 0o700)
    capture = secret_dir / "capture.jsonl"
    http = agent_run.HttpServer(args.server.resolve(), extra_args=["--capture-file", str(capture)])
    workdir = Path(tempfile.mkdtemp(prefix="pscat-eval-cwd-"))
    try:
        http.start()
        config = secret_dir / "mcp.json"
        agent_run.write_private_json(config, http.mcp_config())
        argv_ = agent_run.claude_argv(claude, args.model, 2, config, None)
        sg = guard.StreamGuard(agent_run.SERVER_NAME, args.model)
        lines, rc, timed_out = agent_run.run_child(
            argv_,
            PROMPT,
            guard.child_env(os.environ),
            workdir,
            180,
            sg.feed,
            secret_dir / "claude.jsonl",
            secret_dir / "claude.stderr",
            http.token,
        )
        if timed_out:
            print("claude timed out", file=sys.stderr)
            return 1
        sg.finish()
        print(f"claude exited {rc}")
    except guard.GuardError as e:
        print(f"billing guard stopped the run: {type(e).__name__}: {e}", file=sys.stderr)
        return 2
    finally:
        agent_run.forget_claude_project(workdir)
        shutil.rmtree(workdir, ignore_errors=True)
        http.stop()
    try:
        records = [json.loads(line) for line in capture.read_text().splitlines() if line.strip()]
        if args.keep_capture:
            args.keep_capture.write_text(capture.read_text())
        meta = {
            "id": "claude-code",
            "client_name": "claude-code",
            "client_version": version,
            "obtained": "real-capture",
            "how": (
                "claude -p (one prompt, one get_renderer_info call, subscription login behind the billing "
                "guard) against serve-http --capture-file; evals/server/record_claude.py"
            ),
            "captured_on": _dt.date.today().isoformat(),
        }
        fixture = capture_to_fixture.build_fixture(records, meta, [http.token])
        args.out.write_text(json.dumps(fixture, indent=2) + "\n")
        print(f"wrote {args.out}: {len(fixture['requests'])} request(s): " + ", ".join(r["name"] for r in fixture["requests"]))
    finally:
        shutil.rmtree(secret_dir, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
