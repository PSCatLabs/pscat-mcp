"""Offline tests of scripts/oauth-check.py. Synthetic, unsigned tokens; no network."""

import base64
import importlib.util
import io
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "oauth-check.py"
_spec = importlib.util.spec_from_file_location("oauth_check", SCRIPT)
oc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(oc)

ISSUER = "https://auth.example.com"
RESOURCE = "https://mcp.example.com/mcp"
NOW = 1_800_000_000
SIGNATURE = "SignatureMarker-9f3a"  # distinctive, to look for in the output


def b64(obj):
    raw = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def claims(**over):
    c = {"iss": ISSUER, "aud": [RESOURCE], "sub": "user_01", "exp": NOW + 3600,
         "email": "Alice@Example.com", "email_verified": True}
    c.update(over)
    return {k: v for k, v in c.items() if v is not DROP}


def header(**over):
    h = {"alg": "RS256", "kid": "key-1", "typ": "JWT"}
    h.update(over)
    return {k: v for k, v in h.items() if v is not DROP}


DROP = object()


def token(c=None, h=None, sig=SIGNATURE):
    return "%s.%s.%s" % (b64(h if h is not None else header()),
                         b64(c if c is not None else claims()), sig)


def run(tok=None, allow=(), jwks=None, scopes=(), **kw):
    return oc.check_token(tok if tok is not None else token(), ISSUER, RESOURCE,
                          [oc.parse_allow_user(a) for a in allow], jwks, scopes, now=NOW, **kw)


def get(results, name):
    return [r for r in results if r.name == name][0]


def failed(results):
    return {r.name for r in results if r.status == oc.FAIL}


def rsa_jwks(kid="key-1", **extra):
    n = base64.urlsafe_b64encode(b"\xc0" + b"\x01" * 255).rstrip(b"=").decode()  # 2048 bits
    key = {"kty": "RSA", "kid": kid, "n": n, "e": "AQAB"}
    key.update(extra)
    return {"keys": [key]}


class GoodToken(unittest.TestCase):
    def test_every_check_passes(self):
        r = run(allow=["email:alice@example.com"], jwks=rsa_jwks())
        self.assertEqual(failed(r), set(), r)
        self.assertEqual(get(r, "allowlist").status, oc.PASS)
        self.assertEqual(get(r, "jwks").status, oc.PASS)

    def test_checks_that_need_input_are_skipped_not_failed(self):
        r = run()
        self.assertEqual(failed(r), set())
        self.assertEqual(get(r, "jwks").status, oc.SKIP)
        self.assertEqual(get(r, "allowlist").status, oc.SKIP)


class Issuer(unittest.TestCase):
    def test_wrong_issuer(self):
        r = run(token(claims(iss="https://other.example.com")))
        self.assertEqual(failed(r), {"iss"})
        self.assertEqual(get(r, "iss").reason, "wrong_issuer")

    def test_trailing_slash_differs(self):
        r = run(token(claims(iss=ISSUER + "/")))
        self.assertIn("trailing slash", get(r, "iss").detail)
        self.assertEqual(failed(r), {"iss"})


class Audience(unittest.TestCase):
    def test_string_and_array_both_pass(self):
        self.assertEqual(failed(run(token(claims(aud=RESOURCE)))), set())
        self.assertEqual(failed(run(token(claims(aud=["x", RESOURCE])))), set())

    def test_missing_the_resource(self):
        for aud in ("https://other.example.com/mcp", ["a", "b"], [], DROP, 5, [5, None]):
            r = run(token(claims(aud=aud)))
            self.assertEqual(failed(r), {"aud"}, aud)
            self.assertEqual(get(r, "aud").reason, "wrong_audience")

    def test_one_trailing_slash_is_ignored_and_case_is_not(self):
        self.assertEqual(failed(run(token(claims(aud=RESOURCE + "/")))), set())
        self.assertEqual(failed(run(token(claims(aud=RESOURCE.upper())))), {"aud"})


class Times(unittest.TestCase):
    def test_expired(self):
        r = run(token(claims(exp=NOW - 3600)))
        self.assertEqual(failed(r), {"exp"})
        self.assertEqual(get(r, "exp").reason, "expired")

    def test_inside_the_leeway_passes_and_says_so(self):
        r = run(token(claims(exp=NOW - 30)))
        self.assertEqual(failed(r), set())
        self.assertIn("leeway", get(r, "exp").detail)

    def test_leeway_is_adjustable(self):
        self.assertEqual(failed(run(token(claims(exp=NOW - 30)), leeway=0)), {"exp"})

    def test_missing_or_non_numeric_exp(self):
        for exp in (DROP, "9999999999", True, None):
            self.assertEqual(failed(run(token(claims(exp=exp)))), {"exp"}, exp)

    def test_nbf_in_the_future(self):
        r = run(token(claims(nbf=NOW + 3600)))
        self.assertEqual(failed(r), {"nbf"})
        self.assertEqual(get(r, "nbf").reason, "not_yet_valid")

    def test_nbf_null_or_past_passes(self):
        self.assertEqual(failed(run(token(claims(nbf=None)))), set())
        self.assertEqual(failed(run(token(claims(nbf=NOW - 5)))), set())
        self.assertEqual(failed(run(token(claims(nbf="soon")))), {"nbf"})


class Header(unittest.TestCase):
    def test_missing_kid(self):
        r = run(token(h=header(kid=DROP)))
        self.assertEqual(failed(r), {"kid"})
        self.assertEqual(get(r, "kid").reason, "missing_kid")
        self.assertEqual(failed(run(token(h=header(kid="")))), {"kid"})

    def test_alg_none_and_hmac_are_refused(self):
        for alg in ("none", "HS256", "ES512", "rs256", None):
            r = run(token(h=header(alg=alg)))
            self.assertIn("alg", failed(r), alg)
            self.assertEqual(get(r, "alg").reason, "unsupported_alg")

    def test_every_supported_alg_passes(self):
        for alg in oc.SUPPORTED_ALGS:
            self.assertEqual(failed(run(token(h=header(alg=alg)))), set(), alg)

    def test_crit_present_even_if_null(self):
        self.assertEqual(failed(run(token(h=header(crit=["exp"])))), {"crit"})
        self.assertEqual(failed(run(token(h=header(crit=None)))), {"crit"})


class Shape(unittest.TestCase):
    def test_not_a_jwt(self):
        for t in ("opaque-token", "a.b", "a.b.c.d", "%s.%s.%s" % (b64([1]), b64(claims()), "x"),
                  "%s.%s.%s" % (b64(header()), b64(claims()), "pad=")):
            r = run(t)
            self.assertEqual(get(r, "shape").reason, "not_a_jwt", t)
            self.assertEqual(r[-1].name, "shape")  # nothing else is judged

    def test_padded_segment_is_refused_like_the_server(self):
        padded = base64.urlsafe_b64encode(json.dumps(header()).encode()).decode()
        if "=" in padded:
            self.assertEqual(get(run("%s.%s.s" % (padded, b64(claims()))), "shape").status, oc.FAIL)

    def test_too_large(self):
        r = run(token(claims(pad="x" * 9000)))
        self.assertEqual(get(r, "shape").reason, "token_too_large")

    def test_missing_subject(self):
        for sub in (DROP, "", 7):
            self.assertEqual(failed(run(token(claims(sub=sub)))), {"sub"}, sub)


class EmailVerified(unittest.TestCase):
    def test_string_true_fails_with_an_email_entry(self):
        r = run(token(claims(email_verified="true")), allow=["email:alice@example.com"])
        self.assertIn("email_verified", failed(r))
        self.assertEqual(get(r, "email_verified").reason, "not_allowed")
        self.assertIn("allowlist", failed(r))

    def test_string_true_fails_without_an_allowlist_too(self):
        r = run(token(claims(email_verified="true")))
        self.assertEqual(failed(r), {"email_verified"})

    def test_number_one_is_not_a_boolean(self):
        r = run(token(claims(email_verified=1)), allow=["email:alice@example.com"])
        self.assertIn("email_verified", failed(r))

    def test_a_matching_sub_entry_makes_it_a_note(self):
        r = run(token(claims(email_verified="true")), allow=["sub:user_01", "email:alice@example.com"])
        self.assertEqual(failed(r), set())
        self.assertEqual(get(r, "email_verified").status, oc.INFO)

    def test_false_is_a_boolean_but_does_not_match(self):
        r = run(token(claims(email_verified=False)), allow=["email:alice@example.com"])
        self.assertEqual(get(r, "email_verified").status, oc.PASS)
        self.assertEqual(failed(r), {"allowlist"})

    def test_missing_email_for_an_email_entry(self):
        r = run(token(claims(email=DROP, email_verified=DROP)), allow=["email:alice@example.com"])
        self.assertTrue({"email", "email_verified", "allowlist"} <= failed(r))


class Allowlist(unittest.TestCase):
    def test_email_entry_matches_case_insensitively(self):
        r = run(allow=["email: ALICE@example.com "])
        self.assertEqual(get(r, "allowlist").status, oc.PASS)
        self.assertIn("entry 1 (email:)", get(r, "allowlist").detail)

    def test_email_entry_does_not_match(self):
        r = run(allow=["email:bob@example.com"])
        self.assertEqual(failed(r), {"allowlist"})
        self.assertEqual(get(r, "allowlist").reason, "not_allowed")

    def test_sub_entry_is_exact(self):
        self.assertEqual(get(run(allow=["sub:user_01"]), "allowlist").status, oc.PASS)
        self.assertEqual(get(run(allow=["sub:USER_01"]), "allowlist").status, oc.FAIL)

    def test_which_entries_match(self):
        r = run(allow=["email:bob@example.com", "sub:user_01", "email:alice@example.com"])
        self.assertIn("entry 2 (sub:), entry 3 (email:)", get(r, "allowlist").detail)

    def test_entries_are_parsed_like_the_server(self):
        self.assertEqual(oc.parse_allow_user("email:A@B.com"), ("email", "a@b.com"))
        self.assertEqual(oc.parse_allow_user(" sub: x "), ("sub", "x"))
        for bad in ("alice@example.com", "email:nobody", "sub:", "user:x", ""):
            self.assertIsNone(oc.parse_allow_user(bad), bad)

    def test_lowercasing_is_ascii_only_like_the_server(self):
        self.assertEqual(oc.parse_allow_user("email:\u00c9@example.com"),
                         ("email", "\u00c9@example.com"))
        r = run(token(claims(email="\u00e9@example.com")), allow=["email:\u00c9@example.com"])
        self.assertEqual(get(r, "allowlist").status, oc.FAIL)

    def test_the_output_does_not_print_the_person(self):
        text = "\n".join(map(repr, run(allow=["email:alice@example.com"])))
        for secret in ("alice", "user_01"):
            self.assertNotIn(secret, text.lower())


class Scopes(unittest.TestCase):
    def test_scope_and_scp(self):
        self.assertEqual(failed(run(token(claims(scope="openid mcp")), scopes=("mcp",))), set())
        self.assertEqual(failed(run(token(claims(scp=["mcp"])), scopes=("mcp",))), set())
        r = run(token(claims(scope="openid")), scopes=("mcp",))
        self.assertEqual(get(r, "scope").reason, "insufficient_scope")


class Jwks(unittest.TestCase):
    def test_kid_missing_from_the_jwks(self):
        r = run(jwks=rsa_jwks(kid="other"))
        self.assertEqual(failed(r), {"jwks"})
        self.assertEqual(get(r, "jwks").reason, "unknown_kid")

    def test_keys_the_server_skips_do_not_count(self):
        self.assertEqual(failed(run(jwks=rsa_jwks(use="enc"))), {"jwks"})
        self.assertEqual(get(run(jwks={"keys": []}), "jwks").reason, "unknown_kid")
        self.assertEqual(get(run(jwks={}), "jwks").reason, "unknown_kid")

    def test_key_that_cannot_verify_the_alg(self):
        r = run(jwks=rsa_jwks(alg="PS256"))
        self.assertEqual(get(r, "jwks").reason, "bad_signature")
        r = run(token(h=header(alg="ES256")), jwks=rsa_jwks())
        self.assertEqual(get(r, "jwks").reason, "bad_signature")

    def test_ec_and_okp_keys(self):
        x = b64(b"\x01" * 32)
        ec = {"keys": [{"kty": "EC", "crv": "P-256", "kid": "key-1", "x": x, "y": x}]}
        self.assertEqual(failed(run(token(h=header(alg="ES256")), jwks=ec)), set())
        self.assertEqual(failed(run(token(h=header(alg="ES384")), jwks=ec)), {"jwks"})
        okp = {"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "key-1", "x": x}]}
        self.assertEqual(failed(run(token(h=header(alg="EdDSA")), jwks=okp)), set())

    def test_short_rsa_key(self):
        n = b64(b"\xc0" + b"\x01" * 127)  # 1024 bits
        r = run(jwks={"keys": [{"kty": "RSA", "kid": "key-1", "n": n, "e": "AQAB"}]})
        self.assertEqual(get(r, "jwks").reason, "bad_signature")


class Config(unittest.TestCase):
    def test_plain_http_issuer_is_refused(self):
        r = oc.check_token(token(), "http://auth.example.com", RESOURCE, now=NOW)
        self.assertEqual(get(r, "config").status, oc.FAIL)

    def test_loopback_http_is_fine(self):
        r = oc.check_token(token(), "http://127.0.0.1:9000", RESOURCE, now=NOW)
        self.assertEqual(get(r, "config").status, oc.PASS)

    def test_discovery_urls_follow_the_server_order(self):
        self.assertEqual(oc.discovery_urls("https://a.example.com/"), [
            "https://a.example.com/.well-known/oauth-authorization-server",
            "https://a.example.com/.well-known/openid-configuration"])
        self.assertEqual(oc.discovery_urls("https://a.example.com/tenant"), [
            "https://a.example.com/.well-known/oauth-authorization-server/tenant",
            "https://a.example.com/.well-known/openid-configuration/tenant",
            "https://a.example.com/tenant/.well-known/openid-configuration"])


class FetchJwks(unittest.TestCase):
    """Discovery against a throwaway loopback server: no outside network."""

    def serve(self, issuer_override=None):
        import http.server
        import threading
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                outer.paths.append(self.path)
                base = "http://127.0.0.1:%d" % outer.port
                if self.path == "/.well-known/oauth-authorization-server":
                    body = {"issuer": issuer_override or base, "jwks_uri": base + "/jwks"}
                elif self.path == "/jwks":
                    body = rsa_jwks()
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                data = json.dumps(body).encode()
                self.send_response(200)
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a):
                pass

        srv = http.server.HTTPServer(("127.0.0.1", 0), H)
        self.port = srv.server_address[1]
        self.paths = []
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        return "http://127.0.0.1:%d" % self.port

    def test_discovers_and_fetches(self):
        issuer = self.serve()
        said = []
        doc, err = oc.fetch_jwks(issuer, None, 5, said.append)
        self.assertIsNone(err)
        self.assertIn("key-1", oc.usable_keys(doc))
        self.assertEqual(self.paths, ["/.well-known/oauth-authorization-server", "/jwks"])
        self.assertEqual(len(said), 2)
        self.assertTrue(all(m.startswith("GET http://127.0.0.1:") for m in said))

    def test_a_failed_fetch_is_named_in_the_jwks_row(self):
        r = oc.check_token(token(), ISSUER, RESOURCE, now=NOW, jwks_note="x")
        self.assertIn("could not be fetched", get(r, "jwks").detail)

    def test_a_mismatched_issuer_is_refused(self):
        issuer = self.serve(issuer_override="https://other.example.com")
        doc, err = oc.fetch_jwks(issuer, None, 5, lambda m: None)
        self.assertIsNone(doc)
        self.assertIn("another issuer", err)

    def test_an_unreachable_issuer_is_an_error_not_a_crash(self):
        doc, err = oc.fetch_jwks("http://127.0.0.1:9", None, 1, lambda m: None)
        self.assertIsNone(doc)
        self.assertTrue(err)


class Cli(unittest.TestCase):
    def run_cli(self, env_value, *extra, env_name="OAUTH_CHECK_TOKEN", jwks=None):
        env = {"PATH": os.environ.get("PATH", "")}
        if env_value is not None:
            env[env_name] = env_value
        cmd = [sys.executable, "-I", str(SCRIPT), "--issuer", ISSUER, "--resource", RESOURCE]
        return subprocess.run(cmd + list(extra), env=env, capture_output=True, text=True, timeout=30)

    def future(self):
        return token(claims(exp=4_000_000_000))

    def test_exit_zero_only_when_nothing_failed(self):
        ok = self.run_cli(self.future())
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertIn("RESULT: PASS", ok.stdout)
        bad = self.run_cli(token(claims(exp=1)))
        self.assertEqual(bad.returncode, 1)
        self.assertIn("reason=expired", bad.stdout)

    def test_says_the_signature_is_not_verified(self):
        self.assertIn("NOT CHECKED: the signature", self.run_cli(self.future()).stdout)

    def test_never_prints_the_token_or_the_signature(self):
        tok = token(claims(exp=1))
        for p in (self.run_cli(tok), self.run_cli(tok, "--allow-user", "email:alice@example.com"),
                  self.run_cli("not.a.jwt-" + SIGNATURE)):
            for stream in (p.stdout, p.stderr):
                self.assertNotIn(SIGNATURE, stream)
                self.assertNotIn(tok, stream)
                self.assertNotIn(tok.split(".")[1], stream)

    def test_token_comes_from_the_named_variable_only(self):
        self.assertEqual(self.run_cli(self.future(), env_name="MY_TOKEN").returncode, 2)
        p = self.run_cli(self.future(), "--token-env", "MY_TOKEN", env_name="MY_TOKEN")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.run_cli(None).returncode, 2)
        self.assertEqual(self.run_cli("").returncode, 2)

    def test_a_token_on_the_command_line_is_not_accepted(self):
        p = self.run_cli(None, "--token", self.future())
        self.assertEqual(p.returncode, 2)
        self.assertNotIn(self.future(), p.stdout + p.stderr)

    def test_malformed_allow_user_is_not_echoed(self):
        p = self.run_cli(self.future(), "--allow-user", "alice@example.com")
        self.assertEqual(p.returncode, 2)
        self.assertNotIn("alice", p.stdout + p.stderr)

    def test_jwks_file_and_allowlist(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "jwks.json")
            with open(path, "w") as f:
                json.dump(rsa_jwks(), f)
            p = self.run_cli(self.future(), "--jwks-file", path, "--allow-user",
                             "email:alice@example.com")
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("would match entry 1 (email:)", p.stdout)
            with open(path, "w") as f:
                json.dump(rsa_jwks(kid="other"), f)
            p = self.run_cli(self.future(), "--jwks-file", path)
            self.assertEqual(p.returncode, 1)
            self.assertIn("reason=unknown_kid", p.stdout)

    def test_both_jwks_sources_are_a_usage_error(self):
        p = self.run_cli(self.future(), "--jwks-file", "x", "--fetch-jwks")
        self.assertEqual(p.returncode, 2)


if __name__ == "__main__":
    unittest.main()
