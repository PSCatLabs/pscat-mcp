# PSCat MCP

A [Model Context Protocol](https://modelcontextprotocol.io) server that lets
AI agents run PostScript programs with the
[PSCat](https://github.com/PSCatLabs/pscat) interpreter and get the real
rendered result back: PNG images the client can display, SVG, or PDF.

Every image is produced by PSCat from the submitted program, not by an
image-generation model.

* Rust, the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)
  SDK (3.9), Tokio and Axum.
* Two transports sharing one tool implementation: **Streamable HTTP**
  (`serve-http`, for remote use) and **stdio** (`serve-stdio`, for local
  agents such as Claude Code).
* Stateless: no sessions, no storage, any instance serves any request.
* Untrusted programs run in a short-lived, resource-limited worker process
  per render (see [Security](#security-model)).

## Relationship to PSCat

PSCat (`PSCatLabs/pscat`) is the renderer. This repository does not change
it; it depends on PSCat's Rust library at an exact commit:

| | |
|---|---|
| Crate | `pscat` 0.7.0-rc.4 (`default-features = false`) |
| Source | `https://github.com/PSCatLabs/pscat` |
| Commit | `df39114f1223cf475c0c2350c47d7475274c72ee` (tag `v0.7.0-rc.4`) |

A git dependency is used because crates.io only has 0.6.3, which lacks the
`Interp::set_limits` and `Interp::set_read_roots` APIs this server relies
on. The running version is reported by `get_renderer_info` and in every
render result.

PSCat also ships its own `pscat-mcp` binary (a hand-written stdio server
around the `pscat` CLI with more art-oriented tools). This project is the
separate, cloud-oriented server built on the official SDK, with HTTP,
authentication and container packaging.

## Tools

| Tool | What it does | Annotations |
|---|---|---|
| `render_postscript` | Runs a program and returns its pages. Args: `source` (required), `format` `png`\|`svg`\|`pdf` (default `png`), `width`/`height` in points (default US Letter 612×792), `dpi` 9–1200 (default 72). | read-only, non-destructive, idempotent, closed-world |
| `validate_postscript` | Tokenizer pass, then executes the program in the same sandbox and discards the output. Returns the PostScript error (name, offending command, line), lint findings (blank page, stack leak, unbalanced `gsave`), page count and stdout. | same |
| `get_renderer_info` | PSCat version, formats, limits in force, capability notes. Runs nothing. | same |

### Output formats

| Format | Returned as | MIME type |
|---|---|---|
| `png` | one MCP `image` content block per page | `image/png` |
| `svg` | one embedded text resource per page | `image/svg+xml` |
| `pdf` | one embedded blob resource holding every page | `application/pdf` |

Every result also has a short text summary and `structuredContent`
(declared with an `outputSchema`) with `ok`, sizes, page count, program
stdout and the renderer version. Artifact bytes are not duplicated there.
Embedded resources use `urn:pscat:render:<uuid>:page:<n>` identifiers.
They are not URLs, and nothing is stored on the server.

Failures are tool results with `isError: true` and
`structuredContent.error.code`, one of: `invalid_request`,
`source_too_large`, `unsupported_format`, `invalid_dimensions`,
`too_many_pixels`, `postscript_error`, `limit_exceeded`, `timeout`,
`too_many_pages`, `output_too_large`, `server_busy`, `cancelled`,
`worker_failed`. A failed render never returns an image.

## Build

Requires Rust 1.89+ (developed with 1.96) and network access to GitHub for
the PSCat dependency. No C toolchain or system libraries.

```bash
git clone https://github.com/PSCatLabs/pscat-mcp && cd pscat-mcp
cargo build --release
# binary: target/release/pscat-mcp-server
```

## Run over stdio (local agents)

```bash
target/release/pscat-mcp-server serve-stdio
```

Logs go to stderr. Stdout carries only MCP messages. Claude Code:

```bash
claude mcp add pscat -- /absolute/path/to/target/release/pscat-mcp-server serve-stdio
```

or in a client's JSON configuration (Claude Desktop, Cursor, and others):

```json
{
  "mcpServers": {
    "pscat": {
      "command": "/absolute/path/to/target/release/pscat-mcp-server",
      "args": ["serve-stdio"]
    }
  }
}
```

## Run over Streamable HTTP

```bash
export PSCAT_MCP_AUTH_TOKENS="$(openssl rand -hex 32)"
target/release/pscat-mcp-server serve-http            # 127.0.0.1:8080, endpoint /mcp
```

* `POST /mcp`: MCP Streamable HTTP (stateless, JSON responses). Requires
  `Authorization: Bearer <token>`. Clients must send
  `Accept: application/json, text/event-stream`.
* `GET /healthz`: unauthenticated liveness check, `{"status":"ok"}`.

Client configuration for a remote server:

```json
{
  "mcpServers": {
    "pscat": {
      "type": "http",
      "url": "https://your-host.example/mcp",
      "headers": { "Authorization": "Bearer ${PSCAT_MCP_TOKEN}" }
    }
  }
}
```

```bash
claude mcp add --transport http pscat https://your-host.example/mcp \
  --header "Authorization: Bearer $PSCAT_MCP_TOKEN"
```

### Example render with curl

```bash
TOKEN=$PSCAT_MCP_AUTH_TOKENS
curl -s http://127.0.0.1:8080/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"render_postscript","arguments":{
        "source":"%!PS\n1 0 0 setrgbcolor 20 20 80 60 rectfill\n0 0 1 setrgbcolor 150 50 30 0 360 arc fill\nshowpage",
        "width":200,"height":100}}}' \
  | python3 -c 'import sys,json,base64; r=json.load(sys.stdin)["result"]; \
      img=[c for c in r["content"] if c["type"]=="image"][0]; \
      open("out.png","wb").write(base64.b64decode(img["data"])); print(r["structuredContent"])'
open out.png   # a red rectangle and a blue circle, rendered by PSCat
```

More programs are in [`examples/`](examples/) (`shapes.ps`,
`flowchart.ps`), and [`examples/agent-scenarios.md`](examples/agent-scenarios.md)
lists agent-level acceptance cases.

## Docker

```bash
docker build -t pscat-mcp .
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 127.0.0.1:8080:8080 \
  -e PSCAT_MCP_AUTH_TOKENS="$(openssl rand -hex 32)" \
  pscat-mcp
scripts/container-smoke.sh   # build, run hardened, health check, auth, real renders, timeout
```

The image is distroless, runs as non-root (65532), binds `0.0.0.0:$PORT`
and needs no writable filesystem. It is about 76 MB. It builds for the host
architecture; Cloud Run needs `docker buildx build --platform linux/amd64 .`.

## Configuration

Every flag has an environment variable; see [`.env.example`](.env.example)
and `pscat-mcp-server serve-http --help`. The main ones:

| Variable | Default | Meaning |
|---|---|---|
| `PORT` / `PSCAT_MCP_BIND` | `8080` / `127.0.0.1` | listen address (the image sets `0.0.0.0`) |
| `PSCAT_MCP_AUTH_TOKENS` / `PSCAT_MCP_AUTH_TOKEN_FILE` | — | accepted bearer tokens (comma list / one per line) |
| `PSCAT_MCP_ALLOW_UNAUTHENTICATED` | `false` | no auth; **refused unless bound to loopback** |
| `PSCAT_MCP_ALLOWED_HOSTS` | `localhost,127.0.0.1,::1` | `Host` allow-list (DNS-rebinding protection); set to your public hostname, or `*` behind a proxy that checks it |
| `PSCAT_MCP_ALLOWED_ORIGINS` | empty (reject any `Origin`) | browser origins allowed |
| `PSCAT_MCP_MAX_SOURCE_BYTES` | 262144 | largest program |
| `PSCAT_MCP_RENDER_TIMEOUT_MS` | 20000 | wall-clock deadline; worker is SIGKILLed after it |
| `PSCAT_MCP_MAX_STEPS` | 5000000000 | PSCat step limit |
| `PSCAT_MCP_MAX_MEMORY_BYTES` | 536870912 | PSCat heap-growth limit per render |
| `PSCAT_MCP_MAX_PIXELS` | 16000000 | device pixels per page |
| `PSCAT_MCP_MAX_PAGES` | 10 | pages per render |
| `PSCAT_MCP_MAX_OUTPUT_BYTES` | 8388608 | inline output per render |
| `PSCAT_MCP_MAX_CONCURRENT_RENDERS` | min(CPUs, 4) | renders at once |
| `PSCAT_MCP_QUEUE_TIMEOUT_MS` | 10000 | wait for a slot before `server_busy` |
| `PSCAT_MCP_LOG_FORMAT` | `json` (http) / `pretty` (stdio) | log format; `RUST_LOG` sets the level |

## Authentication

The HTTP server will not start without at least one bearer token, unless
`PSCAT_MCP_ALLOW_UNAUTHENTICATED=true` is set **and** the bind address is
loopback. Authentication runs before the request body is read and before
rmcp sees the request, so an unauthenticated request cannot reach a render.
Tokens are stored as SHA-256 digests, compared in constant time, and never
logged. Mount tokens as a file (`PSCAT_MCP_AUTH_TOKEN_FILE`) rather than
passing them on the command line.

OAuth (the MCP authorization spec) can replace the bearer check by
implementing the `auth::Authenticator` trait and serving protected-resource
metadata; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#adding-oauth-later).

## Security model

PostScript is a programming language, so every program is treated as
untrusted code. Each render or validation:

1. is checked **before execution**: source size, format, dpi, page size
   (≤ 8000 px a side) and pixel count;
2. runs in a **new worker process** (`render-worker`, the same binary) with
   an empty environment, cwd `/`, its own process group, and rlimits: CPU
   time, no file writes (`RLIMIT_FSIZE=0`), 64 file descriptors, no core
   dumps, and on Linux an address-space cap;
3. runs under **PSCat's own limits**: steps, time and heap growth (exact per
   process via PSCat's counting allocator), plus a pixel cap on
   `setpagedevice`; file reads are confined to PSCat's built-in `lib/`, and
   there are no write roots;
4. is **SIGKILLed** at the wall-clock deadline, or when the client cancels
   or disconnects, even if it is stuck inside a single long operator;
5. has its output size, page count and stdout capped.

PSCat has no operators for processes, network or environment access. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full table (and [docs/CLOUD_RUN.md](docs/CLOUD_RUN.md) for deployment) of which
control is enforced by PSCat and which by the server.

## Current limitations

* **No artifact storage.** Output over `PSCAT_MCP_MAX_OUTPUT_BYTES` returns
  `output_too_large`. No links are returned.
* **macOS has no address-space cap** on the worker; memory there is bounded
  by PSCat's allocator limit and the deadline.
* **Worker network namespace.** The worker shares the host's or container's
  network namespace. PSCat cannot use it, but for public deployment add a
  kernel sandbox (gVisor, which Cloud Run provides, or seccomp).
* **Survival after a server kill.** If the server is SIGKILLed itself, a
  running worker lives until its CPU or time limit, a few seconds.
* **Fonts.** Only the fonts compiled into PSCat are available (base-35
  equivalents, Liberation, TeX Gyre). Downloadable font packs are not
  loaded.
* **`validate_postscript` executes the program.** PSCat has no separate
  static checker beyond its tokenizer, so validation runs the program under
  the same limits.
* **Argument type errors.** A wrong argument type, such as a negative
  `width`, is reported by rmcp as a text-only tool error without the
  structured envelope.
* **Client testing.** Tested with the rmcp 3.9 client over stdio and HTTP
  (it negotiates 2025-11-25), and with raw JSON-RPC over stateless HTTP at
  2025-03-26, 2025-06-18 and 2026-07-28 (sessionless, with `_meta` and the
  `Mcp-Method`/`Mcp-Name` headers that version requires). Not yet tested
  with ChatGPT, Claude, Gemini, Muse or MCP Inspector.
* **`Accept` header.** HTTP clients must accept both `application/json` and
  `text/event-stream`, as the spec requires. rmcp answers 406 otherwise.

## Tests

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all              # unit + integration; all use the real PSCat worker
scripts/container-smoke.sh    # needs Docker
```

| Suite | Covers |
|---|---|
| `tests/backend.rs` | real PNG/SVG/PDF renders with pixel checks, deadline kill and no surviving process, cancel kills the worker, PSCat step and memory limits, file read/write refused, output caps, crash/protocol errors, the child's rlimits/env/cwd |
| `tests/mcp_stdio.rs` | rmcp client over stdio: initialize, tool list, annotations and schemas, PNG image content with pixel checks, SVG/PDF resources, PostScript errors without images, pre-execution rejections, validation, renderer info |
| `tests/http.rs` | 401 without or with a wrong token (and zero renders executed), rmcp HTTP client render, legacy protocol clients over stateless HTTP, bounded concurrency with correct per-request output, `server_busy`, loopback-only unauthenticated mode, Host and Origin checks, body limit |

## License

MIT OR Apache-2.0. PSCat is (MIT OR Apache-2.0) AND OFL-1.1 AND LPPL-1.3c
because of its embedded fonts; see the PSCat repository.
