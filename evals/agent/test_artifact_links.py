"""Offline tests for per-case server environments, measure-only `observe`
blocks and the artifact-link cases (#155). No model, no server."""

import json
import re
import unittest
from pathlib import Path

import guard
import run
import transcript as tx

HERE = Path(__file__).parent
SERVER = "pscat-render"
CTX = {
    "repo": str(HERE.parent.parent),
    "server": SERVER,
    "server_version": "0.1.0",
    "pscat_version": "0.7.0",
    "pscat_revision": "a" * 40,
    "pscat_revision7": "a" * 7,
    "huge_source": "x\n",
}
MEMORY = {"PSCAT_MCP_ARTIFACT_LINKS": "memory"}
RENDER_ID = "0b5c2f1e-6c63-4a52-9d0c-1f2e3d4c5b6a"


def case(case_id):
    return next(c for c in run.load_cases() if c["id"] == case_id)


class ServerEnv(unittest.TestCase):
    def test_memory_and_off_are_accepted(self):
        for value in ("memory", "off"):
            env = {"PSCAT_MCP_ARTIFACT_LINKS": value}
            self.assertEqual(run.check_case_server_env({"transports": ["stdio"], "server_env": env}), env)

    def test_no_server_env_is_empty(self):
        self.assertEqual(run.check_case_server_env({"transports": ["stdio", "http"]}), {})

    def test_other_names_and_values_are_refused(self):
        for env in (
            {"PSCAT_MCP_ARTIFACT_LINKS": "gcs"},
            {"PSCAT_MCP_ARTIFACT_LINKS": ""},
            {"PSCAT_MCP_ARTIFACT_BUCKET": "b"},
            {"PSCAT_MCP_AUTH_TOKEN": "t"},
            {"ANTHROPIC_API_KEY": "sk"},
            {"PATH": "/x"},
            {},
            [],
        ):
            with self.subTest(env=env), self.assertRaises(ValueError):
                run.check_case_server_env({"transports": ["stdio"], "server_env": env})

    def test_a_case_that_also_runs_over_http_is_refused(self):
        for transports in (["stdio", "http"], ["http"]):
            with self.assertRaises(ValueError):
                run.check_case_server_env({"transports": transports, "server_env": MEMORY})
        # load_cases defaults an absent `transports` to both, so that is refused too.
        with self.assertRaises(ValueError):
            run.check_case_server_env({"server_env": MEMORY})

    def test_stdio_config_puts_env_on_the_server_entry(self):
        cfg = run.stdio_config(Path("/bin/server"), MEMORY)
        entry = cfg["mcpServers"][SERVER]
        self.assertEqual(entry["env"], MEMORY)
        self.assertEqual(entry["args"], ["serve-stdio"])
        # without a server_env the config is exactly what it was
        self.assertEqual(
            run.stdio_config(Path("/bin/server")),
            {"mcpServers": {SERVER: {"type": "stdio", "command": "/bin/server", "args": ["serve-stdio"]}}},
        )

    def test_the_agent_child_gains_no_variable(self):
        parent = {"HOME": "/h", "PATH": "/bin", "PSCAT_MCP_ARTIFACT_LINKS": "gcs", "PSCAT_MCP_ARTIFACT_BUCKET": "b"}
        env = guard.child_env(parent, run.parse_client_env(None))
        self.assertFalse([k for k in env if k.startswith("PSCAT_")])
        self.assertEqual(set(env), {"HOME", "PATH", "TERM", "NO_COLOR"})
        # and the claude command line has no way to carry it either
        argv = run.claude_argv("claude", "claude-sonnet-5-5", 8, Path("/tmp/mcp.json"), None)
        self.assertFalse([a for a in argv if "PSCAT_MCP" in a])

    def test_codex_gets_the_env_as_config_overrides(self):
        argv = run.codex_argv("codex", None, Path("/w"), Path("/bin/server"), None, MEMORY)
        self.assertIn(f'mcp_servers.{SERVER}.env.PSCAT_MCP_ARTIFACT_LINKS="memory"', argv)
        plain = run.codex_argv("codex", None, Path("/w"), Path("/bin/server"), None)
        self.assertFalse([a for a in plain if ".env." in a])

    def test_artifact_cases_set_memory_and_stdio_only(self):
        for cid in ("artifact-links", "artifact-links-both"):
            c = case(cid)
            self.assertEqual(c["transports"], ["stdio"])
            self.assertEqual(c["server_env"], MEMORY)

    def test_other_cases_are_without_a_server_env(self):
        for c in run.load_cases():
            if not c["id"].startswith("artifact-links"):
                self.assertNotIn("server_env", c, c["id"])


def render_call(args, structured, content_types, text=""):
    c = tx.ToolCall("1", f"mcp__{SERVER}__render_postscript", SERVER, "render_postscript", args)
    c.result = {"is_error": False, "text": text, "content_types": content_types, "structured": structured}
    return c


def manifest(delivery, fmt):
    returned = {"url": "link", "both": "image" if fmt == "png" else "blob"}[delivery]
    return {
        "ok": True,
        "format": fmt,
        "render_id": RENDER_ID,
        "artifacts": [
            {
                "returned_as": returned,
                "bytes": 24000 if fmt == "png" else 1200000,
                "download_url": f"pscat://render/{RENDER_ID}/1",
                "expires_at": "2026-11-17T12:30:00Z",
            }
        ],
    }


def transcript(delivery, answer, content_types=("text",), text="", extra_calls=(), arg_delivery=None):
    png = render_call(
        {"source": "0 0 800 500 rectfill 400 250 150 0 360 arc fill", "width": 800, "height": 500, "delivery": arg_delivery or delivery},
        manifest(delivery, "png"),
        list(content_types),
        text,
    )
    svg = render_call(
        {"source": "0 1 13000 {rectfill} for", "format": "svg", "width": 400, "height": 300, "delivery": arg_delivery or delivery},
        manifest(delivery, "svg"),
        list(content_types),
        text,
    )
    return tx.Transcript(
        agent="claude",
        calls=[png, svg, *extra_calls],
        answer=answer,
        num_turns=3,
        completed=True,
        stop="success",
    )


def failed(case_id, t):
    return [c.name for c in tx.evaluate_case(case(case_id), t, SERVER, dict(CTX), "stdio") if not c.ok]


def seen(case_id, t):
    return {o.name: o.ok for o in tx.evaluate_observations(case(case_id), t, SERVER, dict(CTX))}


class ObservationsMeasureOnly(unittest.TestCase):
    ANSWER = "PNG and SVG both came back as links; I had no tool to read them."

    def test_the_url_case_passes_whatever_the_client_did_with_the_links(self):
        # the client showed the model nothing but the structured JSON, and the
        # agent neither quoted a link nor tried to read one: every observation
        # is "no", and the run still passes
        t = transcript("url", self.ANSWER)
        self.assertEqual(failed("artifact-links", t), [])
        obs = seen("artifact-links", t)
        self.assertTrue(obs)
        self.assertFalse(any(obs.values()), obs)

    def test_observations_see_what_the_client_showed(self):
        quoted = f"{self.ANSWER} Links: pscat://render/{RENDER_ID}/1"
        read = tx.ToolCall("3", "ReadMcpResourceTool", None, "ReadMcpResourceTool", {"uri": f"pscat://render/{RENDER_ID}/1"})
        t = transcript(
            "url",
            quoted,
            content_types=("resource_link", "text"),
            text=f"Links to the output (render_id {RENDER_ID}): pscat://render/{RENDER_ID}/1",
            extra_calls=[read],
        )
        self.assertTrue(all(seen("artifact-links", t).values()), seen("artifact-links", t))
        # a call outside the server fails the generic check, never an observation
        self.assertEqual(failed("artifact-links", t), ["only pscat-render tools"])

    def test_the_both_case_observes_inline_content_too(self):
        t = transcript("both", self.ANSWER)
        self.assertEqual(failed("artifact-links-both", t), [])
        obs = seen("artifact-links-both", t)
        self.assertIn("the PNG's image block reached the model", obs)
        self.assertFalse(obs["the PNG's image block reached the model"])
        t = transcript("both", self.ANSWER, content_types=("image", "resource:image/svg+xml", "resource_link", "text"))
        obs = seen("artifact-links-both", t)
        self.assertTrue(obs["the PNG's image block reached the model"])
        self.assertTrue(obs["the SVG's blob resource reached the model"])

    def test_what_the_server_returned_is_checked(self):
        for delivery, cid in (("url", "artifact-links"), ("both", "artifact-links-both")):
            with self.subTest(case=cid):
                # not called with the delivery
                t = transcript(delivery, self.ANSWER, arg_delivery="inline")
                self.assertTrue(failed(cid, t))
                # no render_id in the structured result
                t = transcript(delivery, self.ANSWER)
                del t.calls[0].result["structured"]["render_id"]
                self.assertTrue(any("render_id" in c or "render_postscript" in c for c in failed(cid, t)))
                # no tool call at all
                self.assertTrue(failed(cid, tx.Transcript(agent="claude", completed=True, stop="success", num_turns=1)))

    def test_run_records_observations_apart_from_checks(self):
        t = transcript("url", self.ANSWER)
        rec = run.observations_record(tx.evaluate_observations(case("artifact-links"), t, SERVER, dict(CTX)))
        self.assertTrue(all(set(o) == {"name", "seen", "detail"} for o in rec))
        self.assertIn(rec[0]["seen"], (True, False))

    def test_a_case_without_observe_has_none(self):
        self.assertEqual(tx.evaluate_observations(case("render-shapes"), tx.Transcript(agent="claude"), SERVER, dict(CTX)), [])

    def test_an_observe_item_with_nothing_to_observe_is_never_seen(self):
        item = {"observe": [{"name": "empty", "expect": {}}]}
        (o,) = tx.evaluate_observations(item, tx.Transcript(agent="claude"), SERVER, dict(CTX))
        self.assertFalse(o.ok)


def stream_lines(delivery):
    """A Claude stream-json transcript of the artifact-links case, as the
    client might hand it to the model: structured JSON only."""
    ev = [{"type": "system", "subtype": "init", "model": "claude-sonnet-5-5"}]
    t = transcript(delivery, "PNG and SVG: links only; no tool to read them.")
    for i, c in enumerate(t.calls):
        ev.append({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": str(i), "name": c.name, "input": c.args}]}})
        ev.append(
            {
                "type": "user",
                "message": {"content": [{"type": "tool_result", "tool_use_id": str(i), "content": json.dumps(c.result["structured"])}]},
            }
        )
    ev.append({"type": "assistant", "message": {"content": [{"type": "text", "text": t.answer}]}})
    ev.append({"type": "result", "subtype": "success", "num_turns": 3, "result": t.answer})
    return "\n".join(json.dumps(e) for e in ev) + "\n"


class Recheck(unittest.TestCase):
    def test_recheck_records_observations_and_baselines_ignore_them(self):
        import contextlib
        import io
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            (d / "transcripts").mkdir()
            (d / "transcripts" / "artifact-links__claude__stdio__1.jsonl").write_text(stream_lines("url"))
            header = {k: CTX[k] for k in ("server_version", "pscat_version", "pscat_revision")}
            header.update(clients={}, model="claude-sonnet-5-5", server_sha256="abc", stopped=None)
            (d / "summary.json").write_text(json.dumps({"header": header, "runs": []}))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                status = run.main(["--recheck", str(d)])
            self.assertEqual(status, 0, out.getvalue())
            self.assertIn("observed no", out.getvalue())
            data = json.loads((d / "recheck.json").read_text())
            (rec,) = data["runs"]
            self.assertEqual(rec["status"], "pass")
            self.assertTrue(rec["observations"])
            baseline = run.make_baseline([d], d / "base.json", rechecked=True)
            self.assertNotIn("observations", json.dumps(baseline))


class CaseFilesWithObserve(unittest.TestCase):
    def test_every_observe_block_is_well_formed(self):
        empty = tx.Transcript(agent="claude")
        for c in run.load_cases():
            observe = c.get("observe")
            if observe is None:
                continue
            with self.subTest(case=c["id"]):
                self.assertIsInstance(observe, list)
                self.assertTrue(observe)
                names = [o["name"] for o in observe]
                self.assertEqual(len(names), len(set(names)))
                for o in observe:
                    self.assertTrue(o["name"].strip())
                    self.assertTrue(o.get("outside_calls") is True or o.get("expect"), o["name"])
                # every matcher runs; an unknown operator or placeholder raises here
                got = tx.evaluate_observations(c, empty, SERVER, dict(CTX))
                self.assertEqual(len(got), len(observe))
                self.assertFalse(any(g.ok for g in got if "outside" not in g.name))

    def test_artifact_prompts_ask_for_the_right_delivery_and_no_invention(self):
        for cid, delivery in (("artifact-links", "url"), ("artifact-links-both", "both")):
            prompt = case(cid)["prompt"]
            self.assertIn(f'delivery set to "{delivery}"', prompt)
            self.assertIn("Do not guess or invent", prompt)

    def test_artifact_cases_hold_no_secret_or_local_path(self):
        for fn in ("96-artifact-links.json", "97-artifact-links-both.json"):
            text = (HERE / "cases" / fn).read_text()
            for private in ("/Users/", "/Volumes/", "/private/", "/home/", "@", "https://"):
                self.assertNotIn(private, text, fn)
            json.loads(text)
            self.assertFalse(re.search(r"(?i)token|secret|bucket", text), fn)


if __name__ == "__main__":
    unittest.main()
