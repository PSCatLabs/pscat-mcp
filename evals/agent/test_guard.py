"""Offline tests for the billing guard. No model, no network:

    python3 -I -m unittest discover -s evals/agent -t evals/agent
"""

import copy
import json
import unittest
from pathlib import Path

import guard
import run

FIXTURE = Path(__file__).parent / "fixtures" / "claude-first-prompt-stdio.jsonl"
SERVER = "pscat-render"


def fixture_events():
    return [json.loads(line) for line in FIXTURE.read_text().splitlines() if line.strip()]


def feed_all(events, model="claude-sonnet-5-5"):
    sg = guard.StreamGuard(SERVER, model)
    for e in events:
        sg.feed(json.dumps(e))
    return sg


def with_init(change):
    events = fixture_events()
    for e in events:
        if e.get("type") == "system" and e.get("subtype") == "init":
            change(e)
    return events


AUTH_OK = {
    "loggedIn": True,
    "authMethod": "claude.ai",
    "apiProvider": "firstParty",
    "email": "someone@example.com",
    "subscriptionType": "max",
}


class ParentEnv(unittest.TestCase):
    def test_clean_environment_passes(self):
        guard.check_parent_env({"HOME": "/h", "PATH": "/bin", "CLAUDECODE": "1"})

    def test_each_forbidden_variable_refuses_even_when_empty(self):
        for name in guard.FORBIDDEN_ENV:
            with self.subTest(name=name):
                with self.assertRaises(guard.GuardError) as cm:
                    guard.check_parent_env({"HOME": "/h", name: ""})
                self.assertIn(name, str(cm.exception))

    def test_issue_variables_are_all_listed(self):
        for name in (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
        ):
            self.assertIn(name, guard.FORBIDDEN_ENV)

    def test_error_names_the_variable_not_its_value(self):
        with self.assertRaises(guard.GuardError) as cm:
            guard.check_parent_env({"ANTHROPIC_API_KEY": "sk-ant-secret"})
        self.assertNotIn("sk-ant-secret", str(cm.exception))


class ChildEnv(unittest.TestCase):
    PARENT = {
        "HOME": "/h",
        "PATH": "/bin",
        "USER": "me",
        "CLAUDECODE": "1",
        "CLAUDE_CODE_ENTRYPOINT": "cli",
        "CLAUDE_CODE_SESSION_ID": "abc",
        "CLAUDE_CODE_MESSAGING_TOKEN": "t",
        "CLAUDE_EFFORT": "high",
        "ANTHROPIC_API_KEY": "sk-ant-x",
        "ANTHROPIC_AUTH_TOKEN": "y",
        "CLAUDE_CODE_USE_BEDROCK": "1",
        "AWS_PROFILE": "p",
    }

    def test_only_allowlisted_variables_survive(self):
        env = guard.child_env(self.PARENT)
        self.assertEqual(env["HOME"], "/h")
        self.assertEqual(env["USER"], "me")  # macOS keychain lookup needs it
        for k in env:
            self.assertTrue(k in guard.ENV_ALLOWLIST or k in ("TERM", "NO_COLOR"), k)
        for k in ("CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_EFFORT", "AWS_PROFILE"):
            self.assertNotIn(k, env)
        for k in guard.FORBIDDEN_ENV:
            self.assertNotIn(k, env)

    def test_extra_cannot_reintroduce_a_forbidden_variable(self):
        with self.assertRaises(guard.GuardError):
            guard.child_env(self.PARENT, {"ANTHROPIC_API_KEY": "sk"})

    def test_extra_is_passed(self):
        env = guard.child_env(self.PARENT, {"PSCAT_EVAL_TOKEN": "tok"})
        self.assertEqual(env["PSCAT_EVAL_TOKEN"], "tok")


class AuthStatus(unittest.TestCase):
    def test_claude_ai_login_passes_and_drops_identity(self):
        summary = guard.check_auth_status(0, json.dumps(AUTH_OK))
        self.assertEqual(summary["authMethod"], "claude.ai")
        self.assertNotIn("email", summary)

    def test_api_key_login_refused(self):
        # What `claude auth status` prints with ANTHROPIC_API_KEY set.
        status = dict(AUTH_OK, authMethod="api_key")
        with self.assertRaises(guard.GuardError):
            guard.check_auth_status(0, json.dumps(status))

    def test_logged_out_refused(self):
        status = dict(AUTH_OK, loggedIn=False, authMethod="none")
        with self.assertRaises(guard.GuardError):
            guard.check_auth_status(1, json.dumps(status))
        with self.assertRaises(guard.GuardError):
            guard.check_auth_status(0, json.dumps(status))

    def test_cloud_provider_refused(self):
        with self.assertRaises(guard.GuardError):
            guard.check_auth_status(0, json.dumps(dict(AUTH_OK, apiProvider="bedrock")))

    def test_missing_field_refused(self):
        status = dict(AUTH_OK)
        del status["authMethod"]
        with self.assertRaises(guard.GuardError):
            guard.check_auth_status(0, json.dumps(status))

    def test_unparseable_output_refused(self):
        for out in ("", "Logged in as someone", "[]", "null"):
            with self.subTest(out=out), self.assertRaises(guard.GuardError):
                guard.check_auth_status(0, out)


class Argv(unittest.TestCase):
    def argv(self, **kw):
        return run.claude_argv("claude", "claude-sonnet-5-5", 8, Path("/tmp/mcp.json"), kw.get("effort"))

    def test_runner_argv_is_isolated_and_safe(self):
        argv = self.argv()
        for flag in (
            "--strict-mcp-config",
            "--tools=",
            "--no-session-persistence",
            "--verbose",
        ):
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "project")
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")
        self.assertEqual(argv[argv.index("--allowedTools") + 1], "mcp__pscat-render__*")
        self.assertEqual(argv[argv.index("--model") + 1], "claude-sonnet-5-5")
        for bad in guard.FORBIDDEN_FLAGS:
            self.assertFalse(any(a == bad or a.startswith(bad + "=") for a in argv), bad)

    def test_forbidden_flags_refused(self):
        for argv in (
            ["claude", "-p", "--bare"],
            ["claude", "--settings", "{}"],
            ["claude", "--settings={}"],
            ["claude", "--fallback-model", "x"],
            ["claude", "--mcp-config", '{"apiKeyHelper": "x"}'],
        ):
            with self.subTest(argv=argv), self.assertRaises(guard.GuardError):
                guard.assert_safe_argv(argv)

    def test_similar_flags_are_not_confused(self):
        guard.assert_safe_argv(["claude", "--setting-sources", "project"])


class Stream(unittest.TestCase):
    def test_recorded_subscription_transcript_passes(self):
        sg = feed_all(fixture_events())
        self.assertEqual(sg.init["apiKeySource"], "none")
        self.assertTrue(sg.result_seen)
        self.assertTrue(sg.rate_limits)

    def test_api_key_source_other_than_none_aborts(self):
        for source in ("ANTHROPIC_API_KEY", "apiKeyHelper", "/login managed key", "", None):
            with self.subTest(source=source), self.assertRaises(guard.GuardError):
                feed_all(with_init(lambda e: e.__setitem__("apiKeySource", source)))

    def test_missing_api_key_source_aborts(self):
        with self.assertRaises(guard.GuardError):
            feed_all(with_init(lambda e: e.pop("apiKeySource")))

    def test_plugin_mcp_server_aborts(self):
        def add(e):
            e["mcp_servers"].append({"name": "plugin:pscat:pscat", "status": "connected"})

        with self.assertRaises(guard.IsolationError):
            feed_all(with_init(add))

    def test_failed_server_aborts(self):
        with self.assertRaises(guard.IsolationError):
            feed_all(with_init(lambda e: e["mcp_servers"][0].__setitem__("status", "failed")))

    def test_foreign_tool_aborts(self):
        for tool in ("Bash", "ToolSearch", "mcp__plugin_pscat_pscat__render_postscript"):
            with self.subTest(tool=tool), self.assertRaises(guard.IsolationError):
                feed_all(with_init(lambda e: e["tools"].append(tool)))

    def test_installed_plugin_aborts(self):
        def add(e):
            e["plugins"].append({"name": "pscat", "path": "/Users/me/.claude/plugins/pscat"})

        with self.assertRaises(guard.IsolationError):
            feed_all(with_init(add))

    def test_model_mismatch_aborts(self):
        with self.assertRaises(guard.GuardError):
            feed_all(fixture_events(), model="claude-opus-5-5")

    def test_event_before_init_aborts(self):
        events = fixture_events()
        first_assistant = next(e for e in events if e["type"] == "assistant")
        with self.assertRaises(guard.GuardError):
            feed_all([first_assistant] + events)

    def test_no_init_then_rate_limit_aborts(self):
        rl = next(e for e in fixture_events() if e["type"] == "rate_limit_event")
        with self.assertRaises(guard.GuardError):
            feed_all([rl])

    def test_non_json_line_aborts(self):
        sg = guard.StreamGuard(SERVER)
        with self.assertRaises(guard.GuardError):
            sg.feed("Error: something went wrong")

    def rate_limit_variant(self, change):
        events = copy.deepcopy(fixture_events())
        for e in events:
            if e["type"] == "rate_limit_event":
                change(e["rate_limit_info"])
        return events

    def test_overage_aborts(self):
        with self.assertRaises(guard.GuardError):
            feed_all(self.rate_limit_variant(lambda i: i.__setitem__("isUsingOverage", True)))

    def test_missing_overage_flag_aborts(self):
        with self.assertRaises(guard.GuardError):
            feed_all(self.rate_limit_variant(lambda i: i.pop("isUsingOverage")))

    def test_rate_limited_stops(self):
        with self.assertRaises(guard.RateLimited):
            feed_all(self.rate_limit_variant(lambda i: i.__setitem__("status", "rejected")))

    def test_non_first_party_usage_aborts(self):
        events = fixture_events()
        for e in events:
            if e["type"] == "result":
                for u in e["modelUsage"].values():
                    u["provider"] = "bedrock"
        with self.assertRaises(guard.GuardError):
            feed_all(events)

    def test_list_price_cost_is_not_a_refusal(self):
        # Subscription sessions report a nonzero list-price estimate.
        result = next(e for e in fixture_events() if e["type"] == "result")
        self.assertGreater(result["total_cost_usd"], 0)
        feed_all(fixture_events())


if __name__ == "__main__":
    unittest.main()
