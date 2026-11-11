"""Offline tests of the capture-to-fixture converter. No server, no model."""

import json
import unittest

import capture_to_fixture as c

RECORDS = [
    {
        "seq": 1, "unix_ms": 1, "t_ms": 2, "method": "POST", "path": "/mcp", "query": False,
        "headers": {"accept": "application/json, text/event-stream", "content-type": "application/json",
                    "user-agent": "x/1", "host": "127.0.0.1:5555", "mcp-session-id": "abc"},
        "other_header_names": ["connection"],
        "authorization": {"scheme": "Bearer"},
        "body": json.dumps({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}}),
        "body_bytes": 10, "body_truncated": False,
        "response": {"status": 200, "content_type": "application/json"},
    },
    {
        "method": "POST", "path": "/mcp", "headers": {"content-type": "application/json"},
        "body": json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        "body_truncated": False, "response": {"status": 202, "content_type": None},
    },
    {
        "method": "GET", "path": "/mcp", "headers": {"accept": "text/event-stream"}, "body": "",
        "body_truncated": False, "response": {"status": 405, "content_type": None},
    },
]
META = {"id": "t", "client_name": "x", "client_version": "1", "obtained": "real-capture",
        "how": "test", "captured_on": "2026-11-10"}


class ConvertTests(unittest.TestCase):
    def test_drops_host_session_and_timing_and_keeps_the_scheme_only(self):
        fx = c.build_fixture(RECORDS, META, ["the-token"])
        first = fx["requests"][0]
        self.assertNotIn("host", first["headers"])
        self.assertNotIn("mcp-session-id", first["headers"])
        self.assertEqual(first["authorization"], {"scheme": "Bearer"})
        self.assertEqual(first["name"], "initialize")
        self.assertEqual(first["body"]["method"], "initialize")
        for key in ("seq", "unix_ms", "t_ms"):
            self.assertNotIn(key, json.dumps(fx["requests"]))

    def test_names_notifications_and_pins_non_post_status(self):
        fx = c.build_fixture(RECORDS, META, [])
        self.assertEqual(fx["requests"][1]["name"], "notifications/initialized")
        self.assertNotIn("expect", fx["requests"][1])
        self.assertEqual(fx["requests"][2]["expect"], {"status_in": [405]})
        self.assertNotIn("body", fx["requests"][2])

    def test_scrubs_the_tool_use_id(self):
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "t", "_meta": {"claudecode/toolUseId": "toolu_01ABCdef"}}}
        fx = c.build_fixture([dict(RECORDS[0], body=json.dumps(body))], META, [])
        self.assertNotIn("01ABCdef", json.dumps(fx))
        self.assertIn("toolu_scrubbed", json.dumps(fx))

    def test_refuses_a_secret(self):
        rec = dict(RECORDS[0], body=json.dumps({"id": 1, "note": "the-token"}))
        with self.assertRaises(SystemExit):
            c.build_fixture([rec], META, ["the-token"])

    def test_refuses_a_truncated_body(self):
        with self.assertRaises(SystemExit):
            c.build_fixture([dict(RECORDS[0], body_truncated=True)], META, [])


if __name__ == "__main__":
    unittest.main()
