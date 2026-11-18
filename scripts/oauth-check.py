#!/usr/bin/env python3
"""Check an OAuth access token against the rules of pscat-mcp's `oauth` method.

Prints a pass/fail checklist that mirrors `src/oauth.rs`, so a failed sign-in
can be diagnosed before any client is involved. Standard library only; run it
as `python3 -I scripts/oauth-check.py ...`.

This does NOT verify the signature. A token that passes every check here can
still be refused by the server (bad_signature). The checklist says so.

The token is read from an environment variable (default OAUTH_CHECK_TOKEN,
change it with --token-env), never from the command line, and neither the
token nor any part of it, the signature included, is printed. The claims that
identify a person (`sub`, `email`) are not printed either, only whether they
are present, their type and whether an allowlist entry would match, so the
output can be pasted into an issue.

Exit status: 0 when no check failed, 1 when one failed, 2 for a usage error.
"""

import argparse
import base64
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# Mirrors of the constants in src/oauth.rs.
MAX_TOKEN_BYTES = 8 * 1024
MAX_DOCUMENT_BYTES = 256 * 1024
DEFAULT_LEEWAY_SECS = 60
SUPPORTED_ALGS = (
    "RS256", "RS384", "RS512",
    "PS256", "PS384", "PS512",
    "ES256", "ES384",
    "EdDSA",
)
RSA_ALGS = ("RS256", "RS384", "RS512", "PS256", "PS384", "PS512")
RSA_MIN_BITS, RSA_MAX_BITS = 2048, 8192

PASS, FAIL, SKIP, INFO = "PASS", "FAIL", "SKIP", "INFO"

# The server's `reason` values in the order it checks them.
ORDER = ("not_a_jwt", "token_too_large", "unsupported_alg", "missing_kid", "unknown_kid",
         "bad_signature", "wrong_issuer", "wrong_audience", "expired", "not_yet_valid",
         "missing_subject", "insufficient_scope", "not_allowed")

_B64URL = re.compile(r"^[A-Za-z0-9_-]*$")


class Result:
    """One line of the checklist. `reason` is the server's log `reason`."""

    def __init__(self, status, name, detail, reason=None):
        self.status = status
        self.name = name
        self.detail = detail
        self.reason = reason

    def __repr__(self):
        return "Result(%r, %r, %r, %r)" % (self.status, self.name, self.detail, self.reason)


# ---------------------------------------------------------------- decoding

def b64url_decode(segment):
    """Strict base64url without padding, as the server's URL_SAFE_NO_PAD."""
    if not _B64URL.match(segment) or len(segment) % 4 == 1:
        return None
    try:
        return base64.urlsafe_b64decode(segment + "=" * (-len(segment) % 4))
    except (ValueError, TypeError):
        return None


def json_segment(segment):
    raw = b64url_decode(segment)
    if raw is None:
        return None
    try:
        return json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return None


def is_number(v):
    """A JSON number the server reads with as_f64: not a bool, finite."""
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


def ascii_lower(s):
    """Lowercase ASCII letters only, as the server's to_ascii_lowercase."""
    return "".join(c.lower() if "A" <= c <= "Z" else c for c in s)


def trim_slash(s):
    return s[:-1] if s.endswith("/") else s


def https_or_loopback(url):
    p = urllib.parse.urlsplit(url)
    if not p.hostname or "#" in url:
        return False
    if p.scheme == "https":
        return True
    return p.scheme == "http" and p.hostname in ("localhost", "127.0.0.1", "::1")


# --------------------------------------------------------------- allowlist

def parse_allow_user(entry):
    """('sub'|'email', value) as `load_allowlist` reads it, or None if malformed."""
    entry = entry.strip()
    if entry.startswith("sub:"):
        value = entry[4:].strip()
        return ("sub", value) if value else None
    if entry.startswith("email:"):
        value = entry[6:].strip()
        return ("email", ascii_lower(value)) if "@" in value else None
    return None


# -------------------------------------------------------------------- JWKS

def usable_keys(jwks):
    """The keys the server would keep, by kid: a mirror of `parse_jwks`.

    The server skips keys without a kid, with a `use` other than `sig`, of an
    unknown type or curve, or malformed. A kid whose key is skipped is
    `unknown_kid` to the server.
    """
    out = {}
    keys = jwks.get("keys") if isinstance(jwks, dict) else None
    if not isinstance(keys, list):
        return out
    for k in keys:
        if not isinstance(k, dict) or not isinstance(k.get("kid"), str):
            continue
        if "use" in k and k["use"] != "sig":
            continue
        kty, crv = k.get("kty"), k.get("crv")

        def field(name):
            v = k.get(name)
            return b64url_decode(v) if isinstance(v, str) else None

        if kty == "RSA":
            n, e = field("n"), field("e")
            if n is None or e is None:
                continue
            n = n.lstrip(b"\0")
            kind, size = "RSA", len(n) * 8 - (8 - n[0].bit_length() if n else 0)
        elif kty == "EC" and crv in ("P-256", "P-384"):
            ln = 32 if crv == "P-256" else 48
            x, y = field("x"), field("y")
            if x is None or y is None or len(x) != ln or len(y) != ln:
                continue
            kind, size = crv, None
        elif kty == "OKP" and crv == "Ed25519":
            x = field("x")
            if x is None or len(x) != 32:
                continue
            kind, size = "Ed25519", None
        else:
            continue
        alg = k.get("alg") if isinstance(k.get("alg"), str) else None
        out.setdefault(k["kid"], {"kind": kind, "bits": size, "alg": alg})
    return out


def key_fits(key, alg):
    """Why a key cannot verify `alg` (the server then says bad_signature), or None."""
    if key["alg"] is not None and key["alg"] != alg:
        return "the key states alg %s, the token header says %s" % (key["alg"], alg)
    kind = key["kind"]
    if kind == "RSA":
        if alg not in RSA_ALGS:
            return "an RSA key cannot verify %s" % alg
        if not RSA_MIN_BITS <= key["bits"] <= RSA_MAX_BITS:
            return "the RSA key has %d bits; the server accepts %d to %d" % (
                key["bits"], RSA_MIN_BITS, RSA_MAX_BITS)
        return None
    want = {"P-256": "ES256", "P-384": "ES384", "Ed25519": "EdDSA"}[kind]
    if alg != want:
        return "a %s key verifies only %s, not %s" % (kind, want, alg)
    return None


# ------------------------------------------------------------------ checks

def check_config(issuer, resource):
    bad = [n for n, u in (("issuer", issuer), ("resource", resource)) if not https_or_loopback(u)]
    if bad:
        return Result(FAIL, "config", "%s must be an https URL (http only for loopback), no fragment"
                      % " and ".join(bad))
    if "?" in issuer or "?" in resource:
        return Result(FAIL, "config", "issuer and resource must not contain a query")
    return Result(PASS, "config", "issuer and resource are acceptable URLs")


def check_token(token, issuer, resource, allow=(), jwks=None, scopes=(),
                now=None, leeway=DEFAULT_LEEWAY_SECS, jwks_note=None):
    """Run every check; returns a list of Result in the server's order.

    `allow` is a list of parsed entries (see `parse_allow_user`), `jwks` a
    decoded JWKS document or None, `scopes` the required scopes, `now` the
    time in seconds (default: the clock).
    """
    now = time.time() if now is None else now
    out = [check_config(issuer, resource)]

    # --- shape (not_a_jwt, token_too_large)
    if len(token) > MAX_TOKEN_BYTES:
        out.append(Result(FAIL, "shape", "the token is over %d bytes" % MAX_TOKEN_BYTES,
                          "token_too_large"))
        return out
    parts = token.split(".")
    header = claims = None
    if len(parts) == 3:
        header, claims = json_segment(parts[0]), json_segment(parts[1])
        if b64url_decode(parts[2]) is None:
            header = None
    if not isinstance(header, dict) or not isinstance(claims, dict):
        out.append(Result(FAIL, "shape",
                          "not a JWT: need three base64url segments (no padding), a JSON object "
                          "header and a JSON object payload. An opaque token cannot be used with "
                          "`oauth`", "not_a_jwt"))
        return out
    out.append(Result(PASS, "shape", "three base64url segments, JSON header and payload"))

    # --- alg, crit, kid
    alg = header.get("alg")
    if alg in SUPPORTED_ALGS:
        out.append(Result(PASS, "alg", "alg %s is supported" % alg))
    else:
        shown = alg if isinstance(alg, str) and re.match(r"^[A-Za-z0-9_-]{1,12}$", alg) else "(unusable)"
        why = "none and HS* are refused" if shown == "none" or str(shown).startswith("HS") else \
            "supported: " + ", ".join(SUPPORTED_ALGS)
        out.append(Result(FAIL, "alg", "alg %s is not accepted (%s)" % (shown, why),
                          "unsupported_alg"))
    if "crit" in header:
        out.append(Result(FAIL, "crit", "the header has a `crit` member; the server refuses any",
                          "unsupported_alg"))
    else:
        out.append(Result(PASS, "crit", "no `crit` in the header"))
    kid = header.get("kid")
    kid_ok = isinstance(kid, str) and kid != ""
    if kid_ok:
        out.append(Result(PASS, "kid", "the header has a kid"))
    else:
        out.append(Result(FAIL, "kid", "the header has no kid (a non-empty string is required)",
                          "missing_kid"))

    # --- the kid in the JWKS (unknown_kid) and whether the key fits (bad_signature)
    if jwks is None:
        out.append(Result(SKIP, "jwks", "the JWKS could not be fetched (see jwks-fetch)" if jwks_note
                          else "no JWKS given (--jwks-file or --fetch-jwks)"))
    elif not kid_ok:
        out.append(Result(SKIP, "jwks", "no kid to look up"))
    else:
        keys = usable_keys(jwks)
        if kid not in keys:
            out.append(Result(FAIL, "jwks", "no usable signing key with this kid in the JWKS (%d usable "
                              "key(s); keys with another `use`, an unknown type or no kid are "
                              "skipped by the server)" % len(keys), "unknown_kid"))
        else:
            why = key_fits(keys[kid], alg) if alg in SUPPORTED_ALGS else None
            if why:
                out.append(Result(FAIL, "jwks", "the kid is in the JWKS but " + why, "bad_signature"))
            else:
                out.append(Result(PASS, "jwks", "the kid names a usable %s key" % keys[kid]["kind"]))

    # --- iss (wrong_issuer): exact string equality
    iss = claims.get("iss")
    if iss == issuer:
        out.append(Result(PASS, "iss", "iss equals the issuer exactly"))
    else:
        hint = ""
        if isinstance(iss, str) and trim_slash(iss) == trim_slash(issuer):
            hint = " (differs only by a trailing slash; the server compares exactly)"
        elif not isinstance(iss, str):
            hint = " (iss is missing or not a string)"
        out.append(Result(FAIL, "iss", "iss does not equal --issuer" + hint, "wrong_issuer"))

    # --- aud (wrong_audience): string or array, one trailing slash ignored, case-sensitive
    want = trim_slash(resource)
    aud = claims.get("aud")
    if isinstance(aud, str):
        shape, match = "a string", trim_slash(aud) == want
    elif isinstance(aud, list):
        shape = "an array of %d" % len(aud)
        match = any(isinstance(a, str) and trim_slash(a) == want for a in aud)
    else:
        shape, match = "missing or not a string/array", False
    if match:
        out.append(Result(PASS, "aud", "aud (%s) contains the resource" % shape))
    else:
        out.append(Result(FAIL, "aud", "aud (%s) does not contain --resource (compared case-sensitively, "
                          "one trailing slash ignored). A client that sent no `resource` parameter "
                          "gets the provider's default audience" % shape, "wrong_audience"))

    # --- exp (expired)
    exp = claims.get("exp")
    if not is_number(exp):
        out.append(Result(FAIL, "exp", "exp is missing or not a number", "expired"))
    elif now <= exp + leeway:
        left = int(exp - now)
        note = "expires in %d s" % left if left >= 0 else \
            "expired %d s ago, still inside the %d s leeway" % (-left, leeway)
        out.append(Result(PASS, "exp", "%s (server leeway %d s, PSCAT_MCP_OAUTH_LEEWAY_SECS)"
                          % (note, leeway)))
    else:
        out.append(Result(FAIL, "exp", "expired %d s ago (server leeway %d s)" % (int(now - exp), leeway),
                          "expired"))

    # --- nbf (not_yet_valid): absent or null is fine
    if "nbf" not in claims or claims["nbf"] is None:
        out.append(Result(PASS, "nbf", "no nbf"))
    else:
        nbf = claims["nbf"]
        if is_number(nbf) and nbf <= now + leeway:
            out.append(Result(PASS, "nbf", "nbf is not in the future (leeway %d s)" % leeway))
        elif is_number(nbf):
            out.append(Result(FAIL, "nbf", "nbf is %d s in the future (server leeway %d s)"
                              % (int(nbf - now), leeway), "not_yet_valid"))
        else:
            out.append(Result(FAIL, "nbf", "nbf is not a number", "not_yet_valid"))

    # --- sub (missing_subject)
    sub = claims.get("sub")
    if isinstance(sub, str) and sub:
        out.append(Result(PASS, "sub", "sub is a non-empty string"))
    else:
        out.append(Result(FAIL, "sub", "sub is missing, empty or not a string", "missing_subject"))

    # --- scopes (insufficient_scope)
    if scopes:
        granted = set()
        for name in ("scope", "scp"):
            v = claims.get(name)
            if isinstance(v, str):
                granted.update(v.split())
            elif isinstance(v, list):
                granted.update(s for s in v if isinstance(s, str))
        missing = [s for s in scopes if s not in granted]
        if missing:
            out.append(Result(FAIL, "scope", "%d required scope(s) not granted in `scope`/`scp`"
                              % len(missing), "insufficient_scope"))
        else:
            out.append(Result(PASS, "scope", "every required scope is granted"))
    else:
        out.append(Result(SKIP, "scope", "no --scope given (the server's default: no scope check)"))

    # --- email and email_verified. Only `email:` entries depend on them, so
    # they fail the check when such an entry is the way in (or when no
    # allowlist was given to tell); a matching `sub:` entry makes them notes.
    email = claims.get("email")
    ev = claims.get("email_verified", None)
    email_entries = [a for a in allow if a[0] == "email"]
    sub_matched = isinstance(sub, str) and any(k == "sub" and v == sub for k, v in allow)
    relevant = (bool(email_entries) and not sub_matched) or not allow
    bad = FAIL if relevant else INFO
    why = "not_allowed" if relevant and email_entries else None
    if "email_verified" in claims and not isinstance(ev, bool):
        kind = {str: "a string", int: "a number", float: "a number", list: "an array",
                dict: "an object", type(None): "null"}.get(type(ev), "not a boolean")
        out.append(Result(bad, "email_verified", "email_verified is %s; the server only counts a JSON "
                          "boolean true, so every `email:` entry fails with not_allowed. Fix the "
                          "JWT template, or use `sub:` entries" % kind, why))
    elif isinstance(ev, bool):
        out.append(Result(PASS, "email_verified", "email_verified is a JSON boolean (%s)"
                          % ("true" if ev else "false")))
    elif email is not None or email_entries:
        out.append(Result(bad, "email_verified", "email_verified is missing from the access token; "
                          "`email:` entries need it (a JSON boolean true)", why))
    else:
        out.append(Result(SKIP, "email_verified", "no email claims and no `email:` entries"))
    if email_entries and not isinstance(email, str):
        out.append(Result(bad, "email", "`email:` entries are used but the token has no `email` "
                          "string; add it to the JWT template or use `sub:` entries", why))
    elif isinstance(email, str):
        out.append(Result(PASS, "email", "the token has an email claim (value not printed)"))

    # --- allowlist (not_allowed)
    if not allow:
        out.append(Result(SKIP, "allowlist", "no --allow-user given"))
    else:
        matched = []
        for i, (kind, value) in enumerate(allow, 1):
            if kind == "sub" and isinstance(sub, str) and sub == value:
                matched.append("entry %d (sub:)" % i)
            elif (kind == "email" and ev is True and isinstance(email, str)
                  and ascii_lower(email) == value):
                matched.append("entry %d (email:)" % i)
        if matched:
            out.append(Result(PASS, "allowlist", "would match " + ", ".join(matched)))
        else:
            out.append(Result(FAIL, "allowlist", "no entry of %d matches: the server answers 403 "
                              "not_allowed" % len(allow), "not_allowed"))
    return out


# ------------------------------------------------------------- JWKS fetch

class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def _get_json(url, timeout):
    """GET a bounded JSON document: https (or loopback), no redirects, 256 KiB."""
    if not https_or_loopback(url):
        raise ValueError("not an https URL")
    opener = urllib.request.build_opener(_NoRedirect)
    req = urllib.request.Request(url, headers={"Accept": "application/json",
                                               "User-Agent": "pscat-oauth-check"})
    with opener.open(req, timeout=timeout) as resp:
        body = resp.read(MAX_DOCUMENT_BYTES + 1)
    if len(body) > MAX_DOCUMENT_BYTES:
        raise ValueError("the document is over %d bytes" % MAX_DOCUMENT_BYTES)
    return json.loads(body.decode("utf-8"))


def discovery_urls(issuer):
    """The issuer's metadata URLs, in the server's order (`discovery_urls`)."""
    base = trim_slash(issuer)
    i = base.find("://")
    j = base.find("/", i + 3) if i >= 0 else -1
    origin, path = (base[:j], base[j:]) if j >= 0 else (base, "")
    if not path:
        return [origin + "/.well-known/oauth-authorization-server",
                origin + "/.well-known/openid-configuration"]
    return [origin + "/.well-known/oauth-authorization-server" + path,
            origin + "/.well-known/openid-configuration" + path,
            base + "/.well-known/openid-configuration"]


def fetch_jwks(issuer, jwks_url, timeout, say):
    """Fetch the JWKS as the server does. Returns (document, error text).

    The token is never involved. `say` receives every URL used.
    """
    try:
        if not jwks_url:
            last = "no metadata URL"
            for url in discovery_urls(issuer):
                say("GET " + url)
                try:
                    doc = _get_json(url, timeout)
                except (OSError, ValueError, urllib.error.URLError) as e:
                    last = "%s: %s" % (url, e)
                    continue
                if not isinstance(doc, dict) or doc.get("issuer") != issuer:
                    return None, "the metadata at %s names another issuer than --issuer" % url
                jwks_url = doc.get("jwks_uri")
                if not isinstance(jwks_url, str):
                    return None, "the metadata at %s has no jwks_uri" % url
                break
            else:
                return None, "discovering the issuer's metadata failed (%s)" % last
        say("GET " + jwks_url)
        return _get_json(jwks_url, timeout), None
    except (OSError, ValueError, urllib.error.URLError) as e:
        return None, "fetching the JWKS failed (%s)" % e


# --------------------------------------------------------------------- cli

class _Parser(argparse.ArgumentParser):
    def error(self, message):
        # argparse would echo an unrecognised argument, which could be a token
        # pasted on the command line by mistake.
        sys.stderr.write("oauth-check.py: invalid arguments (not echoed, in case one is a token); "
                         "see --help. The token goes in an environment variable.\n")
        sys.exit(2)


def build_parser():
    p = _Parser(
        prog="oauth-check.py",
        allow_abbrev=False,  # `--token <jwt>` must not become `--token-env <jwt>`
        description="Check an access token against pscat-mcp's `oauth` rules. Does NOT verify "
                    "the signature. The token is read from an environment variable.")
    p.add_argument("--issuer", required=True, help="PSCAT_MCP_OAUTH_ISSUER, exactly")
    p.add_argument("--resource", required=True, help="PSCAT_MCP_OAUTH_RESOURCE, exactly")
    p.add_argument("--token-env", default="OAUTH_CHECK_TOKEN",
                   help="name of the environment variable holding the token "
                        "(default: OAUTH_CHECK_TOKEN)")
    p.add_argument("--allow-user", action="append", default=[], metavar="sub:X|email:Y",
                   help="an allowlist entry as in PSCAT_MCP_OAUTH_ALLOWED_USERS; repeatable")
    p.add_argument("--scope", action="append", default=[],
                   help="a required scope (PSCAT_MCP_OAUTH_SCOPES); repeatable")
    p.add_argument("--leeway", type=int, default=DEFAULT_LEEWAY_SECS,
                   help="clock leeway in seconds (PSCAT_MCP_OAUTH_LEEWAY_SECS; default %(default)s)")
    p.add_argument("--jwks-file", help="a local JWKS JSON file, to check the kid offline")
    p.add_argument("--fetch-jwks", action="store_true",
                   help="fetch the JWKS from the issuer (network; prints the URLs used)")
    p.add_argument("--jwks-url", help="with --fetch-jwks: the JWKS URL (PSCAT_MCP_OAUTH_JWKS_URL), "
                                      "skipping discovery")
    p.add_argument("--timeout", type=float, default=5.0, help="network timeout in seconds")
    return p


def main(argv=None, environ=None, out=None):
    out = out or sys.stdout
    environ = os.environ if environ is None else environ
    args = build_parser().parse_args(argv)

    def usage(msg):
        sys.stderr.write("oauth-check.py: %s\n" % msg)
        return 2

    if args.jwks_file and args.fetch_jwks:
        return usage("use --jwks-file or --fetch-jwks, not both")
    if args.jwks_url and not args.fetch_jwks:
        return usage("--jwks-url needs --fetch-jwks")
    allow = []
    for n, e in enumerate(args.allow_user, 1):
        parsed = parse_allow_user(e)
        if parsed is None:  # not echoed: it may be a personal address
            return usage("--allow-user #%d is neither sub:<subject> nor email:<address>" % n)
        allow.append(parsed)
    if not re.match(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$", args.token_env):
        # Not echoed: a token pasted here by mistake must not reach the output.
        return usage("--token-env must be the NAME of an environment variable")
    token = environ.get(args.token_env, "").strip()
    if token.lower().startswith("bearer "):
        token = token[7:].strip()
    if not token:
        return usage("the environment variable %s is not set or empty" % args.token_env)

    jwks, jwks_note = None, None
    if args.jwks_file:
        try:
            with open(args.jwks_file, "rb") as f:
                jwks = json.loads(f.read(MAX_DOCUMENT_BYTES + 1).decode("utf-8"))
        except (OSError, ValueError) as e:
            return usage("cannot read --jwks-file (%s)" % type(e).__name__)
    elif args.fetch_jwks:
        jwks, jwks_note = fetch_jwks(args.issuer, args.jwks_url, args.timeout,
                                     lambda m: out.write("network: %s\n" % m))

    results = check_token(token, args.issuer, args.resource, allow, jwks, tuple(args.scope),
                          leeway=args.leeway, jwks_note=jwks_note)
    if jwks_note:
        results.insert(1, Result(FAIL, "jwks-fetch", jwks_note))

    out.write("pscat-mcp oauth token check (token and signature are never printed)\n")
    for r in results:
        line = "[%s] %s: %s" % (r.status, r.name, r.detail)
        if r.status == FAIL and r.reason:
            line += " -> server reason=%s" % r.reason
        out.write(line + "\n")
    failed = [r for r in results if r.status == FAIL]
    skipped = [r.name for r in results if r.status == SKIP]
    first = min((r for r in failed if r.reason), key=lambda r: ORDER.index(r.reason), default=None)
    out.write("\n")
    out.write("NOT CHECKED: the signature. This script does not verify it, so a token that "
              "passes here can still get 401 bad_signature.\n")
    if skipped:
        out.write("Skipped checks: %s\n" % ", ".join(skipped))
    if failed:
        out.write("RESULT: FAIL (%d check(s) failed)\n" % len(failed))
        if first:
            out.write("The server checks in this order: shape, alg/crit/kid, key and signature, iss, "
                      "aud, exp, nbf, sub, scope, allowlist. It would log reason=%s first.\n"
                      % first.reason)
        return 1
    out.write("RESULT: PASS (every check that ran passed; see the notes above)\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
