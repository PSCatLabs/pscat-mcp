"""Turn an agent transcript into tool calls and an answer, and check a case's
expectations against it. Pure functions: no model, no network, no server.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

ERROR_CODE_IN_TEXT = re.compile(r"\(([a-z_]+)\):")


@dataclass
class ToolCall:
    id: str
    name: str  # full name as the agent saw it, e.g. mcp__pscat-render__render_postscript
    server: str | None
    tool: str
    args: dict
    result: dict | None = None  # {is_error, text, content_types, structured}

    @property
    def error_code(self) -> str | None:
        if not self.result or not self.result.get("is_error"):
            return None
        structured = self.result.get("structured")
        if isinstance(structured, dict):
            err = structured.get("error")
            if isinstance(err, dict) and isinstance(err.get("code"), str):
                return err["code"]
        # The server's error text reads "Render failed (<code>): <message>".
        m = ERROR_CODE_IN_TEXT.search(self.result.get("text") or "")
        return m.group(1) if m else None


@dataclass
class Transcript:
    agent: str
    calls: list[ToolCall] = field(default_factory=list)
    answer: str = ""
    num_turns: int | None = None
    completed: bool = False
    stop: str | None = None  # result subtype / terminal reason
    permission_denials: list = field(default_factory=list)
    model: str | None = None
    rate_limits: list[dict] = field(default_factory=list)
    error: str | None = None  # the agent CLI's own error (usage limit, auth)


def split_tool_name(name: str) -> tuple[str | None, str]:
    if name.startswith("mcp__"):
        rest = name[len("mcp__") :]
        server, sep, tool = rest.partition("__")
        if sep:
            return server, tool
    return None, name


# Claude Code hands an embedded resource to the model as a text block:
# "[Resource from <server> at <uri>] <svg ...>" for a text resource, and
# "[Resource from <server> at <uri>] Binary content (application/pdf, 3.4KB)
# saved to <path>" for a blob (observed with Claude Code 2.1.322).
FLATTENED_RESOURCE = re.compile(r"^\[Resource from [^\]]* at [^\]]*\]\s*")
BINARY_MIME = re.compile(r"^Binary content \(([^,)]+)")


def _flattened_resource(text: str) -> str | None:
    m = FLATTENED_RESOURCE.match(text)
    if not m:
        return None
    body = text[m.end() :]
    b = BINARY_MIME.match(body)
    if b:
        return f"resource:{b.group(1).strip()}"
    if body.lstrip().startswith("<svg") or body.lstrip().startswith("<?xml"):
        return "resource:image/svg+xml"
    return "resource"


def _structured_from_text(content: Any) -> dict | None:
    """Claude Code passes a tool's structuredContent to the model as a JSON
    text block (the whole content, or the last block after resources)."""
    blocks = [content] if isinstance(content, str) else [
        b.get("text", "") for b in content or [] if isinstance(b, dict) and b.get("type") == "text"
    ]
    for text in reversed(blocks):
        text = text.strip()
        if text.startswith("{"):
            try:
                value = json.loads(text)
            except ValueError:
                continue
            if isinstance(value, dict):
                return value
    return None


def _blocks_text(content: Any) -> tuple[str, list[str]]:
    """Text and block types of a tool_result's content (string or blocks)."""
    if isinstance(content, str):
        return content, ["text"]
    texts, types = [], []
    for block in content or []:
        if not isinstance(block, dict):
            continue
        btype = block.get("type", "?")
        types.append(btype)
        if btype == "text":
            text = block.get("text", "")
            texts.append(text)
            resource = _flattened_resource(text)
            if resource:
                types[-1] = resource
        elif btype == "resource":
            res = block.get("resource") or {}
            types[-1] = f"resource:{res.get('mimeType', '?')}"
            if "text" in res:
                texts.append(res.get("text", "")[:2000])
    return "\n".join(texts), types


def parse_claude_stream(lines: Iterable[str]) -> Transcript:
    t = Transcript(agent="claude")
    by_id: dict[str, ToolCall] = {}
    last_text = ""
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except ValueError:
            continue
        etype = event.get("type")
        if etype == "system" and event.get("subtype") == "init":
            t.model = event.get("model")
        elif etype == "assistant":
            if event.get("parent_tool_use_id"):
                continue  # a subagent's turn
            for block in (event.get("message") or {}).get("content") or []:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use":
                    server, tool = split_tool_name(block.get("name", ""))
                    call = ToolCall(
                        id=block.get("id", ""),
                        name=block.get("name", ""),
                        server=server,
                        tool=tool,
                        args=block.get("input") or {},
                    )
                    t.calls.append(call)
                    by_id[call.id] = call
                elif block.get("type") == "text" and block.get("text", "").strip():
                    last_text = block["text"]
        elif etype == "user":
            content = (event.get("message") or {}).get("content")
            if not isinstance(content, list):
                continue
            raw = event.get("tool_use_result")
            for block in content:
                if not isinstance(block, dict) or block.get("type") != "tool_result":
                    continue
                call = by_id.get(block.get("tool_use_id", ""))
                if call is None:
                    continue
                text, types = _blocks_text(block.get("content"))
                structured = None
                if isinstance(raw, dict):
                    structured = raw.get("structuredContent") or raw.get("structured_content")
                if structured is None:
                    structured = _structured_from_text(block.get("content"))
                call.result = {
                    "is_error": bool(block.get("is_error")),
                    "text": text,
                    "content_types": types,
                    "structured": structured,
                }
        elif etype == "rate_limit_event":
            info = event.get("rate_limit_info")
            if isinstance(info, dict):
                t.rate_limits.append(info)
        elif etype == "result":
            t.completed = True
            t.stop = event.get("subtype")
            t.num_turns = event.get("num_turns")
            t.permission_denials = event.get("permission_denials") or []
            if isinstance(event.get("result"), str):
                last_text = event["result"] or last_text
    t.answer = last_text
    return t


CODEX_FOREIGN_ITEMS = ("command_execution", "file_change", "web_search")


def parse_codex_jsonl(lines: Iterable[str], server_name: str) -> Transcript:
    """`codex exec --json` events: item.started / item.completed with
    item.type == "mcp_tool_call", agent_message items, turn.completed.

    Written against the documented event shapes; the error and turn events
    were seen with codex-cli 0.167.1, a completed mcp_tool_call was not (the
    account's usage limit was reached), so treat Codex results as
    provisional until a transcript confirms the fields."""
    t = Transcript(agent="codex")
    by_id: dict[str, ToolCall] = {}
    turns = 0
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except ValueError:
            continue
        etype = event.get("type")
        item = event.get("item") if isinstance(event.get("item"), dict) else None
        if etype == "turn.completed":
            turns += 1
            t.completed = True
            t.stop = "success"
        elif etype in ("turn.failed", "error"):
            t.stop = "error"
            err = event.get("error") if isinstance(event.get("error"), dict) else event
            t.error = t.error or str(err.get("message", ""))[:300]
        if not item:
            continue
        itype = item.get("type") or item.get("item_type")
        if itype in CODEX_FOREIGN_ITEMS and item.get("id") not in by_id:
            # Shell, file or web work instead of (or besides) the MCP server.
            call = ToolCall(id=item.get("id", ""), name=itype, server=None, tool=itype, args={})
            t.calls.append(call)
            by_id[call.id] = call
        elif itype == "mcp_tool_call":
            iid = item.get("id", "")
            call = by_id.get(iid)
            if call is None:
                args = item.get("arguments")
                if isinstance(args, str):
                    try:
                        args = json.loads(args)
                    except ValueError:
                        args = {"_raw": args}
                server = item.get("server")
                tool = item.get("tool", "")
                call = ToolCall(
                    id=iid,
                    name=f"mcp__{server}__{tool}",
                    server=server,
                    tool=tool,
                    args=args or {},
                )
                t.calls.append(call)
                by_id[iid] = call
            if etype == "item.completed":
                res = item.get("result") or {}
                err = item.get("error")
                text, types = _blocks_text(res.get("content") if isinstance(res, dict) else None)
                structured = None
                is_error = item.get("status") == "failed" or bool(err)
                if isinstance(res, dict):
                    structured = res.get("structured_content") or res.get("structuredContent")
                    is_error = is_error or bool(res.get("is_error") or res.get("isError"))
                if err and not text:
                    text = err.get("message", "") if isinstance(err, dict) else str(err)
                call.result = {
                    "is_error": is_error,
                    "text": text,
                    "content_types": types,
                    "structured": structured,
                }
        elif itype in ("agent_message", "assistant_message") and etype == "item.completed":
            text = item.get("text") or ""
            if text.strip():
                t.answer = text
    t.num_turns = turns or None
    return t


# --------------------------------------------------------------------------
# Expectations


@dataclass
class Check:
    name: str
    ok: bool
    detail: str = ""


def expand(value: Any, ctx: dict) -> Any:
    """Expand {{file:path}} and {{var}} placeholders in strings (recursively)."""
    if isinstance(value, str):

        def sub(m: re.Match) -> str:
            key = m.group(1).strip()
            if key.startswith("file:"):
                return (Path(ctx["repo"]) / key[5:]).read_text()
            if key not in ctx:
                raise KeyError(f"unknown placeholder {{{{{key}}}}}")
            return str(ctx[key])

        return re.sub(r"\{\{([^}]+)\}\}", sub, value)
    if isinstance(value, list):
        return [expand(v, ctx) for v in value]
    if isinstance(value, dict):
        return {k: expand(v, ctx) for k, v in value.items()}
    return value


def normalize_text(s: str) -> str:
    lines = s.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return "\n".join(line.rstrip() for line in lines).strip("\n")


_MISSING = object()


def match_value(spec: Any, value: Any) -> tuple[bool, str]:
    """A literal means equality; a dict holds operators (all must hold)."""
    if not isinstance(spec, dict):
        if value is _MISSING:
            return False, f"absent, want {spec!r}"
        return value == spec, f"{_short(value)} != {_short(spec)}"
    for op, want in spec.items():
        present = value is not _MISSING
        v = value if present else None
        if op == "absent":
            ok = (not present) == bool(want)
            if not ok:
                return False, f"{'present' if present else 'absent'}, want absent={want}"
            continue
        if op == "in":
            if (v if present else None) not in want:
                return False, f"{_short(v)} not in {want!r}"
            continue
        if not present:
            return False, f"absent (want {op} {_short(want)})"
        if op == "equals":
            if v != want:
                return False, f"{_short(v)} != {_short(want)}"
        elif op == "equals_text":
            if not isinstance(v, str) or normalize_text(v) != normalize_text(want):
                return False, f"text differs from expected ({_diff_hint(v, want)})"
        elif op == "contains":
            wants = want if isinstance(want, list) else [want]
            missing = [w for w in wants if not isinstance(v, str) or w not in v]
            if missing:
                return False, f"does not contain {missing!r}"
        elif op == "not_contains":
            wants = want if isinstance(want, list) else [want]
            found = [w for w in wants if isinstance(v, str) and w in v]
            if found:
                return False, f"contains {found!r}"
        elif op == "min":
            if not isinstance(v, (int, float)) or v < want:
                return False, f"{_short(v)} < {want}"
        elif op == "max":
            if not isinstance(v, (int, float)) or v > want:
                return False, f"{_short(v)} > {want}"
        elif op == "regex":
            if not isinstance(v, str) or not re.search(want, v):
                return False, f"no match for /{want}/"
        else:
            raise ValueError(f"unknown matcher {op!r}")
    return True, ""


def _short(v: Any, n: int = 60) -> str:
    s = repr(v)
    return s if len(s) <= n else s[: n - 3] + "..."


def _diff_hint(got: Any, want: str) -> str:
    if not isinstance(got, str):
        return f"got {type(got).__name__}"
    a, b = normalize_text(got), normalize_text(want)
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            return f"first difference at char {i}: {a[i:i + 20]!r} vs {b[i:i + 20]!r}"
    return f"lengths {len(a)} vs {len(b)}"


def _where_matches(where: dict, args: dict) -> bool:
    for key, spec in where.items():
        ok, _ = match_value(spec, args.get(key, _MISSING))
        if not ok:
            return False
    return True


def _dig(obj: Any, path: str) -> Any:
    """`a.b.0.c` into nested dicts and lists; _MISSING when absent."""
    for part in path.split("."):
        if isinstance(obj, dict) and part in obj:
            obj = obj[part]
        elif isinstance(obj, list) and part.isdigit() and int(part) < len(obj):
            obj = obj[int(part)]
        else:
            return _MISSING
    return obj


def _check_call(call: ToolCall, spec: dict) -> list[str]:
    problems = []
    for key, vspec in (spec.get("args") or {}).items():
        ok, why = match_value(vspec, call.args.get(key, _MISSING))
        if not ok:
            problems.append(f"arg {key}: {why}")
    rspec = spec.get("result")
    if rspec:
        res = call.result
        if res is None:
            return problems + ["no tool result"]
        if "is_error" in rspec and bool(res.get("is_error")) != rspec["is_error"]:
            problems.append(f"is_error={res.get('is_error')}, want {rspec['is_error']}")
        if "error_code" in rspec:
            ok, why = match_value(rspec["error_code"], call.error_code or _MISSING)
            if not ok:
                problems.append(f"error code: {why}")
        for ctype in rspec.get("content_types", []):
            # "image", "resource" or "resource:application/pdf" (prefix match).
            if not any(c.startswith(ctype) for c in res.get("content_types", [])):
                problems.append(f"no {ctype} block (got {res.get('content_types')})")
        for path, vspec in (rspec.get("structured") or {}).items():
            ok, why = match_value(vspec, _dig(res.get("structured"), path))
            if not ok:
                problems.append(f"structured {path}: {why}")
        for needle in rspec.get("text_contains", []):
            if needle.lower() not in (res.get("text") or "").lower():
                problems.append(f"result text lacks {needle!r}")
    return problems


def check_answer(spec: dict, answer: str) -> list[Check]:
    low = answer.lower()
    checks = []
    for kw in spec.get("all", []):
        checks.append(Check(f"answer has {kw!r}", kw.lower() in low))
    for group in spec.get("any", []):
        group = group if isinstance(group, list) else [group]
        checks.append(Check(f"answer has one of {group!r}", any(g.lower() in low for g in group)))
    for kw in spec.get("none", []):
        checks.append(Check(f"answer lacks {kw!r}", kw.lower() not in low))
    for rx in spec.get("regex", []):
        checks.append(Check(f"answer matches /{rx}/", re.search(rx, answer, re.I) is not None))
    return checks


def evaluate_expect(expect: dict, t: Transcript, server: str) -> list[Check]:
    ours = [c for c in t.calls if c.server == server]
    checks: list[Check] = []
    called = {c.tool for c in ours}
    for tool in expect.get("tools_required", []):
        checks.append(Check(f"called {tool}", tool in called))
    for tool in expect.get("tools_forbidden", []):
        checks.append(Check(f"did not call {tool}", tool not in called))
    if "max_calls" in expect:
        checks.append(
            Check(f"at most {expect['max_calls']} tool calls", len(ours) <= expect["max_calls"], f"{len(ours)} calls")
        )
    for spec in expect.get("calls", []):
        where = spec.get("where") or {}
        label = spec.get("label") or f"{spec['tool']} {json.dumps(where) if where else ''}".strip()
        candidates = [c for c in ours if c.tool == spec["tool"] and _where_matches(where, c.args)]
        if spec.get("forbid"):
            checks.append(Check(f"no call {label}", not candidates, f"{len(candidates)} such call(s)"))
            continue
        if "max_count" in spec:
            checks.append(
                Check(
                    f"{label}: at most {spec['max_count']} call(s)",
                    len(candidates) <= spec["max_count"],
                    f"{len(candidates)} call(s)",
                )
            )
        if not candidates:
            checks.append(Check(f"{label}: called", False, "no matching call"))
            continue
        which = spec.get("which", "any")
        pool = {"any": candidates, "last": candidates[-1:], "first": candidates[:1], "all": candidates}[which]
        results = [(_check_call(c, spec)) for c in pool]
        if which == "all":
            bad = [p for p in results if p]
            checks.append(Check(f"{label}: every call", not bad, "; ".join(bad[0]) if bad else ""))
        else:
            ok = any(not p for p in results)
            detail = "" if ok else "; ".join(results[-1])
            checks.append(Check(f"{label} ({which})", ok, detail))
    if "answer" in expect:
        checks.extend(check_answer(expect["answer"], t.answer))
    return checks


def evaluate_case(case: dict, t: Transcript, server: str, ctx: dict, transport: str) -> list[Check]:
    """Checks every run gets, then the case's own. `expect_any_of` passes when
    one alternative passes; the best alternative's checks are reported."""
    checks = [
        Check("agent finished", t.completed and t.stop == "success", f"stop={t.stop}"),
        Check("no permission denials", not t.permission_denials, json.dumps(t.permission_denials)[:200]),
        Check(
            f"only {server} tools",
            all(c.server == server for c in t.calls),
            ", ".join(sorted({c.name for c in t.calls if c.server != server})),
        ),
    ]
    max_turns = case.get("max_turns")
    if max_turns and t.num_turns is not None:
        checks.append(Check(f"at most {max_turns} turns", t.num_turns <= max_turns, f"{t.num_turns} turns"))
    if transport == "http" and "http_posts" in ctx:
        ours = sum(1 for c in t.calls if c.server == server)
        checks.append(
            Check(
                "server logged a POST /mcp per tool call",
                ctx["http_posts"] >= ours,
                f"{ctx['http_posts']} POST /mcp for {ours} call(s)",
            )
        )
    alternatives = case.get("expect_any_of") or [case.get("expect") or {}]
    alternatives = expand(alternatives, ctx)
    best: list[Check] | None = None
    for i, alt in enumerate(alternatives):
        got = evaluate_expect(alt, t, server)
        if len(alternatives) > 1:
            got = [Check(f"[alt {i + 1}] {c.name}", c.ok, c.detail) for c in got]
        if all(c.ok for c in got):
            best = got
            break
        if best is None or sum(c.ok for c in got) > sum(c.ok for c in best):
            best = got
    return checks + (best or [])
