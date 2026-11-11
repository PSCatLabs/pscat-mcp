# PSCat MCP

[![CI](https://github.com/PSCatLabs/pscat-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/PSCatLabs/pscat-mcp/actions/workflows/ci.yml)
[![Container image](https://github.com/PSCatLabs/pscat-mcp/actions/workflows/container.yml/badge.svg)](https://github.com/PSCatLabs/pscat-mcp/actions/workflows/container.yml)

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
* Stateless: no sessions, no storage, any instance serves any request. The
  one exception is the optional async render jobs (`start_render`), kept in
  the server's memory: always on over stdio, opt-in over HTTP for a single
  instance (see [Long renders](#long-renders-async-jobs)).
* Untrusted programs run in a short-lived, resource-limited worker process
  per render (see [Security](#security-model)).

## Relationship to PSCat

PSCat (`PSCatLabs/pscat`) is the renderer. This repository doesn't change
it; it depends on PSCat's Rust library at a release tag:

| | |
|---|---|
| Crate | `pscat` 0.7.0-rc.4 (`default-features = false`) |
| Source | `https://github.com/PSCatLabs/pscat`, tag `v0.7.0-rc.4` |
| Commit | `df39114f1223cf475c0c2350c47d7475274c72ee` (recorded in `Cargo.lock`) |

It is a git dependency because crates.io only has 0.6.3, which lacks the
`Interp::set_limits` and `Interp::set_read_roots` APIs this server relies on.
`get_renderer_info` and every render result report the running version.

The PSCat plugin's skills (`plugin/skills/` at the same commit) are vendored
unchanged in [`guide/`](guide/), with the tag and commit in
`guide/SOURCE.json`, and served as [prompts and resources](#prompts-and-resources).

pscat-mcp makes a release after each PSCat release.

* `scripts/bump-pscat.sh <tag>` or the **Bump PSCat** workflow moves the pin
  and refreshes `guide/` from it (`scripts/vendor-guide.py`).
  The workflow runs daily and on demand, and opens a pull request.
* Tagging `v<version>` publishes the image.

The steps are in [`docs/RELEASING.md`](docs/RELEASING.md).

PSCat also ships its own `pscat-mcp` binary (a hand-written stdio server
around the `pscat` CLI with more art-oriented tools). This project is the
separate, cloud-oriented server built on the official SDK, with HTTP,
authentication and container packaging.

## Tools

| Tool | What it does | Annotations |
|---|---|---|
| `render_postscript` | Runs a program and returns its pages. Args: `source` (required), `format` `png`\|`svg`\|`pdf` (default `png`), `width`/`height` in points (default US Letter 612×792), `dpi` 9–1200 (default 72), `halftone` boolean (default false; PNG only, screens the raster like a mono laser printer into black and white dots; with `svg` or `pdf` it fails with `invalid_request`). | read-only, non-destructive, idempotent, closed-world |
| `chalkboard_math` | Writes a LaTeX formula on a chalkboard (PSCat's `pscat chalk`): hand-drawn chalk strokes, eraser marks, the ghost of earlier writing. Args as in the PSCat plugin: `latex` (required, at most `PSCAT_MCP_MAX_LATEX_BYTES`), `size` (points, default 64), `irregular` 0–5 (default 1), `slope` −45–45 degrees, `jitter` 0–0.5, `align` `center`\|`left`, `color` (`white`, `yellow`, `pink`, `blue`, `green`, `orange`), `board` (`true` a slate, the default; `false` chalk alone on a transparent page; `"slate"`, `"greenboard"`, `[r, g, b]` or an object of board keys), `ghosting` 0–1 or `false` (default 0.4), `seed` (default 1), `width`/`height` in points (default 800×400), `dpi` (default 144: 1600×800 px), `format` `png`\|`svg`\|`pdf`, and `config`, the full `pscat chalk --json` board config, with the other arguments laid over it. Returns the board like `render_postscript` returns a page, with the same envelope; SVG and PDF are returned inline too (the plugin writes them to `out_path`; this server has none). A bad board setting is `invalid_request`; a TeX command outside mathkit's subset is `postscript_error`, with `error.input.code` (`unsupported_command`, ...) and `error.input.token` naming it. The LaTeX is parsed and the program generated in the worker, under the render limits. | same |
| `handwrite` | Writes text as a handwriting-style note (PSCat's `pscat handwrite`, the `/HandScript` face): hand-drawn letters with per-letter variation, word-wrapped, on a page whose height is fitted to the text. Args as in the PSCat plugin: `text` (required, at most `PSCAT_MCP_MAX_HANDWRITE_BYTES`), `size` points 0–2000 (default 36), `width` points 1–8000 (default 612), `paper` `plain`\|`ruled` (default plain), `ink` `"R,G,B"` each 0–1 (default `0.13,0.14,0.34`), `jitter` 0–10000 (default 16), `seed` (default 1985; the same arguments give the same bytes), `dpi` 9–1200 (default 72). PNG only. The face has `a`–`z` (capitals are written as lowercase), digits and `. , - ' ! ?`; any other character leaves a gap, and text with none of them is `invalid_request`. These are hand-drawn styles for notes, cards and art, not for imitating a specific person's handwriting. The note's height is known only after the text is laid out, in the worker: a note past `PSCAT_MCP_MAX_PIXELS` is `too_many_pixels`, past 8000 pixels (or 8000 points) a side `invalid_dimensions`, each before anything is drawn and with `error.input.code` `height`. Returns the note like `render_postscript` returns a page, with the same envelope. | same |
| `review_sheet` | Composes one labelled PNG from several programs, each rendered at several resolutions, to compare candidates side by side (PSCat's `pscat sheet`; the layout, label and caption strips, zoom and frames are PSCat's own). Args as in the PSCat plugin, without `out_path`: `entries` (required, 1 to `PSCAT_MCP_MAX_SHEET_CELLS` rows), each with `source` (required), `label` (at most 200 characters; default `program N`), `width`/`height` in points 1–8000 and a `dpi` list; top-level `width`/`height` (default 612×792), `dpi` (default `[72]`), `labels` and `sizes` (default true). A `dpi` item is a number 9–1200, or `{"dpi": 9, "zoom": 4}` to draw that render enlarged with nearest-neighbour sampling (2–16). Each cell is drawn at its true pixel size with a hairline, the label above the row and `WxH px` under each cell; a program that shows several pages gives its last one. A cell whose program fails (a PostScript error, or its own step, time or memory limit) is drawn as far as it got and the sheet still succeeds. Every budget (cells, source bytes in all and per entry, each cell's sides, the cells' pixels, the sheet's pixels and sides, time enough for every cell) is checked before a worker starts, with `error.input.code` naming it (`cells`, `sources`, `source`, `deadline`, `cell_side`, `cell_pixels`, `sheet_side`, `sheet_pixels`). The whole sheet is one worker job in one render slot, with its own deadline (`PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`); each cell runs on a fresh interpreter with the render's step and time limits and at most an equal share of the time left. Returns one PNG image block and a text line per cell; `structuredContent` has the sheet's size and, for every cell, its row and column, dpi, zoom, render size, place on the sheet and, if it failed, its error code and a bounded PSCat report (under 24 KB with 32 failed cells). | same |
| `validate_postscript` | Tokenizer pass, then executes the program in the same sandbox and discards the output. Returns the PostScript error (name, offending command, line), lint findings (blank page, stack leak, unbalanced `gsave`), page count and stdout. | same |
| `describe_art_capabilities` | Lists the creative capabilities of the linked PSCat build: fonts, Type 3 faces, mood palettes, page templates, artkit and style-pack procedures, native operators, config dials. The whole catalog is about 280 KB, so a reply is one bounded view (under 24 KB, in both `content` and `structuredContent`, which carry the same information; some clients, Claude Code among them, give the model only the `structuredContent` JSON). No arguments: an index of every kind with its names, and installed font packs. `kind` (`font`, `type3_face`, `palette`, `template`, `procedure`, `operator`, `dial`): that kind's names with a one-line description each, 100 at a time (`offset`, `limit` 1–100). `names` (up to 20, optionally narrowed by `kind`): full entries with parameters, defining library file, `run` lines that load it, example and availability; unknown names are listed under `not_found`, and names that do not fit the reply under `omitted`. Runs no PostScript. The catalog is read once in a worker process, so it matches what renders can reach, and kept for the life of the server; later calls take no render slot. Server file paths are not reported. | same |
| `get_renderer_info` | PSCat version, formats, limits in force, capability notes. Runs nothing. | same |
| `start_render` | Starts `render_postscript` as a background job, for a render past the sync deadline: the same arguments (checked here, before a job is accepted), and it returns a `job_id` (a random UUID) at once with state `queued`. The job runs under the job deadline (`PSCAT_MCP_JOB_TIMEOUT_MS`, 10 minutes) and every other render limit. Only with async jobs on (always over stdio; `PSCAT_MCP_ASYNC_JOBS` over HTTP). A cap or the results budget refuses it with `server_busy` or `results_budget_exceeded`. | not read-only, not idempotent, non-destructive, closed-world |
| `render_status` | A job's `state` (`queued`, `running`, `done`, `failed`, `cancelled`), `elapsed_seconds`, `timeout_seconds`, and `job_error` (code and message) for a failed job or `expires_in_seconds` for a finished one. Under 2 KB. An unknown, expired or other client's `job_id` is `unknown_job`. | read-only, idempotent |
| `render_result` | A finished job's result: what `render_postscript` returns (the same content blocks and envelope, within the same output caps), or the failed render's error with `isError`. A queued or running job is `job_not_finished`, a cancelled one `cancelled`. It can be fetched again until the job expires. | read-only, idempotent |
| `cancel_render` | Cancels a job: a queued one never starts, a running one's worker is killed. Returns the job's status (`cancelled` once it has stopped); a finished job is left as it is. | not read-only, idempotent, non-destructive |

### Output formats

| Format | Returned as | MIME type |
|---|---|---|
| `png` | one MCP `image` content block per page | `image/png` |
| `svg` | one embedded text resource per page while all pages together are at most `PSCAT_MCP_MAX_SVG_BYTES` (40 KiB); above that, one embedded blob resource (base64) per page, up to `PSCAT_MCP_MAX_OUTPUT_BYTES` | `image/svg+xml` |
| `pdf` | one embedded blob resource holding every page | `application/pdf` |

Every result also has a short text summary and `structuredContent`
(declared with an `outputSchema`) with `ok`, sizes, page count, program
stdout and the renderer version, and for each artifact its MIME type, size
and `returned_as` (`image`, `text` or `blob`). Artifact bytes are not
duplicated there.

A client decides what the model sees of a resource. Claude Code shows a
text resource as markup (up to about 50,000 characters) and saves a blob
resource as a file, an SVG as a `.svg` file, giving the model the path.
A small SVG therefore stays readable and editable as markup, and a large
one reaches the user as a usable file instead of failing. Measurements per
client are in [`docs/LOCAL_TESTING.md`](docs/LOCAL_TESTING.md), section 11.
Embedded resources use `urn:pscat:render:<uuid>:page:<n>` identifiers.
They are not URLs, and nothing is stored on the server.

Failures are tool results with `isError: true` and
`structuredContent.error.code`, one of: `invalid_request`,
`source_too_large`, `unsupported_format`, `invalid_dimensions`,
`too_many_pixels`, `postscript_error`, `limit_exceeded`, `timeout`,
`too_many_pages`, `output_too_large`, `server_busy`, `cancelled`,
`worker_failed`, and for async jobs `unknown_job`, `job_not_finished` and
`results_budget_exceeded`. A failed render never returns an image.

## Long renders: async jobs

A synchronous call is limited by `PSCAT_MCP_RENDER_TIMEOUT_MS` (20 s) and,
in practice, by the client's own tool-call timeout (Codex's is 60 s). For a
longer render there are two ways in, served by one job store in the render
service:

1. **The explicit tools**, as in the PSCat plugin: `start_render` returns a
   `job_id`, `render_status` polls it, `render_result` fetches the pages and
   `cancel_render` stops it. They are ordinary tools, so every client can
   use them. They run `render_postscript` only.
2. **The MCP Tasks extension** (SEP-2663, `io.modelcontextprotocol/tasks`),
   with `PSCAT_MCP_TASKS=true`. The server then advertises the extension,
   and a `tools/call` of `render_postscript`, `chalkboard_math` or
   `handwrite` that declares it in its client capabilities gets a task
   handle; `tasks/get` returns the tool's result once it is done, and
   `tasks/cancel` kills the worker. A call that does not declare it gets
   today's synchronous result. A task is a job: the explicit tools see it
   too. `review_sheet` always answers synchronously: its plan shares out its
   own deadline (`PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`) between its cells.

How it behaves:

* **Ownership.** A job belongs to the client that started it: the bearer
  token over HTTP (its position in the configured list, never the token),
  the local user over stdio. Any other client gets the same `unknown_job`
  (or "unknown task") as for an id that never existed. Tokens are read once
  at startup, so a position names the same token for the life of the
  process (and its jobs). With `PSCAT_MCP_ALLOW_UNAUTHENTICATED` (loopback
  only) and async jobs, every caller is `anonymous` and they all share one
  owner: any of them can see, fetch and cancel the others' jobs, and the
  per-client cap is shared.
* **Render slots.** A job holds one render slot while it runs, shared with
  synchronous calls. At most `PSCAT_MCP_MAX_RUNNING_JOBS` jobs run at once
  (half the slots by default, and always fewer than the slots, so a
  synchronous call always has one; with a single slot it is 1, and a running
  job then makes synchronous calls wait). More jobs wait `queued`, with no
  queue timeout; time queued does not count against the job deadline.
* **Caps and memory.** At most `PSCAT_MCP_MAX_JOBS_PER_PRINCIPAL` unfinished
  jobs per client and `PSCAT_MCP_MAX_JOBS` in all (`server_busy`). Results
  are held in memory: a job reserves its largest possible result
  (`PSCAT_MCP_MAX_OUTPUT_BYTES` as base64, plus stdout) when it is accepted
  and keeps what it produced once it finishes, all against
  `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES` (`results_budget_exceeded`). At the
  defaults a reservation is about 11 MiB, so the 128 MiB budget holds about
  11 unfinished jobs: past that, `results_budget_exceeded` comes before the
  total cap of 16 (`server_busy`), and the budget, not the cap, is what
  bounds memory. Raise the budget to about 180 MiB for the cap to bind
  first, or lower `PSCAT_MCP_MAX_OUTPUT_BYTES` to shrink each reservation. A
  finished job is kept for `PSCAT_MCP_JOB_RESULT_TTL_MS` (an hour, as in
  the plugin) and can be fetched again until then; when the budget is short,
  results already fetched are dropped first.
* **Deadline.** `PSCAT_MCP_JOB_TIMEOUT_MS`, 10 minutes by default: thirty
  times the sync deadline, while one job holds a slot and up to the memory
  limit for that long. The plugin allows an hour; set 3600000 to match.
  PSCat's own time limit and the worker's CPU rlimit follow it. A job's
  step limit is `PSCAT_MCP_JOB_MAX_STEPS` (25,000,000,000 by default, PSCat's
  own default for untrusted programs and the plugin's; the sync 5,000,000,000
  would stop a busy program long before the deadline). Every other limit
  (memory, pixels, pages, output) is the render's.
* **Shutdown.** Running jobs are killed (not drained) when the stdio client
  disconnects (closes the server's stdin) or after the HTTP grace period,
  and the server waits for them to stop before it exits; their results are
  lost.
* **Hosted HTTP.** Off by default (`PSCAT_MCP_ASYNC_JOBS=false`): with it
  off the four tools are not listed and the Tasks extension is not
  advertised. The store is in memory, so it only works on **one instance
  whose CPU stays allocated**: on Cloud Run with more than one instance a
  status call can reach an instance that never saw the job, a scale-to-zero
  loses every job, and with CPU allocated only during requests a job gets
  almost no CPU between polls. A shared store and somewhere the render gets
  CPU are tracked in the `pscat-cloud` design (see
  [`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md#async-render-jobs)).

Which clients send the Tasks capability (November 2026):

| Client | Declares `io.modelcontextprotocol/tasks` on `tools/call`? | How we know |
|---|---|---|
| Claude Code 2.1.322 (HTTP) | **No.** It declares it on `server/discover` and `tools/list`, not on `tools/call`, so it always gets the synchronous result. | verified: the capture in `evals/server/fixtures/claude-code.json` |
| Codex | unknown | no capture; the MCP extension matrix lists no support (docs only) |
| Claude.ai, ChatGPT | unknown | not tested; the community-maintained extension matrix on modelcontextprotocol.io gives no confirmed support (docs only) |
| rmcp 3.9 client | yes, when built with `ClientCapabilities::builder().enable_tasks()` | verified: `tests/jobs.rs` |

So the Tasks path is off by default and the explicit tools are the way in
for the clients in use today.

## Prompts and resources

PSCat's own plugin teaches an agent to write good PSCat PostScript with four
skills: `pscat`, `psart`, `psviz` and `functional-postscript`. This server
offers the same text through standard MCP features, for the client (or the
user) to use when it chooses. The tool descriptions and server instructions
stay neutral: they say the guides exist, not when to read them.

* **Resources** (`resources/list`, `resources/read`): each of the 14 files of
  PSCat's `plugin/skills/` is a `text/markdown` resource at
  `pscat://guide/<path>`: `pscat://guide/pscat/SKILL.md`,
  `pscat://guide/psart/references/paint.md`, and so on. The URIs mirror the
  plugin's layout, so the relative links in the text resolve. The largest
  read is about 18 KB. There are no resource templates or subscriptions.
* **Prompts** (`prompts/list`, `prompts/get`), every argument optional:

  | Prompt | Arguments | Attaches |
  |---|---|---|
  | `diagram` | `subject`, `format` (`png`\|`svg`\|`pdf`) | `pscat://guide/pscat/SKILL.md` |
  | `generative_art` | `subject`, `palette` (an artkit palette name), `seed` (integer) | `pscat://guide/psart/SKILL.md` |
  | `chart` | `kind` (`bar`, `line`, `area`, `scatter`, `pie`, `function`, `parametric`, `polar`, `surface`), `data` | `pscat://guide/psviz/SKILL.md` |

  `prompts/get` returns one user message: a short task written by this
  server (render with `render_postscript`, standard 35 fonts, refine after
  looking), then the attached document as `resources/read` serves it.
* **PSCat's text is not rewritten.** It was written for the plugin, which
  runs the `pscat` command line on the user's machine, and it mentions things
  this server doesn't have: `out_path`, the `pscat` CLI, `eval_postscript`,
  the render jobs when async jobs are off (`start_render` and the rest; on
  over stdio, off over HTTP by default), `bands`, `bbox`, and reading the
  user's files. Each document is served as
  a short note written by this server, then PSCat's text byte for byte. The
  note gives the source (tag, commit, path, copyright, MIT OR Apache-2.0) and
  what differs here. Its tool lists are computed from the running server's
  own tools as this transport configured them, so they stay true as tools
  are added or turned off.
* An unknown URI is JSON-RPC error `-32002` (resource not found; `-32602`
  for a 2026-07-28 client, as that version specifies); an unknown
  prompt, an unknown argument or a bad argument value is `-32602` (invalid
  params). Nothing here starts a worker.
* Every library the guides name (`lib/artkit.ps`, `lib/graph.ps`,
  `lib/dataviz.ps`, `lib/functional.ps`, the style packs, ...) loads in this
  server's worker, and every runnable `postscript` block in them renders
  (`tests/guide.rs`). Their examples that read a file, such as the JPEG
  background in `psart`, fail here with `invalidfileaccess`.

Which clients show prompts and resources, and how, is in
[`docs/LOCAL_TESTING.md`](docs/LOCAL_TESTING.md#prompts-and-resources-in-each-client).
In short: Claude Code shows prompts as slash commands and resources as `@`
mentions, and its model can read resources itself; Codex's model can read
resources but Codex has no prompts; Claude.ai lets the user attach both;
ChatGPT appears to use neither.

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

Logs go to stderr. Stdout carries only MCP messages. Claude Code (the name
`pscat-render` avoids a clash with PSCat's own `pscat` plugin):

```bash
claude mcp add pscat-render -- /absolute/path/to/target/release/pscat-mcp-server serve-stdio
```

[`docs/LOCAL_TESTING.md`](docs/LOCAL_TESTING.md) walks through testing with
Claude Code and Codex, over stdio and HTTP.

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
  `Authorization: Bearer <token>`. Clients should send
  `Accept: application/json, text/event-stream`; `application/json` alone,
  `*/*` or no `Accept` also work (see Limitations).
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
claude mcp add --transport http pscat-render https://your-host.example/mcp \
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
and needs no writable filesystem. It is about 76 MB. A local build targets the
host architecture.

CI publishes the `linux/amd64` image for Cloud Run to
`ghcr.io/pscatlabs/pscat-mcp` on every push to `main` and every `v*` tag,
after the container smoke test passes against that image. Tags:

* `sha-<commit>`
* `main` and `latest`
* `<version>`
* `pscat-<pscat version>`

[`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md) explains how Cloud Run can pull it.

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
| `PSCAT_MCP_MAX_LATEX_BYTES` | 4096 | largest `chalkboard_math` formula (`source_too_large`) |
| `PSCAT_MCP_MAX_PROMPT_ARGUMENT_BYTES` | 8192 | longest value of one guide prompt argument (`invalid_params`); see [Prompts and resources](#prompts-and-resources) |
| `PSCAT_MCP_MAX_HANDWRITE_BYTES` | 65536 | largest `handwrite` text (`source_too_large`); also raises the HTTP request-body limit if larger than the source limit |
| `PSCAT_MCP_MAX_SHEET_CELLS` | 32 | cells (dpi items over all rows) in one `review_sheet` (the plugin's cap) |
| `PSCAT_MCP_MAX_SHEET_SOURCE_BYTES` | `PSCAT_MCP_MAX_SOURCE_BYTES` | source bytes of one `review_sheet`, all entries together (each entry is also held to the source limit); also raises the HTTP request-body limit if larger than the source limit |
| `PSCAT_MCP_MAX_SHEET_CELL_PIXELS` | `PSCAT_MCP_MAX_PIXELS` | device pixels the cells of one `review_sheet` render, all together, before zoom |
| `PSCAT_MCP_MAX_SHEET_PIXELS` | `PSCAT_MCP_MAX_PIXELS`, at most 64000000 | device pixels of a composed `review_sheet` (and 8000 a side, so at most 64000000); the sheet's canvas, 4 bytes a pixel, is held outside the cells' memory limits (see `docs/CLOUD_RUN.md`) |
| `PSCAT_MCP_MAX_SHEET_TIMEOUT_MS` | `PSCAT_MCP_RENDER_TIMEOUT_MS` | wall-clock deadline of one whole `review_sheet`; its worker is SIGKILLed after it |
| `PSCAT_MCP_RENDER_TIMEOUT_MS` | 20000 | wall-clock deadline; worker is SIGKILLed after it |
| `PSCAT_MCP_MAX_STEPS` | 5000000000 | PSCat step limit |
| `PSCAT_MCP_MAX_MEMORY_BYTES` | 536870912 | PSCat heap-growth limit per render |
| `PSCAT_MCP_MAX_PIXELS` | 16000000 | device pixels per page |
| `PSCAT_MCP_MAX_PAGES` | 10 | pages per render |
| `PSCAT_MCP_MAX_OUTPUT_BYTES` | 8388608 | inline output per render (PNG, PDF, and SVG as well) |
| `PSCAT_MCP_MAX_SVG_BYTES` | 40960 (40 KiB, or the output limit if that is smaller) | SVG per render returned as text resources, all pages together; a larger SVG is returned as blob resources (base64), held to `PSCAT_MCP_MAX_OUTPUT_BYTES`. 0 returns every SVG as a blob. May not exceed `PSCAT_MCP_MAX_OUTPUT_BYTES` (the server refuses to start) |
| `PSCAT_MCP_MAX_CONCURRENT_RENDERS` | min(CPUs, 4) | renders at once |
| `PSCAT_MCP_QUEUE_TIMEOUT_MS` | 10000 | wait for a slot before `server_busy` |
| `PSCAT_MCP_ASYNC_JOBS` | `false` | `serve-http` only: serve async jobs (`start_render` and companions). In memory: one always-on instance only (see [Long renders](#long-renders-async-jobs)); always on over stdio |
| `PSCAT_MCP_TASKS` | `false` | also serve the MCP Tasks extension on top of the jobs (needs async jobs) |
| `PSCAT_MCP_JOB_TIMEOUT_MS` | 600000 | wall-clock deadline of one job's worker (SIGKILL after it); PSCat's time limit and the CPU rlimit follow it |
| `PSCAT_MCP_JOB_MAX_STEPS` | 25000000000 (0 if `PSCAT_MCP_MAX_STEPS` is 0) | PSCat step limit of a job |
| `PSCAT_MCP_MAX_RUNNING_JOBS` | half the render slots, at least 1 | jobs running at once (more wait queued); must stay below `PSCAT_MCP_MAX_CONCURRENT_RENDERS` (1 with one slot) |
| `PSCAT_MCP_MAX_JOBS_PER_PRINCIPAL` | 4 | queued or running jobs one client (token, or the stdio user) may have |
| `PSCAT_MCP_MAX_JOBS` | 16 | queued or running jobs in all |
| `PSCAT_MCP_JOB_RESULT_TTL_MS` | 3600000 | how long a finished job and its result are kept |
| `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES` | 134217728 | memory for job results, all jobs together; at least one largest result (`PSCAT_MCP_MAX_OUTPUT_BYTES` × 4/3 plus stdout and 4 KiB); job settings are checked only when jobs are on |
| `PSCAT_MCP_LOG_FORMAT` | `json` (http) / `pretty` (stdio) | log format; `RUST_LOG` sets the level |
| `PSCAT_MCP_CAPTURE_FILE` | unset (off) | debug, `serve-http` only: append every `/mcp` request (allow-listed headers, no tokens, no query string, body cut at 64 KiB) as a JSON line to this file, to record a client's traffic for the replay fixtures; the file also holds the PostScript sources sent, so it is refused unless the bind address is loopback, and never for production |

## Authentication

The HTTP server will not start without at least one bearer token, unless
`PSCAT_MCP_ALLOW_UNAUTHENTICATED=true` is set **and** the bind address is
loopback. Authentication uses only the request headers and runs before rmcp
sees the request, so an unauthenticated request cannot reach a render. A
rejected request's body is read and discarded (up to 8 MiB and 5 s, as for
an oversized one), so a client still uploading gets the `401` rather than a
connection reset.
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
   (≤ 8000 px a side) and pixel count, and that `halftone` goes only with PNG;
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
  `output_too_large`, with `error.input.code` `output_bytes` and the limit
  and the size reached in `error.input.limit_bytes` and `reached_bytes`. No
  links are returned.
* **SVG output is large, so a large SVG is a blob.** PSCat draws text as
  glyph outlines (paths, not `<text>` elements) and a gradient as many
  separate shapes, so an SVG grows quickly: an 88 KB SVG came from a
  600x400 pt scene with a gradient sky, about 60 stars, some curves and
  three short strings (#31), and a program that draws thousands of marks
  is megabytes. Claude Code shows a text result inline only up to about
  50,000 characters and saves a larger one as a JSON array in a `.txt`
  file, whatever `MAX_MCP_OUTPUT_TOKENS` says (#64). So an SVG up to
  `PSCAT_MCP_MAX_SVG_BYTES` (40 KiB by default, summed over every page of
  one render) comes back as text resources the model can read, and a
  larger one as blob resources, which Claude Code saves as `.svg` files
  byte for byte (measured from 40 KB to 1 MB, #63). One result never mixes
  the two. The model then sees the file's path, not the markup; a model
  with a file tool can open it. Until #63 an SVG over the limit (128 KiB
  then) failed with `output_too_large` and `error.input.code` `svg_bytes`;
  that code is gone. What claude.ai, Claude Desktop and other clients do
  with an SVG blob has not been measured; raise the limit to keep more SVG
  as text for a client that shows long text results. Codex shows the model
  only `structuredContent`, so it sees neither form.
* **macOS has no address-space cap** on the worker; memory there is bounded
  by PSCat's allocator limit and the deadline.
* **Worker network namespace.** The worker shares the host's or container's
  network namespace. PSCat cannot use it, but for public deployment add a
  kernel sandbox (gVisor, which Cloud Run provides, or seccomp).
* **Survival after a server kill.** If the server is SIGKILLed itself, a
  running worker lives until its CPU or time limit, a few seconds.
* **Fonts.** The fonts compiled into PSCat are available (base-35
  equivalents, Liberation, TeX Gyre). Downloadable font packs are not
  supported or tested. `describe_art_capabilities` reports the faces PSCat's
  font scan finds in the worker's environment (an empty one, so `PSCAT_HOME`
  is not used), which with a standard install are the compiled-in ones.
* **Chalkboard vector output is large.** A board's SVG and PDF keep every
  chalk grain as a vector mark: the default board is about 0.3 MB as PNG,
  2.6 MB as PDF and 7 to 10 MB as SVG. A board's SVG is always over the
  default `PSCAT_MCP_MAX_SVG_BYTES`, so it comes back as a blob, and a
  default board's is near or over `PSCAT_MCP_MAX_OUTPUT_BYTES`
  (`output_too_large`, `error.input.code` `output_bytes`); a 400x200
  board is about 5 MB. PNG is the practical format. A LaTeX formula near `PSCAT_MCP_MAX_LATEX_BYTES` can take
  several seconds; one that does not finish in the render timeout is
  `timeout`.
* **`handwrite` sizes the note in the worker.** The page height depends on
  how the text wraps, so the server cannot know it (or check it against the
  pixel limits) until the worker has laid the text out. The layout pass runs
  under the render's step, time and memory limits and takes its share of
  them; the draw gets the rest. A text of hundreds of thousands of lines (needs a raised
  `PSCAT_MCP_MAX_HANDWRITE_BYTES`) is laid out and refused
  (`invalid_dimensions`), not drawn. The plugin refuses an empty
  text only; here a text of only whitespace, or with no character the face
  can draw, is `invalid_request` rather than a blank note, and `ink`
  components outside 0–1 are refused rather than clamped.
* **`review_sheet` runs every cell in one worker.** The plugin starts one
  `pscat` child per cell; here a sheet is one worker job in one render slot,
  its cells run one after another, each on a fresh thread and interpreter
  with its own limits. A crash in one cell (a stack overflow, an abort) loses
  the whole sheet (`worker_failed`), and a single long operator that PSCat
  cannot interrupt runs into the sheet's deadline (`timeout`). Each cell gets
  at most an equal share of the time left, so a runaway cell cannot starve
  the ones after it; the plugin gives each cell whatever is left of the call.
  The plan sizes the label strips at an upper bound (1.1 em a character), so
  a sheet near the pixel or side limit whose labels are wider than its cells
  can be refused where the plugin would draw it; `labels: false` avoids that.
  Labels in Korean, Japanese, Chinese or Thai need PSCat's `cjk-thai` font
  pack, which the worker's empty environment does not find with a standard
  install: they are drawn as `?`. A label is text only, never PostScript.
* **Async jobs are in memory.** They live in the server process: lost on
  a restart, killed at shutdown, and on HTTP usable only on a single
  always-on instance (off by default there). `render_status` has no
  `pages_done` (the plugin counts pages as `showpage` ends them; this
  server's worker reports its pages only at the end). `review_sheet` does
  not run as a job.
* **The guides were written for the PSCat plugin.** They are served
  unchanged, with a note on what differs here (see
  [Prompts and resources](#prompts-and-resources)); an agent that follows a
  `pscat --png` command or an `out_path` example literally has to map it
  to a tool call.
* **`validate_postscript` executes the program.** PSCat has no separate
  static checker beyond its tokenizer, so validation runs the program under
  the same limits.
* **Client testing.** Tested with the rmcp 3.9 client over stdio and HTTP
  (it negotiates 2025-11-25), with raw JSON-RPC over stateless HTTP at
  2025-03-26, 2025-06-18 and 2026-07-28 (sessionless, with `_meta` and the
  `Mcp-Method`/`Mcp-Name` headers that version requires), and with Claude
  Code 2.1.322 over HTTP (its traffic is recorded in
  `evals/server/fixtures/` and replayed in CI; it speaks 2026-07-28). Not
  yet tested with Codex (its fixture is synthesized), ChatGPT, Gemini, Muse
  or MCP Inspector. The guide prompts and resources are tested with the
  rmcp client and raw JSON-RPC only; how clients show them is from their
  docs and source, not tried in a client.
* **`Accept` header.** The spec asks an HTTP client to list both
  `application/json` and `text/event-stream`, and rmcp alone answers 406 to a
  POST that does not. This server always answers a POST with a JSON body, so
  it adds the missing type for any POST whose `Accept` allows JSON:
  `application/json` alone, `*/*`, `application/*` and no `Accept` header at
  all are served (RFC 9110: no header, or `*/*`, accepts anything). A POST
  whose `Accept` does not allow JSON (`text/html`, `text/event-stream` alone,
  `application/json;q=0`) is still 406. `GET` and `DELETE` are unchanged (a stateless server has no stream to open:
  `GET` is 405 whatever `Accept` says). Authentication, the Host and Origin
  checks and the body limit run first, so a request without a valid token is
  401 whatever it sends.

## Tests

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all              # unit + integration; all use the real PSCat worker
scripts/container-smoke.sh    # needs Docker
scripts/container-eval.sh     # server-layer evals against the image; needs Docker
scripts/agent-eval.sh --test  # offline unit tests of the agent evals
python3 -I -m unittest discover -s evals/server -t evals/server   # recorder tests
```

GitHub Actions runs these on every pull request and push to `main`:

* `.github/workflows/ci.yml`:
  * fmt and clippy;
  * the test suite on Linux and macOS;
  * a `cargo check` with the minimum Rust version in `Cargo.toml`;
  * the agent evals' offline unit tests (no model);
  * the offline unit tests of the server evals' recorder.
* `.github/workflows/container.yml`: the amd64 image, the smoke test, and
  the server-layer evals against that image.

| Suite | Covers |
|---|---|
| `tests/backend.rs` | real PNG/SVG/PDF renders with pixel checks, a generated page held to its plan (a `handwrite` note's height laid out in the worker within a pixel budget, a note over the budget refused before it is drawn, a page that misses the plan a bad job), deadline kill and no surviving process, cancel kills the worker, `halftone` in the worker job, PSCat step and memory limits, file read/write refused, output caps, crash/protocol errors, the child's rlimits/env/cwd, the worker's art catalog, a `review_sheet` composed in one job and its exact size checked against the budget before any cell runs |
| `tests/mcp_stdio.rs` | rmcp client over stdio: initialize, tool list (one `EXPECTED_TOOLS` table), annotations and schemas, the art catalog (index, kind pages and named entries with size ceilings, argument errors, no server paths, load lines usable in a render), PNG image content with pixel checks, `halftone` (pure black-and-white pixels, tone kept, every page, refused for SVG/PDF), SVG/PDF resources, PostScript errors without images, pre-execution rejections, argument type errors with the structured envelope, validation, renderer info checked against `Cargo.lock` |
| `tests/svg_cap.rs` | how SVG is returned, over stdio: up to `PSCAT_MCP_MAX_SVG_BYTES` a text resource, over it a blob resource holding the same SVG byte for byte (`returned_as` in `structuredContent`, every key checked against the output schema, the text summary naming the limit), every page counted together and never mixed, exactly at the limit still text, 0 for every SVG a blob, PNG and PDF unaffected, an SVG over the output cap `output_too_large` with `error.input.code` `output_bytes` and its whole size, the limit in `get_renderer_info` and the tool descriptions, its default pulled down by a small output cap, and a limit above the output cap refused at startup |
| `tests/chalkboard.rs` | `chalkboard_math` over stdio: the tool's schema matches the plugin's arguments, a default 1600×800 board (dark slate, bright chalk, same image twice), `board: false` transparent and an RGB board, PDF inline, SVG as text with the limit raised and as a blob by default, and over the output cap `output_too_large` with board advice, the LaTeX size limit, an unsupported command as `postscript_error` naming it, bad board configs and the pixel limit before execution, argument type errors with the envelope, and pathological LaTeX (10,000 nested `\frac`, 50,000 nested braces, an `\underbrace` chain) ending in a structured error while the server keeps serving |
| `tests/handwrite.rs` | `handwrite` over stdio: the tool's schema matches the plugin's arguments, a default 612×146 note with ink pixels in the ink colour, ink, size, width, dpi and `paper: ruled` (ruled lines and margin rule, none on plain paper), the same seed twice giving identical bytes, the text limit, a note too tall for the pixel cap or the side limit as a structured error (and a 500 000-line text refused, not killed) with the server serving the next call, the measuring pass under the step limit (and a limit hit inside a megabyte word keeping the reply small), argument errors through the envelope, and empty, whitespace-only, non-Latin and control-character text |
| `tests/review_sheet.rs` | `review_sheet` over stdio: the schema matches the plugin's arguments (no `$ref`, the cell cap in it), a 2×2 sheet of real programs with each cell at its true size, colour, hairline and position checked by pixel probes, each cell byte for byte the `render_postscript` page of its program (72, 144 and 100 dpi), nearest-neighbour zoom, labels and captions turned off (exact sheet sizes), a failing cell drawn as far as it got with its error while the others render, a runaway cell stopped at its own step limit and at its share of the sheet's time while the rest render, the output limit, every budget refused before execution (and nothing executed, by the service's counters), argument errors through the envelope, a reply under 24 KB with 32 failed cells, the worker's own sheet deadline as a `timeout` envelope valid against the output schema (and counted), a sheet at the pixel limit (16,000,000 and 64,000,000) with one cell zoomed 16x, a cell's memory limit giving the same result alone and beside a 62 MB sheet, and odd programs and labels never crashing the server |
| `tests/jobs.rs` | async jobs with the real worker and short deadlines: a job past the sync deadline completes and its PNG is fetched (twice), states seen in order (`queued` behind a running job, `running`, `done`), a failed job keeps its PostScript error, `cancel_render` and `tasks/cancel` kill the worker (by `pgrep` and the counters) and a queued job never starts, the job deadline kills and reports `timeout`, an expired result is `unknown_job`, the per-client and total caps and the results budget with their codes, argument errors through the envelope, shutdown kills running jobs, the job tools listed only with jobs on (also over HTTP), and the guide note naming them as missing exactly when they are not listed, another bearer token sees `unknown_job` with the same message as an unknown id, and the Tasks extension: a Tasks-capable rmcp client gets a task handle (render and `handwrite`), a plain one or one on a server without `--tasks` the sync result, and over HTTP a later `tasks/get` (a new handler) finds the task for its token only; a `start_render` job whose SVG is over `PSCAT_MCP_MAX_SVG_BYTES` (set through the environment, over stdio) ends `done` and `render_result` returns it as a blob resource holding the whole SVG, and `render_result` returns SVG as text or as a blob by the limit, like `render_postscript` |
| `tests/guide.rs` | prompts and resources over stdio: `guide/` is byte for byte `plugin/skills/` of the PSCat checkout cargo resolves, and `guide/SOURCE.json` names the commit in `Cargo.lock`; `initialize` advertises prompts and resources; every resource listed and read (the note, then PSCat's text verbatim; under 24 KB; the note lists exactly this server's tools); every prompt with and without arguments; unknown URIs and prompts and bad arguments as MCP errors; every runnable `postscript` block in the guides renders, and every library they name loads |
| `tests/mcp_cancel.rs` | `notifications/cancelled` for an in-flight render kills its worker and frees the render slot |
| `tests/http.rs` | 401 without or with a wrong token (and zero renders executed), rmcp HTTP client render, legacy protocol clients over stateless HTTP, bounded concurrency with correct per-request output, `server_busy`, loopback-only unauthenticated mode, Host and Origin checks, `Accept` handling (`application/json` alone, `*/*`, `application/*` and none are served as JSON; `text/html`, SSE-only and `q=0` stay 406, GET stays 405; 401, 403 and 413 come first), body limit (413 with `Content-Length` and for chunked bodies, with no render; the limit follows the largest of the source, `handwrite` text and `review_sheet` source limits), client disconnect kills the worker and frees the slot, guide prompts and resources through the rmcp client and as raw stateless requests at 2026-07-28 and 2025-06-18 (no worker started) |
| `tests/eval_server.rs` + `evals/server/scenarios/` | server-layer evals, no model: JSON scenario files (the agent evals' format) run with the rmcp client over stdio, over HTTP with a bearer token, over HTTP with async jobs on (`PSCAT_MCP_ASYNC_JOBS=true`, so scenario 66 runs there too) and, with `scripts/container-eval.sh`, in the container image. Steps call a tool or send another request (`resources/read`, `prompts/get`, ...). Exact assertions on `isError` and the error code, JSON-RPC errors, content block and MIME types, PNG size and pixel samples, SVG and PDF signatures, `structuredContent` fields, elapsed-time bounds and the number of renders executed: the first prompt in LOCAL_TESTING, every scenario in `examples/agent-scenarios.md`, and every error code (`timeout`, `limit_exceeded`, `server_busy`, ...) except `cancelled` (`tests/mcp_cancel.rs`). Prints a table and writes JSON and JUnit to `target/eval/`. See [Server-layer evals](docs/LOCAL_TESTING.md#10-server-layer-evals) |
| `tests/eval_replay.rs` + `evals/server/fixtures/` | replay of recorded client traffic against the in-process server: what Claude Code and the rmcp client really sent (headers, `initialize`/`server/discover`, `tools/list`, `tools/call`) plus synthesized cases, each marked real or synthesized; asserts 2xx and valid JSON-RPC. The `Accept: application/json`-only fixture (#17) is an ordinary passing case. Also tests capture mode: no token, query string or unlisted header value reaches the file |
| `evals/agent/` (`scripts/agent-eval.sh`) | local and manual only: real agents (Claude Code headless on the claude.ai subscription, optionally Codex) run the scenarios in `examples/agent-scenarios.md` and the LOCAL_TESTING first prompt over stdio and HTTP, checking tool choice, verbatim arguments, results, error codes and the answer; a billing guard refuses API-key billing. CI runs only its offline tests (guard and transcript checks on recorded fixtures). See [Automated agent evals](docs/LOCAL_TESTING.md#9-automated-agent-evals) |

## Contributing

[`SDLC.md`](SDLC.md) describes how changes reach `main`: issues with
`priority:`, `difficulty:` and `model:` labels, a branch per issue, the
required CI checks, review, and squash merges.

## License

MIT OR Apache-2.0. PSCat is (MIT OR Apache-2.0) AND OFL-1.1 AND LPPL-1.3c
because of its embedded fonts; see the PSCat repository.
