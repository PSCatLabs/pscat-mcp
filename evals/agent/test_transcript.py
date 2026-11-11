"""Offline tests for transcript parsing and case checks. No model needed."""

import json
import unittest
from pathlib import Path

import run
import transcript as tx

HERE = Path(__file__).parent
FIXTURE = HERE / "fixtures" / "claude-first-prompt-stdio.jsonl"
SERVER = "pscat-render"

CTX = {
    "repo": str(HERE.parent.parent),
    "server": SERVER,
    "server_version": "0.1.0",
    "pscat_version": "0.7.0-rc.4",
    "pscat_revision": "df39114f1223cf475c0c2350c47d7475274c72ee",
    "pscat_revision7": "df39114",
    "huge_source": "x\n",
}


def case(case_id):
    return next(c for c in run.load_cases() if c["id"] == case_id)


def fixture():
    return tx.parse_claude_stream(FIXTURE.read_text().splitlines())


class ParseClaude(unittest.TestCase):
    def test_calls_and_results(self):
        t = fixture()
        self.assertEqual(
            [(c.tool, c.args.get("format")) for c in t.calls],
            [
                ("get_renderer_info", None),
                ("render_postscript", "png"),
                ("render_postscript", "svg"),
                ("render_postscript", "pdf"),
                ("validate_postscript", None),
                ("render_postscript", None),
            ],
        )
        self.assertTrue(all(c.server == SERVER for c in t.calls))
        self.assertTrue(all(c.result is not None for c in t.calls))
        self.assertEqual(t.num_turns, 7)
        self.assertTrue(t.completed)
        self.assertEqual(t.stop, "success")
        self.assertEqual(t.model, "claude-sonnet-5-5")
        self.assertIn("liento", t.answer)

    def test_content_types_include_flattened_resources(self):
        types = [c.result["content_types"] for c in fixture().calls]
        self.assertEqual(types[1][0], "image")
        # Claude Code turns embedded resources into "[Resource from ...]" text.
        self.assertEqual(types[2][0], "resource:image/svg+xml")
        self.assertEqual(types[3][0], "resource:application/pdf")

    def test_structured_content_recovered_from_json_text(self):
        calls = fixture().calls
        self.assertEqual(calls[0].result["structured"]["server_version"], "0.1.0")
        self.assertEqual(calls[1].result["structured"]["artifacts"][0]["width_px"], 300)
        self.assertFalse(calls[4].result["structured"]["valid"])

    def test_error_code_from_structured_then_text(self):
        call = tx.ToolCall("1", "mcp__s__render_postscript", "s", "render_postscript", {})
        call.result = {"is_error": True, "text": "", "structured": {"ok": False, "error": {"code": "timeout"}}}
        self.assertEqual(call.error_code, "timeout")
        call.result = {"is_error": True, "text": "Render failed (unsupported_format): no", "structured": None}
        self.assertEqual(call.error_code, "unsupported_format")
        call.result = {"is_error": False, "text": "Render failed (timeout): x", "structured": None}
        self.assertIsNone(call.error_code)

    def test_subagent_turns_and_garbage_are_ignored(self):
        lines = [
            "not json",
            json.dumps(
                {
                    "type": "assistant",
                    "parent_tool_use_id": "x",
                    "message": {"content": [{"type": "tool_use", "id": "a", "name": "Bash", "input": {}}]},
                }
            ),
        ]
        self.assertEqual(tx.parse_claude_stream(lines).calls, [])


class FirstPromptFixture(unittest.TestCase):
    def test_only_the_loop_step_fails(self):
        # The fixture was recorded with the original LOCAL_TESTING step 5,
        # `%!PS { 1 pop } loop` on one line: all of it is a comment, so the
        # render succeeded with a blank page and the agent said so. The case
        # (and the doc) now put the loop on its own line.
        checks = tx.evaluate_case(case("first-prompt"), fixture(), SERVER, CTX, "stdio")
        failed = [c.name for c in checks if not c.ok]
        self.assertEqual(failed, ["loop render (any)"], [(c.name, c.detail) for c in checks if not c.ok])

    def test_http_post_count_check(self):
        checks = tx.evaluate_case(case("first-prompt"), fixture(), SERVER, dict(CTX, http_posts=2), "http")
        post = next(c for c in checks if "POST /mcp" in c.name)
        self.assertFalse(post.ok)
        checks = tx.evaluate_case(case("first-prompt"), fixture(), SERVER, dict(CTX, http_posts=9), "http")
        self.assertTrue(next(c for c in checks if "POST /mcp" in c.name).ok)

    def test_corrected_typo_is_caught(self):
        t = fixture()
        validate = next(c for c in t.calls if c.tool == "validate_postscript")
        validate.args["source"] = validate.args["source"].replace("liento", "lineto")
        checks = tx.evaluate_case(case("first-prompt"), t, SERVER, CTX, "stdio")
        bad = next(c for c in checks if c.name.startswith("validate keeps the typo"))
        self.assertFalse(bad.ok)
        self.assertIn("text differs", bad.detail)

    def test_foreign_tool_and_unfinished_run_fail(self):
        t = fixture()
        t.calls.append(tx.ToolCall("z", "mcp__pscat__render_postscript", "pscat", "render_postscript", {}))
        t.stop = "error_max_turns"
        names = {c.name for c in tx.evaluate_case(case("first-prompt"), t, SERVER, CTX, "stdio") if not c.ok}
        self.assertIn("only pscat-render tools", names)
        self.assertIn("agent finished", names)


class DescribeCapabilities(unittest.TestCase):
    """Synthetic transcripts for the `describe-capabilities` case. Claude Code
    hands the model a tool's structuredContent as the JSON text of the result
    (see the recorded fixture), so each result here is that JSON text and the
    checks read the structure recovered from it."""

    @staticmethod
    def call(args, payload, is_error=False):
        c = tx.ToolCall("1", "mcp__pscat-render__describe_art_capabilities", SERVER, "describe_art_capabilities", args)
        text = json.dumps(payload)
        c.result = {
            "is_error": is_error,
            "text": text,
            "content_types": ["text"],
            "structured": tx._structured_from_text(text),
        }
        return c

    def run_case(self, calls, answer):
        t = tx.Transcript(agent="claude", calls=calls, answer=answer, completed=True, stop="success", num_turns=4)
        return {c.name: c.ok for c in tx.evaluate_case(case("describe-capabilities"), t, SERVER, CTX, "stdio")}

    ANSWER = "Palettes include acid and brass. Procedure hatch: load it with (lib/artkit.ps) run, then call it."
    INDEX = {"ok": True, "view": "index", "pscat_version": "0.7.0-rc.4", "index": [{"kind": "palette", "count": 2, "names": ["acid", "brass"]}]}
    KIND = {"ok": True, "view": "kind", "kind": "palette", "total": 22, "offset": 0, "summaries": [{"name": "acid", "description": "d"}]}
    ENTRY = {
        "ok": True,
        "view": "names",
        "entries": [
            {
                "name": "hatch",
                "kind": "procedure",
                "description": "d",
                "parameters": [],
                "source": "lib/artkit.ps",
                "load": "(lib/artkit.ps) run",
                "example": "x",
                "availability": "library",
            }
        ],
        "not_found": [],
        "omitted": [],
    }

    def test_index_then_named_entry_passes(self):
        results = self.run_case([self.call({}, self.INDEX), self.call({"names": ["hatch"]}, self.ENTRY)], self.ANSWER)
        self.assertTrue(all(results.values()), results)

    def test_kind_listing_also_counts_as_the_palette_call(self):
        calls = [self.call({"kind": "palette"}, self.KIND), self.call({"names": ["hatch"]}, self.ENTRY)]
        results = self.run_case(calls, self.ANSWER)
        self.assertTrue(all(results.values()), results)

    def test_missing_entry_lookup_or_made_up_names_fail(self):
        index = self.call({}, self.INDEX)
        results = self.run_case([index], self.ANSWER)
        self.assertFalse(results["full entry for named procedure: called"])
        entry = self.call({"names": ["hatch"]}, self.ENTRY)
        results = self.run_case([index, entry], "Palettes: foo, bar. Use (lib/artkit.ps) run.")
        self.assertFalse(all(results.values()))
        missing = dict(self.ENTRY, entries=[], not_found=["hatchh"])
        nf = self.call({"names": ["hatchh"]}, missing)
        results = self.run_case([index, nf], self.ANSWER)
        self.assertFalse(results["full entry for named procedure (any)"])
        failed = self.call({"names": ["hatch"]}, {"ok": False, "error": {"code": "worker_failed"}}, is_error=True)
        results = self.run_case([index, failed], self.ANSWER)
        self.assertFalse(results["full entry for named procedure (any)"])


class Matchers(unittest.TestCase):
    def m(self, spec, value=tx._MISSING):
        return tx.match_value(spec, value)[0]

    def test_operators(self):
        self.assertTrue(self.m(400, 400))
        self.assertFalse(self.m(400, 401))
        self.assertFalse(self.m(400))
        self.assertTrue(self.m({"in": ["png", None]}))
        self.assertTrue(self.m({"in": ["png", None]}, "png"))
        self.assertFalse(self.m({"in": ["png", None]}, "svg"))
        self.assertTrue(self.m({"absent": True}))
        self.assertFalse(self.m({"absent": True}, 1))
        self.assertTrue(self.m({"equals_text": "a\nb\n"}, "a  \r\nb"))
        self.assertFalse(self.m({"equals_text": "a\nb"}, "a\nc"))
        self.assertTrue(self.m({"contains": ["a", "b"]}, "xaby"))
        self.assertFalse(self.m({"contains": "q"}, "xaby"))
        self.assertTrue(self.m({"not_contains": "q"}, "xaby"))
        self.assertTrue(self.m({"min": 1, "max": 3}, 2))
        self.assertFalse(self.m({"max": 3}, 4))
        self.assertTrue(self.m({"regex": "^li"}, "liento"))
        with self.assertRaises(ValueError):
            self.m({"bogus": 1}, 1)

    def test_answer_checks_are_case_insensitive(self):
        checks = tx.check_answer({"all": ["PNG"], "any": [["jpeg", "jpg"]], "none": ["sorry"]}, "No JPEG; png works")
        self.assertTrue(all(c.ok for c in checks))

    def test_expect_any_of_and_forbid(self):
        c = case("unsupported-format")
        declined = tx.Transcript(agent="claude", answer="JPEG isn't supported; I can render a PNG.", completed=True, stop="success")
        self.assertTrue(all(ch.ok for ch in tx.evaluate_case(c, declined, SERVER, CTX, "stdio")))
        call = tx.ToolCall("1", "mcp__pscat-render__render_postscript", SERVER, "render_postscript", {"source": "x", "format": "jpeg"})
        call.result = {"is_error": True, "text": "Render failed (unsupported_format): no", "content_types": ["text"], "structured": None}
        tried = tx.Transcript(agent="claude", calls=[call], answer="JPEG is unsupported; PNG works.", completed=True, stop="success")
        self.assertTrue(all(ch.ok for ch in tx.evaluate_case(c, tried, SERVER, CTX, "stdio")))
        call.result["text"] = "Render failed (worker_failed): no"
        self.assertFalse(all(ch.ok for ch in tx.evaluate_case(c, tried, SERVER, CTX, "stdio")))

    def test_placeholders(self):
        self.assertEqual(tx.expand("v{{server_version}}", CTX), "v0.1.0")
        self.assertTrue(tx.expand("{{file:examples/shapes.ps}}", CTX).startswith("%!PS"))
        with self.assertRaises(KeyError):
            tx.expand("{{nope}}", CTX)


class CaseFiles(unittest.TestCase):
    def test_every_case_is_well_formed(self):
        cases = run.load_cases()
        self.assertGreaterEqual(len(cases), 10)
        empty = tx.Transcript(agent="claude")
        for c in cases:
            with self.subTest(case=c["id"]):
                self.assertTrue(c["prompt"].strip())
                self.assertTrue(set(c["transports"]) <= {"stdio", "http"})
                self.assertTrue("expect" in c or "expect_any_of" in c)
                tx.expand(c["prompt"], CTX)
                # Runs every matcher against an empty transcript: unknown
                # operators or placeholders raise here.
                checks = tx.evaluate_case(c, empty, SERVER, CTX, "stdio")
                self.assertFalse(all(ch.ok for ch in checks))


class ParseCodex(unittest.TestCase):
    def test_documented_event_shapes(self):
        lines = [
            {"type": "thread.started", "thread_id": "t"},
            {"type": "turn.started"},
            {
                "type": "item.started",
                "item": {"id": "i1", "type": "mcp_tool_call", "server": SERVER, "tool": "get_renderer_info", "arguments": {}, "status": "in_progress"},
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "i1",
                    "type": "mcp_tool_call",
                    "server": SERVER,
                    "tool": "get_renderer_info",
                    "arguments": {},
                    "status": "completed",
                    "result": {"content": [{"type": "text", "text": "{}"}], "structured_content": {"server_version": "0.1.0"}},
                },
            },
            {"type": "item.completed", "item": {"id": "i2", "type": "command_execution", "command": "pscat x"}},
            {"type": "item.completed", "item": {"id": "i3", "type": "agent_message", "text": "0.1.0"}},
            {"type": "turn.completed", "usage": {}},
        ]
        t = tx.parse_codex_jsonl([json.dumps(e) for e in lines], SERVER)
        self.assertEqual([c.name for c in t.calls], ["mcp__pscat-render__get_renderer_info", "command_execution"])
        self.assertEqual(t.calls[0].result["structured"]["server_version"], "0.1.0")
        self.assertEqual(t.answer, "0.1.0")
        self.assertTrue(t.completed)
        self.assertIsNone(t.error)

    def test_usage_limit_error_observed_with_codex_0_167(self):
        msg = "You've hit your usage limit. Try again later."
        lines = [
            {"type": "thread.started", "thread_id": "t"},
            {"type": "turn.started"},
            {"type": "error", "message": msg},
            {"type": "turn.failed", "error": {"message": msg}},
        ]
        t = tx.parse_codex_jsonl([json.dumps(e) for e in lines], SERVER)
        self.assertEqual(t.stop, "error")
        self.assertIn("usage limit", t.error)


class Recheck(unittest.TestCase):
    def test_recheck_a_saved_run_without_a_model(self):
        import contextlib
        import io
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "transcripts").mkdir()
            shutil.copy(FIXTURE, Path(d) / "transcripts" / "first-prompt__claude__stdio__1.jsonl")
            header = {k: CTX[k] for k in ("server_version", "pscat_version", "pscat_revision")}
            header.update(clients={}, model="claude-sonnet-5-5")
            (Path(d) / "summary.json").write_text(json.dumps({"header": header}))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                status = run.main(["--recheck", d])
        self.assertEqual(status, 1)  # the recorded one-line loop step fails
        self.assertIn("FAIL loop render", out.getvalue())


class HttpLog(unittest.TestCase):
    def test_post_count_reads_tower_http_lines(self):
        import tempfile

        srv = run.HttpServer.__new__(run.HttpServer)
        srv.token = "secret-token"
        with tempfile.TemporaryDirectory() as d:
            srv.log_path = Path(d) / "log"
            line = {"message": "finished processing request", "status": 200, "span": {"method": "POST", "path": "/mcp"}}
            other = {"message": "finished processing request", "status": 200, "span": {"method": "GET", "path": "/healthz"}}
            srv.log_path.write_text("\n".join(json.dumps(x) for x in (line, other, line)) + "\nnot json secret-token\n")
            self.assertEqual(srv.post_count(), 2)
            self.assertNotIn("secret-token", srv.log_text())


if __name__ == "__main__":
    unittest.main()
