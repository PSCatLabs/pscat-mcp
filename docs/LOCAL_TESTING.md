# Testing locally with Claude Code and Codex

This guide connects a local build of `pscat-mcp-server` to a Claude Code or
Codex session, first over stdio and then over Streamable HTTP. It ends with
what to check and what to report.

The automated tests use the rmcp client and raw JSON-RPC. Real agent clients
haven't been tested yet, so client-specific findings are the point of this
exercise.

## 1. Build

```bash
git clone https://github.com/PSCatLabs/pscat-mcp && cd pscat-mcp
cargo build --release
BIN="$PWD/target/release/pscat-mcp-server"
"$BIN" --version
```

The commands below use `$BIN`. It exists only in the shell where you set
it, so set it again in any new terminal (or paste the absolute path
instead). Clients store the path you give them, so it must be absolute.

## 2. Pick a name that doesn't clash

PSCat's own plugin for Claude Code and Codex registers an MCP server called
`pscat`, and it also has a `render_postscript` tool. If the plugin is
installed, register this server as **`pscat-render`** so the two stay
apart:

* Claude Code names tools `mcp__<server>__<tool>`, so the two never collide
  technically, but an agent given both may pick either. Say "use the
  pscat-render server" in prompts, or turn the plugin off while testing:
  `/plugin` in Claude Code, or `enabled = false` under
  `[plugins."pscat@pscat"]` in `~/.codex/config.toml`.
* `get_renderer_info` exists only on this server. It reports
  `server_version`, the limits in force, and a `renderer` block with the
  PSCat version and git revision.

## 3. stdio

### Claude Code

```bash
claude mcp add -s user pscat-render -- "$BIN" serve-stdio
claude mcp list            # pscat-render ... ✔ Connected
```

`-s user` makes the server available in every directory. Without it the
server is registered only for the directory you ran the command in
(`local` scope), and a session started elsewhere won't see it.

Start a new `claude` session (servers are loaded at startup) and run
`/mcp`. `pscat-render` should be connected with four tools.

To see the server's logs, start the session with `claude --debug`. Logs
are on stderr, so they never mix with MCP messages. Set a level with
`claude mcp add ... -e RUST_LOG=debug -- "$BIN" serve-stdio`.

### Codex

```bash
codex mcp add pscat-render -- "$BIN" serve-stdio
codex mcp list             # pscat-render  ...  enabled
```

The same entry written by hand in `~/.codex/config.toml`:

```toml
[mcp_servers.pscat-render]
command = "/absolute/path/to/target/release/pscat-mcp-server"
args = ["serve-stdio"]
```

Start a new `codex` session and run `/mcp` to see the tools. Codex's
default tool timeout is 60 s, above this server's 20 s render deadline, so
no `tool_timeout_sec` is needed.

## 4. Streamable HTTP

This is the transport a hosted deployment uses, so test it too. The server
and the clients run in different terminals, so keep the bearer token in a
file they can all read:

```bash
(umask 077; openssl rand -hex 32 > "$HOME/.pscat-mcp-test-token")
```

Start the server in its own terminal:

```bash
PSCAT_MCP_AUTH_TOKEN_FILE="$HOME/.pscat-mcp-test-token" PSCAT_MCP_LOG_FORMAT=pretty \
  "$BIN" serve-http        # 127.0.0.1:8080, endpoint /mcp
```

In each terminal where you run `claude mcp add`, `codex mcp add` or
`codex`:

```bash
export PSCAT_MCP_TOKEN="$(cat "$HOME/.pscat-mcp-test-token")"
```

If `PSCAT_MCP_TOKEN` is empty there, the client sends `Bearer ` with no
token and every call gets 401.

Leave it running and watch its log: every request is one line with its
status. A client that gets **406** is
[issue #17](https://github.com/PSCatLabs/pscat-mcp/issues/17) (a client
that doesn't send `Accept: text/event-stream`); note which client it was.

The token is a throwaway for this test.

An HTTP test counts only if the server's terminal logs a `POST /mcp` for
each tool call. With the stdio entry and PSCat's plugin also registered, the
agent may answer through one of those instead. Remove or disable them while
testing HTTP if that happens.

### Claude Code

```bash
claude mcp add -s user --transport http pscat-render-http http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer $PSCAT_MCP_TOKEN"
```

Claude Code saves the header in `~/.claude.json`.

### Codex

```bash
codex mcp add pscat-render-http --url http://127.0.0.1:8080/mcp \
  --bearer-token-env-var PSCAT_MCP_TOKEN
```

Codex reads the token from the variable when it starts, so export
`PSCAT_MCP_TOKEN` (the same value) in the shell that launches `codex`. The
configuration stores only the variable's name.

### The container instead of the binary

```bash
docker build -t pscat-mcp .
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 127.0.0.1:8080:8080 -e PSCAT_MCP_AUTH_TOKENS="$PSCAT_MCP_TOKEN" pscat-mcp
```

The client entries above work unchanged. Build locally rather than pulling
`ghcr.io/pscatlabs/pscat-mcp`: that package is private, and it is
`linux/amd64` only, so on Apple silicon it would run under emulation.

## 5. A first prompt

Paste this into a new session. It calls `get_renderer_info`, `render_postscript` and `validate_postscript`,
and each step has a known result. (`describe_art_capabilities` is covered by
the `describe-capabilities` eval case and scenario 6.)

````text
Use only the pscat-render MCP server for this (not the pscat plugin's tools).

1. Call get_renderer_info and tell me the pscat-mcp server version and the PSCat version and revision.
2. Render this program as a PNG at 300×200 points and describe the image you got back:

%!PS
0.1 0.2 0.6 setrgbcolor 0 0 300 200 rectfill
1 0.3 0.2 setrgbcolor 150 110 60 0 360 arc fill
1 setgray /Helvetica-Bold findfont 20 scalefont setfont 20 20 moveto (pscat-render) show
showpage

3. Render the same program as SVG, then as PDF, and tell me what kind of content block each came back as.
4. Validate this program exactly as written (the typo is deliberate; don't correct it) and tell me the error name, the offending command and the line:

%!PS
100 100 moveto
200 200 liento
stroke showpage

5. Render this program and tell me the error code and roughly how long it took:

%!PS
{ 1 pop } loop

Finish with a table: step, exact tool name you called, error code (if any), result.
````

For the HTTP entry, use `pscat-render-http` in the first line.

A pass looks like this:

| Step | Expected |
|---|---|
| Every step | In Claude Code, tool names start with `mcp__pscat-render__` (or `mcp__pscat-render-http__`). Over HTTP, the server's log shows a `POST /mcp` for each call. |
| 1 | The pscat-mcp version, then the PSCat version and revision, matching the pin in `Cargo.toml` and `Cargo.lock`. |
| 2 | The agent describes a dark blue background, an orange-red circle in the middle, and "pscat-render" in white at the bottom left. Separately, note whether the client showed you the image. |
| 3 | SVG returns an embedded `image/svg+xml` resource, and PDF an `application/pdf` resource, not images. |
| 4 | Not valid: `undefined`, offending command `liento`, line 3, plus a blank-page lint warning. If it comes back valid, check whether the agent corrected the typo in the source it sent. |
| 5 | `limit_exceeded` ("stopped at the time limit") after about 20 s, or `timeout`. |

In step 2, the description is the test. A wrong or vague description means
the image didn't reach the model. Whether the image appears on screen only
shows how that client displays images from tool results; record it either
way.

## 6. What else to try

[`examples/agent-scenarios.md`](../examples/agent-scenarios.md) has six
prompts with the expected tool call and result: render a program, draw a
diagram, diagnose an invalid program, ask for an unsupported format,
try to exhaust resources, and look up palette, template and procedure names. Beyond those and the prompt above:

* **Images.** After a PNG render, does the client show the image to you, and
  can the agent describe it correctly? That tells you whether the image
  content block reached the model, not just the text summary.
* **SVG and PDF.** Ask for `format: svg` and `format: pdf`. They come back
  as embedded resources; note how each client presents them.
* **Errors.** A failed render must show the PostScript error and no image.
* **Timeouts and cancellation.** `{ 1 pop } loop` should fail with
  `limit_exceeded` (PSCat's step limit) or `timeout` within about 20 s.
  Also interrupt the agent mid-render (Esc) and note whether the render
  stops: over stdio the client should send `notifications/cancelled`; over
  HTTP the server's log shows whether the request ended early.
* **Renderer info.** Ask which PSCat version is running.

## 7. What to report

For each client, transport and finding:

* the client and its version (`claude --version`, `codex --version`);
* the transport (stdio or HTTP);
* the server version from `get_renderer_info` (pscat-mcp and PSCat
  versions, PSCat revision);
* the scenario, what happened, and what you expected;
* any non-200 status in the HTTP server's log.

File server problems in
[PSCatLabs/pscat-mcp](https://github.com/PSCatLabs/pscat-mcp/issues), and
rendering problems (a wrong picture, a wrong PostScript error) in
[PSCatLabs/pscat](https://github.com/PSCatLabs/pscat/issues) with the
program attached.

## 8. Clean up

```bash
claude mcp remove -s user pscat-render
claude mcp remove -s user pscat-render-http
codex mcp remove pscat-render
codex mcp remove pscat-render-http
rm "$HOME/.pscat-mcp-test-token"
```

## 9. Automated agent evals

`scripts/agent-eval.sh` runs the prompts above with a real agent and checks
the transcript, so a change to a tool description, a result or an error can
be judged on what an agent actually does with it. It drives Claude Code in
headless mode (`claude -p --output-format stream-json`) and, optionally,
Codex (`codex exec --json`), over stdio and over HTTP.

It is local and manual only. It never runs in CI, which would need a
credential. CI runs only its offline unit tests (`scripts/agent-eval.sh
--test`): the billing guard and the transcript checks against a recorded
transcript, with no model.

### Run it

```bash
cargo build --release
claude auth status                 # "authMethod": "claude.ai"
scripts/agent-eval.sh --list       # the cases
scripts/agent-eval.sh --dry-run    # billing-guard pre-flight and the plan; no model calls
scripts/agent-eval.sh --case first-prompt --transport stdio    # one case
scripts/agent-eval.sh              # every case, stdio and HTTP, once
```

| Option | Meaning |
|---|---|
| `--case ID` | Run only this case (repeatable). |
| `--transport stdio\|http\|both` | Default `both`. |
| `--repeat N` | Runs per case, agent and transport (default 1). Mixed results are reported as `flaky`. |
| `--max-cases N` | Stop after the first N cases. |
| `--model ID` | Claude model, default `claude-sonnet-5-5` (a full ID, not an alias, so results stay comparable). The guard aborts if the session reports a different model. |
| `--effort LEVEL` | Claude effort level. Default: the CLI's own default. |
| `--agent codex` | Also run Codex (repeat `--agent claude --agent codex` for both). Codex uses OpenAI usage, so it is opt-in. |
| `--include-expensive` | Also run cases tagged `expensive` (scenario 5c pastes about 285 KB of PostScript). |
| `--server PATH` | Server binary. Default `target/release/pscat-mcp-server`. |
| `--recheck RUN_DIR` | Check a previous run's saved transcripts again against the current case files, for example after editing a case. No agent runs. |

`python3 -I evals/agent/run.py` takes the same options. The runner uses
the Python 3 standard library only, so it needs no virtualenv and no
packages, and the server crate gets no eval code or dependencies.

Each run writes to `evals/agent-runs/<timestamp>/` (git-ignored): one
`transcripts/<case>__<agent>__<transport>__<n>.jsonl` per run (the raw
stream, with the HTTP token replaced by `<redacted-token>`) plus its
stderr, the HTTP server's log, `summary.json` and `summary.txt`. The
summary prints the client versions, and the server and PSCat versions,
which it reads from `get_renderer_info` over JSON-RPC without a model:

```text
server: pscat-mcp 0.1.0, PSCat 0.7.0-rc.4 (df39114f1223)
clients: claude 2.1.322 (Claude Code)
model: claude-sonnet-5-5

case                agent   transport  verdict  passed  turns
------------------  ------  ---------  -------  ------  -----
diagnose-undefined  claude  http       pass     1/1     3
```

The exit status is 0 when every row passes, 1 when a check failed, and 2
when the billing guard stopped the run.

**Budget.** Cases run one at a time. A full default run is 22 sessions
(11 cases × 2 transports). The first 20 (10 cases) took 54 turns and 34 tool
calls with `claude-sonnet-5-5`, about 4 minutes in all; the
`describe-capabilities` case was added later and not yet timed. Most cases take 2 or 3 turns,
and the first prompt takes 7. It moved the Max 5x plan's five-hour
utilization from 8 % to 10 %, while another session was using the same
subscription. `--repeat N` multiplies all of that. To try a change,
run one case: `--case first-prompt --transport stdio` takes 7 turns.

### The billing guard

Claude usage must come only from the claude.ai (Max) subscription login,
never from per-token API billing.

**First, turn off extra usage in your claude.ai account settings.** Extra
usage (overage) lets a plan keep going past its limits at per-token rates.
With it off, a session that reaches the plan's limit is refused rather than
billed. That setting is the only thing that makes per-token billing
impossible. The guard below can *detect* overage, but it can't prevent the
request that triggered it from being billed: it sees the overage flag only
in the stream, after that request was sent. Each `rate_limit_event` in a
transcript shows the account's state. On the account these evals were
built with, it read `"overageStatus": "rejected"` and
`"overageDisabledReason": "org_level_disabled"`.

The guard in `evals/agent/guard.py` fails closed: a missing field, an
unparseable line or an unexpected event stops the run. It has five layers.

1. **Parent environment.** The runner refuses to start when any of
   `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`,
   `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY` or
   `ANTHROPIC_BASE_URL` is set, even to an empty string. Unset it and try
   again.
2. **Child environment.** Each `claude` child gets an allowlisted
   environment: `HOME`, `PATH`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`,
   `LANG`, `LC_ALL`, `LC_CTYPE` and `CLAUDE_CONFIG_DIR`, plus `TERM=dumb`.
   Nothing else reaches it, so the variables above can't leak through, and
   neither can a parent Claude Code session's own (`CLAUDECODE`,
   `CLAUDE_CODE_SESSION_ID`, `CLAUDE_EFFORT` and the rest) when you start
   the evals from inside one. `USER` is needed: without it the macOS
   keychain lookup fails and `claude auth status` reports
   `"loggedIn": false`.
3. **Login.** `claude auth status --json`, run with that same child
   environment, must exit 0 and report `"loggedIn": true`,
   `"authMethod": "claude.ai"` and `"apiProvider": "firstParty"`. With
   `ANTHROPIC_API_KEY` set it reports `"authMethod": "api_key"`, and the
   guard refuses.
4. **Command line.** The runner never passes `--bare` (it accepts only
   `ANTHROPIC_API_KEY` or an `apiKeyHelper`, never the OAuth login),
   `--settings` (which could add an `apiKeyHelper` or `env`),
   `--fallback-model` or `--betas`. A unit test checks the command it
   builds.
5. **The stream.** The runner reads the child's stream-json line by line
   and kills the child's process group as soon as a check fails:
   * the first non-`system` event must come after `system/init`;
   * `system/init` must report `"apiKeySource": "none"`, which is what a
     claude.ai login produces (observed with Claude Code 2.1.322; an API
     key or `apiKeyHelper` produces another value);
   * every `rate_limit_event` must carry `"isUsingOverage": false`, because
     extra usage beyond the plan is billed per token. A status other than
     `allowed` or `allowed_warning` means the subscription's limit is
     reached, so the run stops instead of retrying;
   * at least one such `rate_limit_event` must arrive before the `result`
     event, or before the stream ends. Those events are the only place
     overage shows up, so a session without one can't be shown to be on the
     subscription. Every session in the runs so far had one;
   * the final `result` must attribute usage only to the `firstParty`
     provider.

`total_cost_usd` in the result event isn't a billing signal. Claude Code
reports a list-price estimate there for subscription sessions too
(`"costBasis": "list"`).

### What the child sees

Every run starts in a fresh, empty temporary directory, so no repository
`CLAUDE.md` or project settings apply. The command is:

```text
claude -p --output-format stream-json --verbose --model claude-sonnet-5-5 --max-turns N
  --mcp-config <0600 temp file> --strict-mcp-config --tools=
  --allowedTools 'mcp__pscat-render__*' --permission-mode dontAsk
  --setting-sources project --no-session-persistence      (prompt on stdin)
```

* `--verbose` is required: `-p` with `stream-json` exits with an error
  without it.
* `--strict-mcp-config` drops every MCP server other than the one in
  `--mcp-config`, including the PSCat plugin's `pscat` server.
* `--setting-sources project` skips your user settings, so installed
  plugins (the PSCat plugin's skills and hooks), hooks, the default
  permission mode and the default model don't apply. Auth is unaffected:
  the login lives in the keychain, not in settings.
* `--tools=` removes the built-in tools (Bash, Read, ToolSearch and the
  rest), so the server's three tools are loaded directly instead of being
  deferred behind ToolSearch.
* `--allowedTools 'mcp__pscat-render__*'` lets every tool of the server run
  in `-p` mode. Without it, each call is refused ("you haven't granted it
  yet") and shows up in `permission_denials`. `--permission-mode dontAsk`
  isn't needed for that, but it keeps a `defaultMode` from managed settings
  from deciding anything else.

The guard checks the result in `system/init`: `mcp_servers` must be
exactly `pscat-render` (connected), every tool must start with
`mcp__pscat-render__`, and only built-in plugins may be loaded. Otherwise
the runner kills the child as soon as it reads that event, and stops the
whole eval run.

Claude Code keeps per-directory state (for example a PDF result saved as a
file) under `~/.claude/projects/<cwd>` even with
`--no-session-persistence`. The runner deletes that one folder for each
run's temporary directory, and touches nothing else in your configuration.

### HTTP

For `--transport http` the runner starts `pscat-mcp-server serve-http` on
`127.0.0.1` with port 0, reads the port it bound from the log, and waits
for `/healthz`. The bearer token is random for each eval run. It's written
to a mode-600 file in a mode-700 temp directory, together with the
client's MCP config (the `Authorization` header). Both are deleted when the
run ends. The token is never printed, and it is redacted from saved
transcripts and logs. Both transports use the server name `pscat-render`,
so the cases are the same. An HTTP run also checks that the server logged
at least one `POST /mcp` for each tool call. The server is stateless, so
every call is its own POST.

### Codex (optional)

`--agent codex` runs `codex exec --json --ephemeral --ignore-user-config
--ignore-rules --sandbox read-only` with the server given through `-c
mcp_servers.pscat-render.*` (for HTTP, the token goes in an environment
variable named by `bearer_token_env_var`). `--ignore-user-config` keeps
your `~/.codex/config.toml` servers and plugins out. Codex still uses its
own login from `CODEX_HOME`. Shell, file and web actions count as calls
outside the server, and fail the "only pscat-render tools" check. The Codex
transcript parser follows the documented `codex exec --json` events. Only
its error path has been seen in a real run, so treat Codex verdicts as
provisional until a passing Codex transcript has been checked.

### Cases

Each `evals/agent/cases/*.json` file is one case: the scenarios in
[`examples/agent-scenarios.md`](../examples/agent-scenarios.md) and the
first prompt in section 5.

```json
{
  "id": "render-shapes",
  "title": "Scenario 1: render a supplied program verbatim at 400x300",
  "transports": ["stdio", "http"],
  "max_turns": 6,
  "prompt": "Render this PostScript ... (400×300 points):\n\n{{file:examples/shapes.ps}}",
  "expect": {
    "tools_required": ["render_postscript"],
    "calls": [{
      "tool": "render_postscript",
      "args": {"source": {"equals_text": "{{file:examples/shapes.ps}}"},
               "width": 400, "height": 300, "format": {"in": ["png", null]}},
      "result": {"is_error": false, "content_types": ["image"],
                 "structured": {"ok": true, "artifacts.0.width_px": 400}}
    }],
    "answer": {"any": [["red"], ["green"], ["gold", "yellow"]]}
  }
}
```

* **`calls`** picks the calls of `tool` whose arguments match `where`. By
  default at least one of them must pass the `args` and `result` checks;
  `which` can be `first`, `last` or `all` instead. `max_count` limits how
  many such calls there may be (for example, an infinite loop must not be
  retried unchanged), and `forbid: true` means there must be none.
* **Matchers.** A plain value means equality. Operators are `equals`,
  `equals_text` (the same text up to line endings and trailing
  whitespace, which is how "source unchanged" is checked), `contains`,
  `not_contains`, `in` (`null` stands for an omitted argument),
  `absent`, `min`, `max` and `regex`.
* **`result`** checks `is_error`, `error_code` (from
  `structuredContent.error.code`, or from the error text), `content_types`
  (`image`, `resource:image/svg+xml`, `resource:application/pdf`),
  `structured` (dotted paths into `structuredContent`) and
  `text_contains`.
* **`answer`** checks the final answer, ignoring case: `all` keywords,
  `any` groups (one of each), `none` and `regex`. It's how "describe the
  image" is checked: for the first prompt, the answer must mention blue, a
  circle, red or orange, and the text "pscat-render".
* **`expect_any_of`** lists alternatives, and the case passes if one of
  them does. Scenario 4, for example, passes when the agent calls with
  `jpeg` and gets `unsupported_format`, and also when it declines and
  offers PNG.
* **Placeholders.** `{{file:PATH}}` inserts a repository file. Answer
  checks can use `{{server_version}}`, `{{pscat_version}}`,
  `{{pscat_revision}}` and `{{pscat_revision7}}`, which come from
  `get_renderer_info`.
* **Checks on every run.** The agent finished (it didn't hit
  `--max-turns`), there were no permission denials, every call went to
  `pscat-render`, and the turn count is at most `max_turns`.

What Claude Code passes to the model differs from what the server sends.
An embedded resource becomes a text block,
`[Resource from pscat-render at <uri>] <svg ...>`. A blob becomes
`Binary content (application/pdf, 3.4KB) saved to <path>`. And the
`structuredContent` JSON replaces the server's text summary. The checks
read those forms.

## 10. Server-layer evals

The agent evals in section 9 judge what an agent does with the server. The
server-layer evals check the server itself, with no model, no tokens and no
network beyond loopback, so they run in CI on every pull request. They
automate what the manual prompts in sections 5 and 6 and
[`examples/agent-scenarios.md`](../examples/agent-scenarios.md) check by
eye.

```bash
cargo test --test eval_server -- --nocapture   # stdio + HTTP (bearer token); about 15 s
cargo test --test eval_replay -- --nocapture   # recorded client traffic
scripts/container-eval.sh                      # the same scenarios in the image (needs Docker)
IMAGE=pscat-mcp:ci SKIP_BUILD=1 scripts/container-eval.sh
```

`cargo test --all` runs the first two. The container run is a separate step
in `container.yml`, after the smoke test. It builds the test crate (cold,
a few minutes; cached after that) and starts one hardened container per
server configuration. The test reads `PSCAT_MCP_EVAL_IMAGE`; the script
maps `IMAGE` onto it. `IMAGE` itself is deliberately not read, because
`container.yml` sets `IMAGE` to the published name for the whole workflow.

The runner is Rust, not a script, because the issue asks for the rmcp client
and a Rust test needs nothing the required checks do not already have:
`test (ubuntu-latest)`, `test (macos-latest)` and the MSRV job build and run
it. The scenarios are data, so a new case is a JSON file, not Rust.

### Scenarios

`evals/server/scenarios/*.json`, one scenario per file, named like the agent
cases. A scenario runs on every target (stdio, http, container) unless
`transports` narrows it, and has the agent cases' vocabulary where it fits:

```json
{
  "id": "render-shapes",
  "title": "Scenario 1: render a supplied program verbatim at 400x300",
  "origin": "examples/agent-scenarios.md section 1",
  "server_env": {"PSCAT_MCP_MAX_PAGES": "2"},
  "steps": [{
    "label": "PNG 400x300 with the expected shapes",
    "tool": "render_postscript",
    "args": {"source": "{{file:examples/shapes.ps}}", "width": 400, "height": 300},
    "result": {
      "is_error": false,
      "content_types": ["image:image/png", "text"],
      "structured": {"ok": true, "artifacts.0.width_px": 400},
      "png": {"width": 400, "height": 300,
              "pixels": [{"x": 90, "y": 130, "rgb": [230, 64, 64]}]},
      "elapsed_ms": {"max": 10000},
      "renders_executed": 1
    }
  }]
}
```

* **`server_env`** sets `PSCAT_MCP_*` variables for the server the scenario
  runs on, so stdio, HTTP and the container are configured the same way
  (the child's environment, or `docker run -e`). Scenarios with the same
  `server_env` share one server. The runaway-program scenarios use short
  deadlines this way (`PSCAT_MCP_RENDER_TIMEOUT_MS=1500`, a step cap, or
  PSCat's own limits off so only the kill can stop the loop); the full
  scenario run takes about 15 s.
* **`result`** has the agent cases' `is_error` (omitted means false),
  `error_code`, `structured` (dotted paths into `structuredContent`),
  `text_contains` and `content_types`, and the same matchers (`equals`,
  `equals_text`, `contains`, `not_contains`, `in`, `absent`, `min`, `max`,
  `regex`). Content types are `text`, `image:<mime>` and
  `resource:<mime>`, matched by prefix; `forbid_content_types` asserts a
  failed render has no image. The server layer adds:
  * `png`: `width`, `height` and `pixels` (`x`, `y` from the top left,
    `rgb`, `tolerance` default 10);
  * `resource`: `mime`, `starts_with` (a string or a list of alternatives),
    `contains`, `min_bytes`, for an SVG or PDF signature;
  * `elapsed_ms`: a matcher on the call's duration. Bounds are generous
    because CI machines are slow; a `min` is safe for a deadline;
  * `renders_executed`: how many jobs the server handed to a worker during
    the step, counted from its `render started` log line, so it works on
    every target. Requests refused before execution assert `0`.
* **`parallel`** (with `outcomes`) fires calls at once and compares the
  sorted list of results (`ok`, or the error code) with `outcomes`, which
  is how `server_busy` is tested without depending on which call wins the
  slot.
* **Placeholders:** `{{file:PATH}}`, `{{server_version}}`,
  `{{pscat_version}}`, `{{pscat_revision}}` (from `Cargo.lock`),
  `{{huge_source}}` and `{{pages:N}}`.
* Unknown keys are an error, so a typo cannot turn an assertion off.

What is covered: the first prompt in section 5 (the runaway loop is
`runaway-steps`, `runaway-time` and `runaway-timeout`), scenarios 1 to 6 of
`agent-scenarios.md` (including 6, the catalog tool), and every error code in its list except `cancelled`
(`tests/mcp_cancel.rs` covers it: an rmcp client's cancellation kills the
worker). Runaway loops must put `%!PS` and `{ 1 pop } loop` on separate
lines: `%` starts a comment that runs to the end of the line.

The run prints a table (scenario, step, result, milliseconds) and writes
`target/eval/<target>.json` and `<target>.xml` (JUnit);
`PSCAT_EVAL_REPORT_DIR` changes the directory, and `$GITHUB_STEP_SUMMARY`
gets a markdown table when it is set.

### Recorded client traffic

`evals/server/fixtures/*.json` hold what clients send over HTTP: the
headers (`Accept`, `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`,
`Content-Type`, `User-Agent`), the `initialize` capabilities (or
`server/discover`), `tools/list` and `tools/call`. `tests/eval_replay.rs`
sends each request to the in-process server with the real worker and checks
the reply is 2xx with valid JSON-RPC (a matching id, a result, no tool
error). Every fixture says in its metadata how it was obtained:

| Fixture | `obtained` | How |
|---|---|---|
| `claude-code.json` | real-capture | Claude Code 2.1.322, one `claude -p` prompt, one tool call, through the billing guard |
| `rmcp-client.json` | real-capture | the rmcp 3.9 client |
| `codex-synthesized.json` | synthesized | the rmcp capture with Codex's User-Agent; Codex was not run |
| `curl-smoke.json`, `legacy-2025-03-26.json` | synthesized | written to match `scripts/container-smoke.sh` and the legacy cases in `tests/http.rs` |
| `accept-json-only.json` | synthesized | the report in [#17](https://github.com/PSCatLabs/pscat-mcp/issues/17) |

Claude Code 2.1.322 speaks the 2026-07-28 stateless protocol: no
`initialize` and no session, a `server/discover` probe, then `tools/list`
and `tools/call`, each with `MCP-Protocol-Version: 2026-07-28`, `Mcp-Method`
and `Mcp-Name` headers and the client info in `params._meta`.

**Known failures.** A request can carry `"known_failure": {"issue": 17,
"status": 406}`. Its `expect` (by default 2xx and a JSON-RPC result) is
what it must do once fixed. The test passes while the server still returns
that status, fails with a changed status, and **fails when the request
starts to pass**, so whoever fixes #17 deletes `known_failure` from
`accept-json-only.json` in the same change and the fixture then guards the
fix.

### Capture mode and recording a fixture

`serve-http --capture-file PATH` (`PSCAT_MCP_CAPTURE_FILE`) is off by
default. When set, the server appends one JSON line per `/mcp` request that
passed authentication and the body limit: the method and path, the values
of `Accept`, `Content-Type`, `MCP-Protocol-Version`, `Mcp-Session-Id`,
`Mcp-Method`, `Mcp-Name`, `User-Agent` and `Host`, the names (not values)
of all other headers, the scheme of `Authorization` and nothing else of it,
the body (cut at 64 KiB), and the response status. It never records tokens
or the query string, and the file is created with mode 0600. The server
logs a warning at startup. Header names are lowercase because the server
sees them after HTTP parsing. A test sends tokens in the header, the query
string, a cookie and an API-key header and checks none appears in the file.
The file contains the PostScript sources clients send, so it is for local
debugging only; the container's root filesystem is read-only, so capture
there needs a mounted writable path.

To record a client:

```bash
# the rmcp client (needs no credential)
PSCAT_EVAL_RECORD_DIR=/tmp/rec cargo test --test eval_replay \
  record_rmcp_client_traffic -- --ignored --nocapture
python3 -I evals/server/capture_to_fixture.py /tmp/rec/rmcp-client.capture.jsonl \
  --id rmcp-client --client-name rmcp --client-version 3.9.0 \
  --how "..." -o evals/server/fixtures/rmcp-client.json

# Claude Code: one tiny prompt, subscription only, behind the billing guard
cargo build --release
python3 -I evals/server/record_claude.py --out evals/server/fixtures/claude-code.json

# any other client: start the server with --capture-file, point the client
# at it, then convert the capture the same way (and pass --secret TOKEN)
```

`record_claude.py` reuses `evals/agent/guard.py` and the agent harness's
server and `claude` wrappers, and so inherits the guard: it refuses to run
unless `claude auth status` shows a claude.ai login, drops the API-key
environment variables, and stops if any event shows per-token billing. It
makes one `claude -p` call with a prompt that triggers a single tool call.
`capture_to_fixture.py` drops the `Host` and session headers and timings,
replaces tool-use ids, and refuses to write a fixture containing a
`--secret` value. Read a new fixture before committing it.

Not recorded yet: Codex (not run), ChatGPT, Claude.ai, Gemini, MCP
Inspector. Codex 0.167.1's binary contains the strings `Mcp-Method`,
`Mcp-Name` and `2026-07-28`, so it probably behaves like Claude Code.
