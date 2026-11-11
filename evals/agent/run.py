#!/usr/bin/env python3
"""Agent-layer evals: real agents (Claude Code headless, optionally Codex)
against pscat-mcp-server over stdio and Streamable HTTP.

Local and manual only; never run in CI. Claude usage comes only from the
claude.ai subscription login; see guard.py and docs/LOCAL_TESTING.md
("Automated agent evals"). Python 3 standard library only:

    python3 -I evals/agent/run.py --list
    python3 -I evals/agent/run.py --case first-prompt --transport stdio
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from contextlib import contextmanager
from pathlib import Path

# `python3 -I` leaves the script's directory off sys.path; add it explicitly.
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import guard  # noqa: E402
import transcript as tx  # noqa: E402

REPO = HERE.parent.parent
CASES_DIR = HERE / "cases"
RUNS_DIR = REPO / "evals" / "agent-runs"
SERVER_NAME = "pscat-render"
DEFAULT_MODEL = "claude-sonnet-5-5"
DEFAULT_MAX_TURNS = 10
DEFAULT_TIMEOUT_S = 300
REDACTED = "<redacted-token>"


# --------------------------------------------------------------------------
# Cases


def load_cases() -> list[dict]:
    cases = []
    for path in sorted(CASES_DIR.glob("*.json")):
        case = json.loads(path.read_text())
        case.setdefault("id", path.stem)
        case.setdefault("transports", ["stdio", "http"])
        case.setdefault("tags", [])
        cases.append(case)
    ids = [c["id"] for c in cases]
    dupes = {i for i in ids if ids.count(i) > 1}
    if dupes:
        sys.exit(f"duplicate case ids: {sorted(dupes)}")
    return cases


# --------------------------------------------------------------------------
# Server


def server_info(binary: Path) -> dict:
    """get_renderer_info over raw JSON-RPC on stdio; no model involved."""
    msgs = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "agent-eval", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "get_renderer_info", "arguments": {}},
        },
    ]
    proc = subprocess.Popen(
        [str(binary), "serve-stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    try:
        assert proc.stdin and proc.stdout
        proc.stdin.write(json.dumps(msgs[0]) + "\n")
        proc.stdin.flush()
        deadline = time.monotonic() + 20
        sent_rest = False
        while time.monotonic() < deadline:
            line = proc.stdout.readline()
            if not line:
                break
            msg = json.loads(line)
            if msg.get("id") == 1 and not sent_rest:
                for m in msgs[1:]:
                    proc.stdin.write(json.dumps(m) + "\n")
                proc.stdin.flush()
                sent_rest = True
            elif msg.get("id") == 2:
                result = msg["result"]
                info = result.get("structuredContent")
                if info is None:
                    info = json.loads(result["content"][0]["text"])
                return info
        raise RuntimeError("no get_renderer_info reply from the server")
    finally:
        proc.kill()
        proc.wait()


def placeholder_ctx(info: dict) -> dict:
    renderer = info.get("renderer") or {}
    rev = renderer.get("revision") or ""
    return {
        "repo": str(REPO),
        "server": SERVER_NAME,
        "server_version": info.get("server_version", ""),
        "pscat_version": renderer.get("version", ""),
        "pscat_revision": rev,
        "pscat_revision7": rev[:7],
        # Scenario 5c: about 285 KB, over the 256 KiB default source limit.
        "huge_source": "0 0 moveto 1 1 lineto stroke\n" * 9500,
    }


class HttpServer:
    """serve-http on a free loopback port with a throwaway bearer token."""

    def __init__(self, binary: Path, extra_args: list[str] | None = None):
        self.binary = binary
        # e.g. ["--capture-file", path] (evals/server/record_claude.py)
        self.extra_args = list(extra_args or [])
        self.token = secrets.token_hex(32)
        self.secret_dir = Path(tempfile.mkdtemp(prefix="pscat-eval-http-"))
        os.chmod(self.secret_dir, 0o700)
        self.token_file = self.secret_dir / "token"
        fd = os.open(self.token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(self.token + "\n")
        self.log_path = self.secret_dir / "server.log"
        self.proc: subprocess.Popen | None = None
        self.url = ""

    def start(self) -> None:
        env = guard.child_env(os.environ, {"PSCAT_MCP_LOG_FORMAT": "json"})
        self._log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [
                str(self.binary),
                "serve-http",
                "--bind",
                "127.0.0.1",
                "--port",
                "0",
                "--auth-token-file",
                str(self.token_file),
                *self.extra_args,
            ],
            stdin=subprocess.DEVNULL,
            stdout=self._log,
            stderr=self._log,
            env=env,
        )
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"serve-http exited {self.proc.returncode}: {self.log_text()[-500:]}")
            m = re.search(r'"addr":"(127\.0\.0\.1:\d+)"', self.log_text())
            if m:
                self.url = f"http://{m.group(1)}/mcp"
                with urllib.request.urlopen(f"http://{m.group(1)}/healthz", timeout=5) as r:
                    if r.status == 200:
                        return
            time.sleep(0.1)
        raise RuntimeError("serve-http did not report its address")

    def log_text(self) -> str:
        try:
            return self.log_path.read_text().replace(self.token, REDACTED)
        except FileNotFoundError:
            return ""

    def post_count(self) -> int:
        n = 0
        for line in self.log_text().splitlines():
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            # tower-http's response line: {"message": "finished processing
            # request", "status": 200, "span": {"method": "POST", "path": "/mcp"}}
            span = rec.get("span") or {}
            if (
                rec.get("message") == "finished processing request"
                and span.get("method") == "POST"
                and str(span.get("path", "")).split("?")[0] == "/mcp"
            ):
                n += 1
        return n

    def mcp_config(self) -> dict:
        return {
            "mcpServers": {
                SERVER_NAME: {
                    "type": "http",
                    "url": self.url,
                    "headers": {"Authorization": f"Bearer {self.token}"},
                }
            }
        }

    def stop(self, save_log_to: Path | None = None) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if getattr(self, "_log", None):
            self._log.close()
        if save_log_to:
            save_log_to.write_text(self.log_text())
        shutil.rmtree(self.secret_dir, ignore_errors=True)


def stdio_config(binary: Path) -> dict:
    return {"mcpServers": {SERVER_NAME: {"type": "stdio", "command": str(binary), "args": ["serve-stdio"]}}}


def forget_claude_project(workdir: Path) -> None:
    """Claude Code keeps per-directory state (large tool results such as a
    PDF blob, an auto-memory folder) under <config>/projects/<slug of cwd>
    even with --no-session-persistence. Each run's cwd is a fresh temp dir,
    so remove that one folder; nothing else in the config is touched."""
    config = Path(os.environ.get("CLAUDE_CONFIG_DIR") or Path.home() / ".claude")
    slug = re.sub(r"[^A-Za-z0-9]", "-", str(workdir.resolve()))
    target = config / "projects" / slug
    if "pscat-eval-cwd-" in slug and target.parent == config / "projects" and target.is_dir():
        shutil.rmtree(target, ignore_errors=True)


def write_private_json(path: Path, data: dict) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(data, f)


# --------------------------------------------------------------------------
# Agents


def claude_argv(claude: str, model: str, max_turns: int, config: Path, effort: str | None) -> list[str]:
    argv = [
        claude,
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",  # required by -p with stream-json
        "--model",
        model,
        "--max-turns",
        str(max_turns),
        "--mcp-config",
        str(config),
        "--strict-mcp-config",  # no user, project or plugin MCP servers
        "--tools=",  # no built-in tools (Bash, Read, ToolSearch, ...)
        "--allowedTools",
        f"mcp__{SERVER_NAME}__*",
        "--permission-mode",
        "dontAsk",  # anything not allowed above is denied, never prompted
        "--setting-sources",
        "project",  # skip user settings: no installed plugins, hooks, default mode
        "--no-session-persistence",
    ]
    if effort:
        argv += ["--effort", effort]
    guard.assert_safe_argv(argv)
    return argv


def codex_argv(codex: str, model: str | None, workdir: Path, binary: Path, http: HttpServer | None) -> list[str]:
    argv = [
        codex,
        "exec",
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",  # no user MCP servers or plugins (auth still from CODEX_HOME)
        "--ignore-rules",
        "--sandbox",
        "read-only",
        "--cd",
        str(workdir),
        "-c",
        'approval_policy="never"',
    ]
    if model:
        argv += ["--model", model]
    key = f"mcp_servers.{SERVER_NAME}"
    if http:
        argv += ["-c", f"{key}.url={json.dumps(http.url)}", "-c", f'{key}.bearer_token_env_var="PSCAT_EVAL_TOKEN"']
    else:
        argv += ["-c", f"{key}.command={json.dumps(str(binary))}", "-c", f'{key}.args=["serve-stdio"]']
    argv.append("-")  # prompt on stdin
    return argv


def tool_version(cmd: str) -> str:
    try:
        out = subprocess.run([cmd, "--version"], capture_output=True, text=True, timeout=30, env=guard.child_env(os.environ))
        return out.stdout.strip() or out.stderr.strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unavailable ({e})"


def claude_preflight(claude: str) -> dict:
    guard.check_parent_env(os.environ)
    env = guard.child_env(os.environ)
    out = subprocess.run([claude, "auth", "status", "--json"], capture_output=True, text=True, timeout=60, env=env)
    return guard.check_auth_status(out.returncode, out.stdout)


class RunAborted(Exception):
    pass


def run_child(argv, prompt, env, cwd, timeout_s, on_line, out_path: Path, err_path: Path, redact: str | None):
    """Run an agent, feeding each stdout line to on_line (which may raise to
    abort). The child gets its own process group so the whole tree dies."""
    proc = subprocess.Popen(
        argv,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=cwd,
        env=env,
        start_new_session=True,
    )

    def kill():
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    timed_out = threading.Event()
    timer = threading.Timer(timeout_s, lambda: (timed_out.set(), kill()))
    timer.start()

    def scrub(s: str) -> str:
        return s.replace(redact, REDACTED) if redact else s

    err_chunks: list[str] = []
    err_thread = threading.Thread(target=lambda: err_chunks.append(proc.stderr.read()), daemon=True)
    err_thread.start()
    lines: list[str] = []
    abort: BaseException | None = None
    try:
        assert proc.stdin and proc.stdout
        proc.stdin.write(prompt)
        proc.stdin.close()
        with open(out_path, "w") as out:
            for line in proc.stdout:
                lines.append(line)
                out.write(scrub(line))
                out.flush()
                try:
                    on_line(line)
                except BaseException as e:  # noqa: BLE001 - abort on anything
                    abort = e
                    kill()
                    break
    finally:
        timer.cancel()
        if abort is not None or timed_out.is_set():
            kill()
        proc.wait()
        err_thread.join(timeout=5)
        err_path.write_text(scrub("".join(err_chunks)))
    if abort is not None:
        raise abort
    return lines, proc.returncode, timed_out.is_set()


# --------------------------------------------------------------------------
# Main


def summarize(results: list[dict]) -> list[dict]:
    rows: dict[tuple, dict] = {}
    for r in results:
        key = (r["case"], r["agent"], r["transport"])
        row = rows.setdefault(key, {"case": key[0], "agent": key[1], "transport": key[2], "pass": 0, "fail": 0, "error": 0, "turns": []})
        row[r["status"]] += 1
        if r.get("num_turns") is not None:
            row["turns"].append(r["num_turns"])
    out = []
    for row in rows.values():
        n = row["pass"] + row["fail"] + row["error"]
        if row["error"] and not row["pass"] and not row["fail"]:
            verdict = "error"
        elif row["pass"] == n:
            verdict = "pass"
        elif row["pass"] == 0:
            verdict = "fail"
        else:
            verdict = "flaky"
        row["verdict"] = verdict
        row["runs"] = n
        out.append(row)
    return out


def print_table(rows: list[dict], header: dict) -> str:
    lines = [
        f"server: pscat-mcp {header['server_version']}, PSCat {header['pscat_version']} ({header['pscat_revision'][:12]})",
        "clients: " + ", ".join(f"{k} {v}" for k, v in header["clients"].items()),
        f"model: {header['model']}",
        "",
    ]
    cols = ("case", "agent", "transport", "verdict", "passed", "turns")
    table = [cols] + [
        (
            r["case"],
            r["agent"],
            r["transport"],
            r["verdict"],
            f"{r['pass']}/{r['runs']}",
            ",".join(map(str, r["turns"])) or "-",
        )
        for r in rows
    ]
    widths = [max(len(str(row[i])) for row in table) for i in range(len(cols))]
    for i, row in enumerate(table):
        lines.append("  ".join(str(c).ljust(w) for c, w in zip(row, widths)).rstrip())
        if i == 0:
            lines.append("  ".join("-" * w for w in widths))
    text = "\n".join(lines)
    print(text)
    return text


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--case", action="append", help="case id to run (repeatable; default: all)")
    p.add_argument("--transport", choices=("stdio", "http", "both"), default="both")
    p.add_argument("--agent", action="append", choices=("claude", "codex"), help="default: claude. codex uses OpenAI usage")
    p.add_argument("--repeat", type=int, default=1, help="runs per case/agent/transport (default 1)")
    p.add_argument("--max-cases", type=int, help="stop after this many cases")
    p.add_argument("--model", default=DEFAULT_MODEL, help=f"Claude model (default {DEFAULT_MODEL})")
    p.add_argument("--effort", choices=("low", "medium", "high", "xhigh", "max"), help="Claude effort level (default: the CLI's)")
    p.add_argument("--codex-model", help="Codex model (default: Codex's)")
    p.add_argument("--server", type=Path, default=REPO / "target" / "release" / "pscat-mcp-server")
    p.add_argument("--include-expensive", action="store_true", help="also run cases tagged expensive")
    p.add_argument("--list", action="store_true", help="list cases and exit")
    p.add_argument("--dry-run", action="store_true", help="run the guard pre-flight and print the plan; no agent runs")
    p.add_argument("--out", type=Path, help="run directory (default evals/agent-runs/<timestamp>)")
    p.add_argument(
        "--recheck",
        type=Path,
        metavar="RUN_DIR",
        help="re-evaluate a previous run's saved transcripts against the current cases; no agent runs",
    )
    args = p.parse_args(argv)

    cases = load_cases()
    if args.recheck:
        return recheck(args.recheck, cases, args.server)
    if args.list:
        for c in cases:
            print(f"{c['id']:<24} {'/'.join(c['transports']):<11} {','.join(c['tags']) or '-':<10} {c.get('title', '')}")
        return 0
    if args.case:
        unknown = set(args.case) - {c["id"] for c in cases}
        if unknown:
            sys.exit(f"unknown case(s): {sorted(unknown)}")
        cases = [c for c in cases if c["id"] in args.case]
    elif not args.include_expensive:
        cases = [c for c in cases if "expensive" not in c["tags"]]
    if args.max_cases:
        cases = cases[: args.max_cases]
    agents = args.agent or ["claude"]
    transports = ["stdio", "http"] if args.transport == "both" else [args.transport]

    if not args.server.is_file():
        sys.exit(f"server binary not found: {args.server} (cargo build --release)")
    binary = args.server.resolve()

    clients = {}
    auth = None
    claude = shutil.which("claude") or "claude"
    codex = shutil.which("codex") or "codex"
    if "claude" in agents:
        try:
            auth = claude_preflight(claude)
        except guard.GuardError as e:
            print(f"billing guard: {e}", file=sys.stderr)
            return 2
        clients["claude"] = tool_version(claude)
    if "codex" in agents:
        clients["codex"] = tool_version(codex)
    info = server_info(binary)
    ctx0 = placeholder_ctx(info)
    header = {
        "server_version": ctx0["server_version"],
        "pscat_version": ctx0["pscat_version"],
        "pscat_revision": ctx0["pscat_revision"],
        "clients": clients,
        "model": args.model,
        "auth": auth,
    }
    plan = [
        (c, a, t, k)
        for c in cases
        for a in agents
        for t in transports
        if t in c["transports"]
        for k in range(args.repeat)
    ]
    print(f"{len(plan)} run(s): {len(cases)} case(s) x {agents} x {transports} x {args.repeat}")
    if auth:
        print(f"billing guard pre-flight ok: {auth}")
    if args.dry_run:
        for c, a, t, k in plan:
            print(f"  {c['id']} {a} {t} #{k + 1}")
        return 0

    stamp = _dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    run_dir = (args.out or RUNS_DIR / stamp).resolve()
    (run_dir / "transcripts").mkdir(parents=True, exist_ok=True)
    print(f"transcripts: {run_dir}")

    results: list[dict] = []
    stopped: str | None = None
    http: HttpServer | None = None
    secret_dir = Path(tempfile.mkdtemp(prefix="pscat-eval-"))
    os.chmod(secret_dir, 0o700)
    try:
        skipped_agents: set[str] = set()
        for c, a, t, k in plan:
            if a in skipped_agents:
                continue
            if t == "http" and http is None:
                http = HttpServer(binary)
                http.start()
            name = f"{c['id']}__{a}__{t}__{k + 1}"
            print(f"- {name} ...", end="", flush=True)
            res = run_one(c, a, t, binary, http, args, claude, codex, ctx0, secret_dir, run_dir, name)
            results.append(res)
            print(f" {res['status']}" + (f" ({res['error']})" if res.get("error") else ""))
            for chk in res.get("checks", []):
                if not chk["ok"]:
                    print(f"    FAIL {chk['name']}: {chk['detail']}")
            if res.get("skip_agent"):
                print(f"    skipping the remaining {a} runs")
                skipped_agents.add(a)
            if res.get("stop_suite"):
                stopped = res["error"]
                break
    except KeyboardInterrupt:
        stopped = "interrupted"
    finally:
        if http:
            http.stop(save_log_to=run_dir / "http-server.log")
        shutil.rmtree(secret_dir, ignore_errors=True)

    rows = summarize(results)
    header["stopped"] = stopped
    print()
    text = print_table(rows, header)
    if stopped:
        print(f"\nSTOPPED: {stopped}")
    (run_dir / "summary.json").write_text(json.dumps({"header": header, "rows": rows, "runs": results}, indent=2))
    (run_dir / "summary.txt").write_text(text + (f"\n\nSTOPPED: {stopped}\n" if stopped else "\n"))
    if stopped and stopped != "interrupted":
        return 2
    return 0 if all(r["verdict"] == "pass" for r in rows) else 1


def recheck(run_dir: Path, cases: list[dict], binary: Path) -> int:
    """Evaluate saved transcripts again, e.g. after editing a case. The HTTP
    POST count is not available offline and is skipped."""
    by_id = {c["id"]: c for c in cases}
    header_path = run_dir / "summary.json"
    header = json.loads(header_path.read_text())["header"] if header_path.exists() else None
    if header:
        ctx0 = {
            "repo": str(REPO),
            "server": SERVER_NAME,
            "server_version": header["server_version"],
            "pscat_version": header["pscat_version"],
            "pscat_revision": header["pscat_revision"],
            "pscat_revision7": header["pscat_revision"][:7],
            "huge_source": "",
        }
    else:
        info = server_info(binary.resolve())
        ctx0 = placeholder_ctx(info)
        header = {k: ctx0[k] for k in ("server_version", "pscat_version", "pscat_revision")}
        header.update(clients={}, model="?")
    results = []
    for path in sorted((run_dir / "transcripts").glob("*.jsonl")):
        case_id, agent, transport, _n = path.stem.split("__")
        case = by_id.get(case_id)
        if case is None:
            print(f"- {path.name}: no case {case_id!r}, skipped")
            continue
        lines = path.read_text().splitlines()
        t = tx.parse_claude_stream(lines) if agent == "claude" else tx.parse_codex_jsonl(lines, SERVER_NAME)
        checks = tx.evaluate_case(case, t, SERVER_NAME, dict(ctx0), transport)
        status = "pass" if all(c.ok for c in checks) else "fail"
        print(f"- {path.stem} ... {status}")
        for c in checks:
            if not c.ok:
                print(f"    FAIL {c.name}: {c.detail}")
        results.append({"case": case_id, "agent": agent, "transport": transport, "status": status, "num_turns": t.num_turns})
    rows = summarize(results)
    print()
    print_table(rows, header)
    return 0 if all(r["verdict"] == "pass" for r in rows) else 1


def run_one(case, agent, transport, binary, http, args, claude, codex, ctx0, secret_dir, run_dir, name) -> dict:
    max_turns = case.get("max_turns", DEFAULT_MAX_TURNS)
    timeout_s = case.get("timeout_s", DEFAULT_TIMEOUT_S)
    ctx = dict(ctx0)
    prompt = tx.expand(case["prompt"], ctx)
    workdir = Path(tempfile.mkdtemp(prefix="pscat-eval-cwd-"))
    out_path = run_dir / "transcripts" / f"{name}.jsonl"
    err_path = run_dir / "transcripts" / f"{name}.stderr"
    record = {"case": case["id"], "agent": agent, "transport": transport, "transcript": str(out_path.relative_to(run_dir))}
    started = time.monotonic()
    posts_before = http.post_count() if (http and transport == "http") else 0
    try:
        if agent == "claude":
            config = secret_dir / f"mcp-{transport}.json"
            write_private_json(config, http.mcp_config() if transport == "http" else stdio_config(binary))
            argv = claude_argv(claude, args.model, max_turns, config, args.effort)
            env = guard.child_env(os.environ)
            sg = guard.StreamGuard(SERVER_NAME, args.model)
            lines, rc, timed_out = run_child(
                argv, prompt, env, workdir, timeout_s, sg.feed, out_path, err_path, http.token if http else None
            )
            if not timed_out:
                sg.finish()
            t = tx.parse_claude_stream(lines)
        else:
            argv = codex_argv(codex, args.codex_model, workdir, binary, http if transport == "http" else None)
            extra = {"PSCAT_EVAL_TOKEN": http.token} if (http and transport == "http") else {}
            for k in ("CODEX_HOME",):
                if k in os.environ:
                    extra[k] = os.environ[k]
            env = guard.child_env(os.environ, extra)
            lines, rc, timed_out = run_child(
                argv, prompt, env, workdir, timeout_s, lambda _l: None, out_path, err_path, http.token if http else None
            )
            t = tx.parse_codex_jsonl(lines, SERVER_NAME)
            if t.error:
                stop = "usage limit" in t.error.lower()
                record.update(status="error", error=f"codex: {t.error}", skip_agent=stop)
                return record
    except guard.GuardError as e:
        record.update(status="error", error=f"{type(e).__name__}: {e}", stop_suite=True)
        return record
    finally:
        forget_claude_project(workdir)
        shutil.rmtree(workdir, ignore_errors=True)
    if http and transport == "http":
        ctx["http_posts"] = http.post_count() - posts_before
    record.update(
        num_turns=t.num_turns,
        exit_code=rc,
        model=t.model,
        calls=[c.name for c in t.calls],
        duration_s=round(time.monotonic() - started, 1),
    )
    if t.rate_limits:
        five = [r.get("utilization") for r in (x.get("unifiedWindows", {}).get("five_hour", {}) for x in t.rate_limits)]
        record["five_hour_utilization"] = [five[0], five[-1]] if five else None
    if timed_out:
        record.update(status="error", error=f"timed out after {timeout_s}s")
        return record
    checks = tx.evaluate_case(case, t, SERVER_NAME, ctx, transport)
    record["checks"] = [{"name": c.name, "ok": c.ok, "detail": c.detail} for c in checks]
    record["status"] = "pass" if all(c.ok for c in checks) else "fail"
    return record


if __name__ == "__main__":
    sys.exit(main())
