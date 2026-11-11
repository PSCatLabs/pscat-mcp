"""Billing guard for the agent evals: Claude usage only through the claude.ai
subscription, never per-token API billing.

Every check fails closed: a missing field, an unparseable line or an
unexpected event is a refusal, not a pass. Layers, in the order they run:

1. ``check_parent_env``: refuse to start when a variable that selects API-key
   or cloud-provider billing is present in the runner's environment (even
   empty).
2. ``child_env``: the child gets an allowlisted environment, so those
   variables (and the parent Claude Code session's own variables) never
   reach it whatever check 1 decided.
3. ``check_auth_status``: ``claude auth status``, run with that same child
   environment, must report a claude.ai login on the first-party provider.
4. ``assert_safe_argv``: the child's command line never carries a flag that
   switches auth (``--bare`` is API-key only; ``--settings`` could add an
   ``apiKeyHelper``).
5. ``StreamGuard``: watches the child's stream-json output line by line. The
   ``system/init`` event must report ``apiKeySource: "none"`` (what a
   claude.ai login produces) before anything else happens; a
   ``rate_limit_event`` with ``isUsingOverage: true`` (extra usage, billed
   per token) or a non-``allowed`` status stops the run; the final
   ``result`` must attribute usage only to the ``firstParty`` provider.

``total_cost_usd`` in the result event is not a billing signal: Claude Code
reports a list-price estimate there for subscription sessions too.
"""

from __future__ import annotations

import json
import os
from typing import Iterable, Mapping

# Present (even empty) in the runner's environment -> refuse to start. The
# first four are named by issue #41; the last two also route requests away
# from the claude.ai subscription.
FORBIDDEN_ENV = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_BASE_URL",
)

# The only variables a child agent process inherits. Everything else,
# including CLAUDECODE, CLAUDE_CODE_* and CLAUDE_EFFORT from a parent
# Claude Code session, is dropped.
ENV_ALLOWLIST = (
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    # Selects which Claude Code configuration (and login) to use; the auth
    # check runs with the same value, so it checks the login the child uses.
    "CLAUDE_CONFIG_DIR",
)

# Flags the runner must never pass to `claude`.
FORBIDDEN_FLAGS = (
    "--bare",  # API key or apiKeyHelper only; never reads the OAuth login
    "--settings",  # could carry apiKeyHelper or env overrides
    "--fallback-model",  # silently changes the model under test
    "--betas",  # API-key users only
)

EXPECTED_AUTH = {
    "loggedIn": True,
    "authMethod": "claude.ai",
    "apiProvider": "firstParty",
}

# The apiKeySource value a claude.ai (subscription) login produces, observed
# with Claude Code 2.1.322.
SUBSCRIPTION_API_KEY_SOURCE = "none"

RATE_LIMIT_OK = ("allowed", "allowed_warning")


class GuardError(RuntimeError):
    """The billing guard refused. The run must stop."""


class IsolationError(GuardError):
    """The child sees something other than the server under test."""


class RateLimited(GuardError):
    """The subscription's rate limit was reached. The run must stop."""


def check_parent_env(environ: Mapping[str, str]) -> None:
    present = sorted(name for name in FORBIDDEN_ENV if name in environ)
    if present:
        raise GuardError(
            "refusing to run: "
            + ", ".join(present)
            + " set in the environment (selects API or cloud billing). "
            "Unset it; the evals use only the claude.ai login."
        )


def child_env(
    environ: Mapping[str, str], extra: Mapping[str, str] | None = None
) -> dict[str, str]:
    env = {k: environ[k] for k in ENV_ALLOWLIST if k in environ}
    env["TERM"] = "dumb"
    env["NO_COLOR"] = "1"
    for k, v in (extra or {}).items():
        if k in FORBIDDEN_ENV:
            raise GuardError(f"refusing to pass {k} to a child process")
        env[k] = v
    # Belt and braces: nothing forbidden survives, whatever the caller did.
    for k in FORBIDDEN_ENV:
        env.pop(k, None)
    return env


def check_auth_status(returncode: int, stdout: str) -> dict:
    """Parse `claude auth status` (JSON) and require a claude.ai login.
    Returns a redacted summary (no email or org) for the run record."""
    if returncode != 0:
        raise GuardError(f"`claude auth status` exited {returncode}; log in with `claude auth login`")
    try:
        status = json.loads(stdout)
    except ValueError as e:
        raise GuardError(f"`claude auth status` did not print JSON: {e}") from None
    if not isinstance(status, dict):
        raise GuardError("`claude auth status` did not print a JSON object")
    for key, want in EXPECTED_AUTH.items():
        got = status.get(key)
        if got != want:
            raise GuardError(
                f"`claude auth status` reports {key}={got!r}, need {want!r} "
                "(a claude.ai subscription login)"
            )
    return {
        k: status.get(k)
        for k in ("authMethod", "apiProvider", "subscriptionType", "planTier")
    }


def assert_safe_argv(argv: Iterable[str]) -> None:
    for arg in argv:
        for flag in FORBIDDEN_FLAGS:
            if arg == flag or arg.startswith(flag + "="):
                raise GuardError(f"refusing to pass {flag} to claude")
        if "apiKeyHelper" in arg:
            raise GuardError("refusing to pass an apiKeyHelper to claude")


class StreamGuard:
    """Checks a Claude Code stream-json transcript as it arrives.

    ``feed`` takes one raw stdout line and raises GuardError (or a subclass)
    the moment the stream is not provably subscription-billed and isolated.
    The caller kills the child on any exception."""

    def __init__(self, server_name: str, model: str | None = None):
        self.server_name = server_name
        self.prefix = f"mcp__{server_name}__"
        self.model = model
        self.init: dict | None = None
        self.rate_limits: list[dict] = []
        self.result_seen = False

    def feed(self, line: str) -> dict:
        line = line.strip()
        if not line:
            return {}
        try:
            event = json.loads(line)
        except ValueError:
            raise GuardError(f"non-JSON line on the stream: {line[:120]!r}") from None
        if not isinstance(event, dict):
            raise GuardError("stream event is not a JSON object")
        etype = event.get("type")
        if self.init is None:
            if etype == "system" and event.get("subtype") == "init":
                self._check_init(event)
                self.init = event
            elif etype != "system":
                raise GuardError(f"{etype!r} event before system/init")
            return event
        if etype == "system" and event.get("subtype") == "init":
            # A second init (resumed or nested session) must pass the same checks.
            self._check_init(event)
        elif etype == "rate_limit_event":
            self._check_rate_limit(event)
        elif etype == "result":
            self._check_result(event)
            self.result_seen = True
        return event

    def _check_init(self, init: dict) -> None:
        source = init.get("apiKeySource")
        if source != SUBSCRIPTION_API_KEY_SOURCE:
            raise GuardError(
                f"system/init reports apiKeySource={source!r}; only "
                f"{SUBSCRIPTION_API_KEY_SOURCE!r} (the claude.ai login) is allowed"
            )
        servers = init.get("mcp_servers")
        if not isinstance(servers, list):
            raise IsolationError("system/init has no mcp_servers list")
        names = sorted(s.get("name") for s in servers if isinstance(s, dict))
        if names != [self.server_name]:
            raise IsolationError(
                f"child sees MCP servers {names}, expected only [{self.server_name!r}]"
            )
        bad = [s for s in servers if s.get("status") != "connected"]
        if bad:
            raise IsolationError(f"MCP server not connected: {bad}")
        tools = init.get("tools")
        if not isinstance(tools, list):
            raise IsolationError("system/init has no tools list")
        foreign = [t for t in tools if not str(t).startswith(self.prefix)]
        if foreign:
            raise IsolationError(f"child has tools outside {self.prefix}*: {foreign}")
        for plugin in init.get("plugins") or []:
            if isinstance(plugin, dict) and plugin.get("path") != "builtin":
                raise IsolationError(
                    f"child loaded the installed plugin {plugin.get('name')!r}"
                )
        if self.model and self.model.startswith("claude-") and init.get("model") != self.model:
            raise GuardError(
                f"child runs model {init.get('model')!r}, requested {self.model!r}"
            )

    def _check_rate_limit(self, event: dict) -> None:
        info = event.get("rate_limit_info")
        if not isinstance(info, dict):
            raise GuardError("rate_limit_event without rate_limit_info")
        self.rate_limits.append(info)
        if info.get("isUsingOverage") is not False:
            raise GuardError(
                "rate_limit_event reports isUsingOverage="
                f"{info.get('isUsingOverage')!r}: extra usage is billed per token"
            )
        if info.get("status") not in RATE_LIMIT_OK:
            raise RateLimited(
                f"subscription rate limit status {info.get('status')!r}"
                f" ({info.get('rateLimitType')}, resets at {info.get('resetsAt')})"
            )

    def _check_result(self, event: dict) -> None:
        usage = event.get("modelUsage") or {}
        if not isinstance(usage, dict):
            raise GuardError("result.modelUsage is not an object")
        for model, u in usage.items():
            provider = u.get("provider") if isinstance(u, dict) else None
            # Older Claude Code versions omit `provider`; only an explicit
            # non-first-party value is refused here (init already proved auth).
            if provider is not None and provider != "firstParty":
                raise GuardError(f"result attributes {model} usage to provider {provider!r}")
