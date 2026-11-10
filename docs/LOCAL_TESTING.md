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

The commands below use `$BIN`. Clients store the path you give them, so it
must be absolute.

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
`/mcp`. `pscat-render` should be connected with three tools.

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

This is the transport a hosted deployment uses, so test it too. In its own
terminal:

```bash
export PSCAT_MCP_TOKEN="$(openssl rand -hex 32)"
PSCAT_MCP_AUTH_TOKENS="$PSCAT_MCP_TOKEN" PSCAT_MCP_LOG_FORMAT=pretty \
  "$BIN" serve-http        # 127.0.0.1:8080, endpoint /mcp
```

Leave it running and watch its log: every request is one line with its
status. A client that gets **406** is
[issue #17](https://github.com/PSCatLabs/pscat-mcp/issues/17) (a client
that doesn't send `Accept: text/event-stream`); note which client it was.

The token is a throwaway for this test. Clients store it in their own
configuration.

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

## 5. What to try

[`examples/agent-scenarios.md`](../examples/agent-scenarios.md) has five
prompts with the expected tool call and result: render a program, draw a
diagram, diagnose an invalid program, ask for an unsupported format, and
try to exhaust resources. Beyond those:

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

## 6. What to report

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

## 7. Clean up

```bash
claude mcp remove -s user pscat-render
claude mcp remove -s user pscat-render-http
codex mcp remove pscat-render
codex mcp remove pscat-render-http
```
