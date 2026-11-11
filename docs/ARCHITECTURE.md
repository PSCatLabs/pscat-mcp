# Architecture

PSCat MCP is a thin, stateless MCP server in front of the
[PSCat](https://github.com/PSCatLabs/pscat) PostScript interpreter. This
document explains how a request becomes a render, where the security
boundaries are, and where to extend it.

## Request path

```
 MCP client
    │  Streamable HTTP (/mcp)            stdio
    ▼                                      ▼
 http.rs ── auth.rs (bearer) ──┐        mcp::serve_stdio
    request id, tracing,       │           │
    body limit, Host/Origin    ▼           ▼
                         mcp.rs  (rmcp #[tool] handlers: translate args/results only)
                               │
                               ▼
                         render.rs  RenderService
                           • validate_request: size, format, dpi, page px, pixel cap
                           • validate_generate: generate.rs plan (input size, board
                             config), then the same format/page/pixel checks
                             (for a note: its width; the worker checks the height;
                             for a review sheet: every sheet budget, in sheet.rs)
                           • semaphore: bounded concurrent renders + queue timeout
                           • maps worker/backend failures to stable ErrorCodes
                               │  WorkerJob (JSON)
                               ▼
                         backend.rs  SubprocessBackend
                           • spawn `pscat-mcp-server render-worker` per job
                           • env cleared, cwd "/", rlimits, own process group
                           • stdout/stderr caps, deadline → SIGKILL group + reap
                               │  stdin: job   stdout: WorkerResult (JSON)
                               ▼
                         worker.rs  (child process, no async runtime)
                           • Generate jobs: generate.rs writes the program here
                             (a review sheet: sheet.rs renders every cell and
                             composes the PNG here)
                           • pscat::Interp with Limits (steps/time/memory)
                           • reads confined to PSCat's compiled-in lib/
                           • no write roots, stdout captured, stderr silenced
                           • PNG / SVG / PDF encoding, page + byte caps
```

On HTTP the `/mcp` stack, outermost first, is: request id and tracing,
authentication, the body limit (six times the largest of the source,
`handwrite` text and `review_sheet` source limits, plus 64 KiB, so an input a little over its limit
still gets a structured error), the optional capture layer (`capture.rs`,
off unless `--capture-file` is set; it records each authenticated request
just before rmcp sees it, without tokens), then rmcp. `render.rs` logs one
`render started` line per job it hands to a worker; the server-layer evals
count those lines to assert how many renders a scenario executed.

Every layer below `mcp.rs` is transport independent. A REST API would be a
second caller of `RenderService` and reuse all validation, limits and error
codes.

## The renderer adapter

PSCat is used as a **Rust library** (`pscat` crate, git dependency pinned
to an exact commit, `default-features = false` so no window system is
linked). The supported API (`Interp`, `Page`, `PsError`, `Limits`) does all
rendering. Several `#[doc(hidden)]` PSCat modules are also used. All of them are
called from `src/pscat_extras.rs` and nowhere else (a PSCat bump that breaks
one is fixed in that file; upstream tracking issue
[PSCatLabs/pscat#1098](https://github.com/PSCatLabs/pscat/issues/1098)). They
are the first thing to re-check when bumping the pin:

* `pscat::lexer` — tokenizer pass for `validate_postscript`
  (PSCatLabs/pscat#1096).
* `pscat::gfx::PageSizeSource` — so a non-72-dpi default page still lets a
  program choose its size with `setpagedevice`, like `Interp::new()`
  (PSCatLabs/pscat#1097).
* `pscat::halftone`, `pscat::png` and the page rasters of `pscat::gfx`
  (`Interp::gfx`, `Gfx::pages`, `Gfx::pixmap`, `has_trailing_art`) — the screening behind
  `render_postscript`'s `halftone` option (`pscat --halftone`). The flag travels
  in the worker job (`WorkerJob.halftone`, PNG only: the request check refuses
  it for SVG and PDF) and the worker screens each page and encodes it, so the
  server process never touches the raster.
* `pscat::chalk`, `Gfx::set_transparent_background`, `pscat::pdf::scan_document_info`
  and `Gfx::set_pdf_info` — `chalkboard_math` (see [Generated
  programs](#generated-programs)). The board config is resolved
  (`ChalkBoard::from_args`) in the server as well, before any worker starts,
  to refuse a bad config and to size the board; it reads a bounded JSON
  object of fixed depth and never the LaTeX. This is the one `pscat_extras`
  call outside the worker. `chalk` needs no cargo feature
  (`default-features = false` only drops the window).
* `pscat::contact_sheet` and `pscat::png::encode` — `review_sheet` (see
  [Review sheets](#review-sheets)): the layout (`Layout`), the label and
  caption strips (`render_text`), nearest-neighbour zoom, the hairline
  frames and the sheet's PNG, as `pscat sheet` uses them; plus the page
  rasters of `pscat::gfx` again (a cell is its program's last page). The
  layout, which is arithmetic on sizes, also runs in the server, to check a
  sheet's budgets before a worker starts; nothing else does.
* `pscat::capabilities` — the art catalog behind `describe_art_capabilities`.
  It runs no PostScript. It lists fonts by scanning font directories, so the
  worker computes it (`JobMode::Capabilities`): the worker's environment is
  cleared and its working directory is `/`, which is what renders see, and the
  scan runs under the same deadline and rlimits as a render. It takes a
  render slot. `RenderService` keeps the parsed catalog after the first
  successful fetch (the worker's environment is fixed, so it is too; a failed
  fetch is not kept), so later calls start no worker. The font-pack status
  contains directories, which `pscat_extras` removes. The catalog is about
  280 KB, so `src/catalog.rs` filters it in the server into three bounded
  views (index, one kind's names with truncated descriptions, full entries
  for up to 20 names within a byte budget), each under 24 KB as structured
  content and as text.

## Generated programs

Some tools take input that is not PostScript and have PSCat write the
program: `chalkboard_math` turns LaTeX into a chalkboard program
(`pscat::chalk::driver`). That input is as untrusted as a program. A
pathological formula could take unbounded time, or recurse deeply, in the
generator itself, and in the server that would hang a Tokio thread or, with a
stack overflow, abort the process for every user. So generation is a worker
job like a render: `JobMode::Generate`, with the job's `generate` field
holding a `GenerateJob` (`src/generate.rs`), one variant per kind, on the
wire `{"kind": "chalkboard_math", "params": {...}}`.

Each kind has three hooks:

| Hook | Runs in | Does |
|---|---|---|
| `plan` | server, before a slot is taken | checks what needs no parsing of the untrusted input (its byte size; for a board, the board config) and returns a `PagePlan`: the dpi and either the whole page in points or only its width (see below). `validate_generate` then applies the render's format, dpi, side and pixel checks to what is known. A failure is a rejection like any other: no slot, no worker, counted in `rejected`. |
| `generate` | worker | writes the program and its page. The worker refuses a page that does not meet the plan (`worker_failed`), then runs the program through the ordinary render path: PNG, SVG or PDF encoding, the page and byte caps, and every interpreter and OS limit. |
| `explain` | worker | maps a PostScript error from the generated program to the input error it stands for (mathkit raises `undefined` naming `mk-unsupported-command:\foo`). Limit errors are never explained. |

**The page is a contract.** A plan promises one of two things
(`PlannedPage`), and the worker holds the generated program to it:

* `Fixed`: the whole page is known before the input is read (a chalkboard's
  size is its config). The server checks the page against the pixel limits;
  the worker refuses a program on any other page.
* `FixedWidth`: the width and dpi are fixed, and the height is decided by
  laying out the untrusted input (a note's height is its line count), so the
  server cannot know it. The server checks the width's pixels against the
  side limit and the pixel cap, and puts a `DecidedHeight` budget in the job
  (`WorkerJob.decided_height`: the width, the most pixels, the most pixels a
  side). The generator lays the input out, computes the height, and calls
  `DecidedHeight::admits` **before anything is allocated**; a page over the
  budget is a structured `too_many_pixels` (the area) or `invalid_dimensions`
  (the side limit), with `error.input.code` `height` and a message with the
  line count and the size it would have had. The worker then checks the
  returned page against the budget again (`worker_failed` if a generator ever
  let one through) and only then builds the render's interpreter, so the
  raster of an over-tall note is never allocated.

A generator may run PostScript of its own before the render (`handwrite`'s
measuring pass). That pass gets the render's whole sandbox
(`worker::apply_limits`: step, time and memory limits, reads confined to
`lib/`, no writes). A limit it hits is reported as the render's
`limit_exceeded`, with the same `postscript` diagnostic. What it spent
(`GeneratedProgram.spent`) is taken off the render's step and time limits,
and added to the result's `steps_used` and `elapsed_ms`, so the two programs
together stay within the limits instead of each getting all of them.

A generator failure travels back as `WorkerFailure::Generate`, with an
`ErrorCode` (`invalid_request` for input that is wrong before anything runs,
`postscript_error` for input the program could not draw), the generator's
own code and token (`error.input` in the envelope) and a one-line message.
The tool's envelope and content blocks are `render_postscript`'s.

For `chalkboard_math` the mapping is: a config error, or LaTeX that is
empty once comments and wrappers go, is `invalid_request`; a TeX error
(unsupported command, character or environment, a missing argument, an
unclosed `\(`) is `postscript_error` with PSCat's message, never the
PostScript report; any other error is the program's own `postscript_error`
or `limit_exceeded`. Deeply nested LaTeX ends in PSCat's
`execstackoverflow` (`postscript_error`), a long one in `timeout`; either
way only the worker is involved (`tests/chalkboard.rs`).

For `handwrite` (`src/handwrite.rs`) the plugin's tool runs `pscat handwrite`,
a subcommand of the `pscat` binary and not of the library; this server does
what it does from public pieces. `plan` checks the text's byte size
(`PSCAT_MCP_MAX_HANDWRITE_BYTES`, 64 KiB; the plugin's is 1 MiB) and every argument
(`HandwriteParams::check`: ranges, `paper`, and `ink`, which is three plain
decimals in 0 to 1 and can carry no code), and returns a `FixedWidth` plan.
`generate` runs `hs-linecount` from `lib/handscript.ps` on a default page
(the measuring pass), computes the height as the plugin does (two margins,
a first line of 1.05 em, 1.5 em for each further line), checks the budget,
and writes the program: the library, an options dictionary, `hs-write
showpage`. The text is lowercased, escaped for a string literal, and reaches
only the options dictionary; no hidden PSCat item is needed
(`(lib/handscript.ps) run` is within the read confinement). The render uses
a fresh interpreter, like the plugin's, so the same seed gives the same
bytes. A text with no drawable character, or only whitespace, is refused in
`plan`. There is no `explain` arm: the library raises no input errors.

To add a kind: a `GenerateJob` variant with its three arms, its hidden PSCat
calls in `pscat_extras`, a `PlannedPage` that says what is fixed, and a tool
in `mcp.rs` that builds a `GenerateRequest` and calls
`RenderService::generate`.

### Review sheets

`review_sheet` (`src/sheet.rs`) is a kind whose output is an image rather
than a program: `generate` returns `Generated::Sheet` (the PNG and a
report of every cell) instead of `Generated::Program`, the worker returns
that PNG as the job's one artifact, and the plan is a `PlannedPage::Sheet`
carrying a `SheetBudget` that travels in the job (`WorkerJob.sheet`). Its
arguments, defaults, caps (32 cells, 200-character labels, zoom 2 to 16),
layout and rule for failed cells are the plugin's, and the PNG is byte for
byte what `pscat sheet` writes for the same programs, pages, dpis and
labels.

**What runs where.** The programs and the labels are untrusted, so the
worker does all of the work: draws the label and caption strips (PSCat
lays the text out by running a program it writes), lays the sheet out
exactly, checks it against the budget again, allocates it, renders every
cell and encodes the PNG. The server only runs the plan, which reads the
arguments and checks every budget:

1. the cell count and the source bytes (in all, and each entry against the
   source limit), counted from the argument lengths before anything is
   allocated per entry or per cell;
2. that every cell can have at least 100 ms of the sheet's time;
3. each cell's sides after zoom (8000 pixels), then the pixels the cells
   render, all together (before zoom);
4. the sheet's sides (8000 pixels) and pixels, from PSCat's `Layout` with
   each label strip at an upper bound (1.1 em a character, 1.4 em tall;
   PSCat's widest glyphs are 1 em) and each caption at 0.75 em a character.

A refusal names its budget in `error.input.code` (`cells`, `sources`,
`source`, `deadline`, `cell_side`, `cell_pixels`, `sheet_side`,
`sheet_pixels`), takes no slot and starts no worker. Because the plan's
strips are upper bounds, a sheet the plan admits always fits once the worker
has the real strips; the worker's check is the backstop, before it
allocates the sheet or runs a cell. The cost is that a sheet near a limit,
with labels wider than its cells, can be refused where the plugin would draw
it.

**Slots and isolation.** A sheet is one worker job in one render slot,
under its own deadline (`PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`, which `RLIMIT_CPU`
follows), and its cells run one after another in that process. Each cell
runs on a fresh thread (8 MiB of stack, the main thread's; a fresh name
interner, so a cell is byte for byte a standalone render) with a fresh
interpreter under `worker::apply_limits`, the sandbox every render gets;
PSCat's memory limit counts heap growth from when a cell's limits are set,
so it is per cell. A panic is that cell's `worker_failed`, drawn blank. The
plugin runs each cell in its own `pscat` child instead. One job keeps the
slot accounting honest (one slot is one process, as for every other tool)
and the rasters in the process that composes them; the trade-off is that a
crash (a stack overflow, an abort) loses the whole sheet as
`worker_failed`, and a single operator PSCat cannot interrupt runs into the
sheet's deadline (`timeout`).

**Time.** The worker shares the sheet deadline, less the render margin
(10%, at least 250 ms) for composing and encoding, between the cells as it
goes. A cell may use at most the render's step limit, the render's
interpreter time limit, and an equal share of the time left (time left ÷
cells left). A runaway cell stops at its share with `limit_exceeded`, and
the cells after it keep theirs; time a quick cell does not use passes on.
(The plugin gives each child whatever is left of the call, so its first
runaway cell can starve the rest.) If a share ever falls under 100 ms, the
sheet stops with `timeout`.

**Failed cells.** A cell whose program raises an error or passes its
limit is drawn as far as it got (its last page or the canvas so far), and
the sheet succeeds. Its status in `structuredContent.cells[]` has the error
code, the limit, and PSCat's report cut to 240 bytes with control
characters removed, so 32 failed cells stay under 24 KB. Labels are not
echoed back.

## Why a child process

The library runs **inside a child process**, not in the server, because:

1. PSCat checks its limits *between* interpreter steps. One long operator (a
   huge fill, a deep path) runs to completion, so only an external kill can
   guarantee a deadline.
2. PSCat's memory limit measures heap growth of the whole process through
   `pscat::TrackingAllocator`. With one program per process the number is
   exact; with concurrent in-process renders it would be meaningless.
3. `pscat::Interp` is `!Send`; a process per job sidesteps thread pinning.
4. OS limits (`setrlimit`) and a crash (stack overflow, abort) are contained
   to the job.

The cost is a process start per render (a few milliseconds; the worker is
the same binary). A pre-forked pool is a possible later optimisation.

## Execution isolation and limits

| Control | Enforced by | Default |
|---|---|---|
| Source size | server, before execution | 256 KiB |
| `chalkboard_math` LaTeX size | server, before execution | 4 KiB |
| `handwrite` text size | server, before execution | 64 KiB |
| `handwrite` note height | worker, after layout, before the page is allocated: side limit, then pixel cap | — |
| `review_sheet` cells, sources (in all and each), cell sides and pixels, sheet sides and pixels, 100 ms a cell | server, before execution (the sheet again in the worker, exactly, before it is allocated) | 32, source limit, 8000 px, `PSCAT_MCP_MAX_PIXELS` |
| `review_sheet` deadline | server: SIGKILL of the worker; each cell at most the render's step and time limits and an equal share of the time left | render timeout |
| Format, `halftone` only with PNG, dpi (9–1200), page ≤ 8000 px a side | server, before execution | — |
| Device pixels of the starting page | server, before execution | 16 M |
| Pages created by `setpagedevice` | PSCat (`set_max_page_pixels`) | 16 M |
| Interpreter steps | PSCat `Limits::max_steps` | 5·10⁹ |
| Interpreter time | PSCat `Limits::max_time` | timeout − 10% |
| Heap growth | PSCat `Limits::max_memory` (counting allocator) | 512 MiB |
| Wall-clock deadline | server: SIGKILL of the worker's process group | 20 s |
| CPU time | kernel `RLIMIT_CPU` | deadline + 2 s |
| Address space | kernel `RLIMIT_AS` (**Linux only**) | memory + 1 GiB |
| File writes | PSCat (no write roots) + kernel `RLIMIT_FSIZE=0` | none allowed |
| File reads | PSCat read roots: only compiled-in `lib/` | — |
| Open files | kernel `RLIMIT_NOFILE` | 64 |
| Output bytes | worker (artifact cap) + server (stdout cap, kill) | 8 MiB |
| Pages returned | worker | 10 |
| Program stdout returned | worker (truncates) | 16 KiB |
| Concurrent renders | server semaphore, queue timeout → `server_busy` | min(CPUs, 4) |
| Client cancel / disconnect | dropping the future kills the worker (`kill_on_drop`) | — |

PSCat has **no operators for processes, network or environment access**
(see PSCat's `SECURITY.md`). The worker's environment is cleared anyway
(only `PSCAT_NO_CHECKOUT=1` is set), so nothing secret is inherited.

Known gaps:

* `RLIMIT_AS` is applied on Linux only. On macOS the memory bound is PSCat's
  allocator limit plus the deadline; a single huge allocation inside one
  operator can overshoot before it is seen.
* If the server process itself is SIGKILLed, a running worker survives
  until its `RLIMIT_CPU` or its own time limit stops it (seconds). In a
  container the whole cgroup is torn down with the instance.
* The worker shares the container's network namespace. PSCat cannot open
  sockets, but defence in depth (seccomp, a network-less sandbox such as
  gVisor, which Cloud Run gen1 provides) is recommended for public use.

## Security boundaries

* **Authentication** (`auth.rs`): every `/mcp` request passes an
  `Authenticator` before rmcp sees it. The provided one checks
  `Authorization: Bearer <token>` against SHA-256 digests of configured
  tokens in constant time. Unauthenticated mode exists for local development
  and the server refuses to start with it on a non-loopback address.
  `/healthz` is unauthenticated and does no work.
* **DNS rebinding / browsers**: rmcp's `Host` allow-list (loopback by
  default; set `PSCAT_MCP_ALLOWED_HOSTS` for a deployment) and `Origin`
  validation.
* **Logging**: PostScript source is never logged, only its byte length;
  tokens are never logged. The one exception is the opt-in capture mode
  (`--capture-file`), which records request bodies and tokens never, and is
  refused unless the bind address is loopback.
* **Statelessness**: Streamable HTTP runs without sessions
  (`legacy_session_mode = false`, JSON responses), so any instance can serve
  any request and nothing is stored between requests.

### Adding OAuth later

The MCP authorization spec makes the server an OAuth 2.1 *resource
server*. To add it:

1. Implement `Authenticator` to validate access tokens (JWT signature and
   `aud`/`iss`/`exp`, or token introspection) issued by an external
   authorization server (e.g. Google Identity Platform, Auth0).
2. Serve `/.well-known/oauth-protected-resource` (RFC 9728) naming that
   authorization server, and return `WWW-Authenticate: Bearer
   resource_metadata="…"` on 401 so clients can discover it.
3. Pass the authenticated principal to handlers through request extensions
   if per-user quotas are needed.

No user database or proprietary OAuth flow is needed in this repository.

## Extension points

* **Another transport** (REST): call `RenderService::render`/`validate`.
* **A tool whose input is not PostScript**: a `GenerateJob` kind (see
  [Generated programs](#generated-programs)); one whose output is an image
  made from several renders follows `review_sheet`.
* **Another backend** (a pool, a remote sandbox, a test double): implement
  `backend::RenderBackend`. Dropping the returned future must stop the job.
* **Artifact storage**: none is configured, so output larger than
  `PSCAT_MCP_MAX_OUTPUT_BYTES` fails with `output_too_large` instead of
  returning a link. A storage provider (e.g. GCS with signed URLs) would sit
  between `RenderService` and `mcp.rs`, returning MCP `resource_link`
  content only for objects it actually stored.
* **Authentication**: implement `auth::Authenticator`.
