#!/usr/bin/env python3
"""Turn a capture file (the server's --capture-file, one JSON line per /mcp
request) into a replay fixture for tests/eval_replay.rs.

    python3 -I evals/server/capture_to_fixture.py CAPTURE.jsonl \\
        --id claude-code --client-name claude-code --client-version 2.1.322 \\
        --obtained real-capture --how "claude -p against serve-http --capture-file" \\
        --secret "$TOKEN" -o evals/server/fixtures/claude-code.json

The capture never holds a credential (the server records only the scheme of
Authorization), and this tool refuses to write a fixture that contains any
``--secret`` value anyway. Python standard library only.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import re
import sys
from pathlib import Path

# Headers a replay must not send as recorded: the Host belongs to the
# recording server, and sessions are not portable.
DROP_HEADERS = {"host", "mcp-session-id", "content-length"}


TOOL_USE_ID = re.compile(r"toolu_[A-Za-z0-9]+")


def scrub(value):
    """Claude Code puts the model's tool-use id in `_meta`; it is per-request
    noise, not part of the protocol, so it is replaced."""
    if isinstance(value, str):
        return TOOL_USE_ID.sub("toolu_scrubbed", value)
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if isinstance(value, dict):
        return {k: scrub(v) for k, v in value.items()}
    return value


def request_name(rec: dict, body) -> str:
    if isinstance(body, dict) and "method" in body:
        name = body["method"]
        if name == "tools/call":
            name += " " + str((body.get("params") or {}).get("name", ""))
        return name.strip()
    return f"{rec.get('method', 'POST')} {rec.get('path', '/mcp')}"


def convert_record(rec: dict) -> dict:
    method = rec.get("method", "POST")
    raw = rec.get("body") or ""
    try:
        body = json.loads(raw) if raw else None
    except ValueError:
        body = raw
    headers = {k: v for k, v in (rec.get("headers") or {}).items() if k not in DROP_HEADERS}
    out: dict = {"name": request_name(rec, body), "method": method, "path": rec.get("path", "/mcp"), "headers": headers}
    auth = rec.get("authorization")
    if isinstance(auth, dict) and auth.get("scheme"):
        out["authorization"] = {"scheme": auth["scheme"]}
    if body is not None:
        out["body"] = scrub(body)
    resp = rec.get("response") or {}
    out["observed"] = {"status": resp.get("status"), "content_type": resp.get("content_type")}
    if method != "POST" and resp.get("status") is not None:
        # A GET (SSE stream) or DELETE (session close): pin what the server
        # answered when the client sent it.
        out["expect"] = {"status_in": [resp["status"]]}
    if rec.get("body_truncated"):
        raise SystemExit(f"{out['name']}: the capture cut this body; record again with a smaller request")
    return out


def build_fixture(records: list[dict], meta: dict, secrets: list[str]) -> dict:
    fixture = {
        "id": meta["id"],
        "client": {"name": meta["client_name"], "version": meta.get("client_version")},
        "obtained": meta["obtained"],
        "how": meta["how"],
        "captured_on": meta["captured_on"],
        "requests": [convert_record(r) for r in records],
    }
    if meta.get("note"):
        fixture["note"] = meta["note"]
    text = json.dumps(fixture)
    for s in secrets:
        if s and s in text:
            raise SystemExit("refusing to write: the fixture contains a --secret value")
    return fixture


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("capture", type=Path)
    p.add_argument("--id", required=True)
    p.add_argument("--client-name", required=True)
    p.add_argument("--client-version", required=True)
    p.add_argument("--obtained", choices=("real-capture", "synthesized"), default="real-capture")
    p.add_argument("--how", required=True, help="one sentence: how the traffic was produced")
    p.add_argument("--note")
    p.add_argument("--date", default=_dt.date.today().isoformat())
    p.add_argument("--secret", action="append", default=[], help="a value that must not appear (repeatable)")
    p.add_argument("-o", "--out", type=Path, required=True)
    args = p.parse_args(argv)
    records = [json.loads(line) for line in args.capture.read_text().splitlines() if line.strip()]
    meta = {
        "id": args.id,
        "client_name": args.client_name,
        "client_version": args.client_version,
        "obtained": args.obtained,
        "how": args.how,
        "captured_on": args.date,
        "note": args.note,
    }
    fixture = build_fixture(records, meta, args.secret)
    args.out.write_text(json.dumps(fixture, indent=2) + "\n")
    print(f"wrote {args.out}: {len(fixture['requests'])} request(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
