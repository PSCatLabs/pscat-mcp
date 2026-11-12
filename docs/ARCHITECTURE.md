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
                         mcp.rs  (rmcp #[tool] handlers: translate args/results only;
                                  PNG → image block, PDF → blob resource, SVG → text
                                  resources up to PSCAT_MCP_MAX_SVG_BYTES, else blobs)
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
just before rmcp sees it, without tokens), the `Accept` fix-up (`accept_json`
in `http.rs`: a POST whose `Accept` allows JSON but lacks `text/event-stream`
gets it added, because the server only ever answers JSON; see the README's
limitations), then rmcp. `render.rs` logs one
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
2. that every cell can have at least 100 ms of the sheet's time, after
   250 ms left for drawing the strips and allocating the sheet (the worker
   shares out what is left once that is done, so the two agree);
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
sheet stops with `timeout` and `error.input.code` `deadline`; that and the
kill at the deadline both count in `Counters::timed_out`.

**Memory.** PSCat's memory limit counts heap growth from when a cell's
limits are set (`Interp::set_limits` takes `memtrack::live_bytes()`, the
whole process's live heap, as its base), so the sheet already allocated
around a cell is in the base, not charged to the cell: a cell has exactly
the limit it would have as a render, and gives the same result alone or on
a large sheet (`tests/review_sheet.rs` checks this at a 64 MiB limit beside
a 61 MB canvas). The worker takes a cell's last page by moving the canvas
out, or by freeing it before it copies a shown page, so it holds no more
page rasters than a render does. What the sheet adds beyond a render is
outside every cell's limit and bounded by the pixel budgets: the canvas (4
bytes a sheet pixel, 64 MB at the default), the strips (about 10 MB at
most), the largest zoomed cell (no larger than the sheet), and, while the
PNG is encoded, a demultiplied copy of the canvas and the PNG. At the
defaults this fits the per-render figure in `docs/CLOUD_RUN.md` and is far
inside the Linux address-space limit (`max_memory + 1 GiB`);
`PSCAT_MCP_MAX_SHEET_PIXELS` is capped at 64,000,000 (8000 a side), which
adds about 200 MB.

**Failed cells.** A cell whose program raises an error or passes its
limit is drawn as far as it got (its last page or the canvas so far), and
the sheet succeeds. Its status in `structuredContent.cells[]` has the error
code, the limit, and PSCat's report cut to 240 bytes with control
characters removed, so 32 failed cells stay under 24 KB. Labels are not
echoed back.

## Async jobs

`src/jobs.rs` holds the job store; `RenderService` owns it
(`RenderService::jobs`, turned on with `with_jobs`) and runs the jobs, so
it is shared by every MCP handler: HTTP builds a `PscatMcp` per request,
and a store in the handler would lose a job between `start_render` and
`render_status`. Two shapes reach it from `mcp.rs`:

* the explicit tools `start_render`, `render_status`, `render_result` and
  `cancel_render` (the PSCat plugin's, without `out_path`), for
  `render_postscript`;
* the MCP Tasks extension (SEP-2663), with `--tasks`: in `call_tool`, a
  `tools/call` of `render_postscript`, `chalkboard_math` or `handwrite`
  whose request declares `io.modelcontextprotocol/tasks` (rmcp's
  `RequestContext::client_capabilities`: the request's `_meta`, or the
  session's `initialize` over stdio) goes to
  `RenderService::call_or_task`, which runs it synchronously first and
  makes it a job, answered with `CreateTaskResult`, only if it is still
  running after `--task-after-ms` (see "Tasks: only long calls" below);
  `get_task`, `cancel_task` and `update_task` on the `ServerHandler` read
  the same store. rmcp's protocol layer (dispatch,
  the capability checks, the task types) is used; its
  `task_manager::TaskManager` store is not, because it has no owner per
  task (any caller with an id can read or cancel it), keeps its entries
  with no hook for a byte budget, measures its TTL from creation and aborts
  a task still running at it (a long job would need a TTL of deadline plus
  queue time plus result time), and cancels only cooperatively.

**What a job runs.** Any render `WorkerJob` whose plan can take the job
deadline: the request is checked by `validate_request`/`validate_generate`
under the *job limits* (`JobsConfig::job_limits`: the render limits with the
job deadline as the render timeout, so PSCat's own time limit is computed
from it, and the job step limit, 25·10⁹ by default; `RLIMIT_CPU` follows
the deadline passed to the backend).
`review_sheet` is excluded: its plan shares the sheet deadline out between
the cells, and its result is a `SheetToolOutput`, not the
`RenderToolOutput` `render_result` declares.

**Running.** A job is a Tokio task, detached from the call that started it.
A drop guard in the task finishes the job as `worker_failed` if the task
panics or is dropped (the runtime shutting down) before it records an end,
so the job leaves the caps and its reservation is released.
It waits for a running-job permit (`--max-running-jobs`, below the render
slots so a synchronous call always has one), then for a render slot from
the same FIFO semaphore as every other call, without the queue timeout, and
only then counts its deadline. Cancel (`cancel_render`, `tasks/cancel`,
shutdown) fires the job's token; the task drops the render future, and the
backend's `Worker` drop kills the worker's process group: the same path as a
client's `notifications/cancelled`. A queued job never starts. Shutdown (the
stdio session's end, or the HTTP grace period's) kills every unfinished job
and waits up to 2 s; jobs are not drained.

**Tasks: only long calls (#57).** SEP-2663 leaves task creation to the
server, per request: it MAY answer a task-capable `tools/call` with a
`CreateTaskResult` instead of the standard result, the client MUST accept
either, and the client does not signal a preference on the request. So a
Tasks client's call runs synchronously first, and only a long one becomes
a task:

1. The request is checked under the job limits and takes a render slot the
   synchronous way (`slot()`, with the queue timeout). The worker starts
   with the job deadline and the job step limit, because a worker's limits
   (its deadline, `RLIMIT_CPU`, PSCat's step and time limits) are fixed
   when it starts.
2. The render is one boxed future, owned by the call, polled against the
   threshold (`--task-after-ms`, from when the slot was taken) and the
   request's cancellation. A call that finishes first returns the tool's
   result; the job store is never touched, so it takes no running-job
   permit, counts against no cap and reserves nothing. A
   `notifications/cancelled` drops the future, which kills the worker, as
   for a synchronous call.
3. At the threshold, `promote` takes a running-job permit without waiting
   (the call holds a render slot, and a synchronous call must always have
   one), then `JobStore::insert_since` checks the caps and reserves the
   largest result, with the job's elapsed time counted from the render's
   start. The job is `running` at once, and the same future, with its
   worker and render slot, moves into a job task (`drive_job`, the same
   runner `start_job` uses, with its drop guard), which selects on the
   job's own token only: from then on `tasks/cancel` and shutdown stop it,
   and the request's token does not. There is one worker and one
   `executed` count, and the reservation is released by the job's one
   `finish`. `CreateTaskResult` goes back only after the job is in the
   store, as the extension requires.
4. If the promotion is refused (no permit free, a cap, the budget, the
   store closed), the call keeps the future and waits for it until the
   sync deadline, which the server enforces here (`timeout`, counted in
   `timed_out`). Such a call ran under the job step limit, so it can reach
   the sync deadline (`timeout`) where a plain synchronous call might have
   stopped at the sync step limit (`limit_exceeded`).

A threshold of zero is the old behaviour: every eligible call is a job at
once (`start_job`, queued for a running-job permit with no queue timeout).
`JobsConfig::check` requires the threshold to be below the sync deadline
when `--tasks` is on, and, with a threshold above zero, the job limits to be
no tighter than the sync ones (job deadline at least the sync deadline, job
step limit at least the sync one, 0 meaning none): a quick call runs under
them and must not be held to less than a synchronous call. The two other options in #57 were rejected: a
per-call opt-in has no place in the protocol (the server decides; the
client sends no preference), and a smaller reservation for task-originated
jobs would still put quick calls through the caps, the running-job share
and the job deadline.

**Ownership and ids.** The owner is the principal. Each handler is built
for one transport (`PscatMcp::new` for stdio, `PscatMcp::for_http`), so the
handler, not the shape of the request, decides: over stdio the single local
user; over HTTP the `auth` layer's `Principal` (`token#N`, never token
material), read from the `http::request::Parts` rmcp puts in the request
extensions, and a request with no `Parts` or no `Principal` is refused.
`token#N` is the token's position in the list read at startup; it is stable
because the list never changes while the process runs (and a restart loses
every job). If tokens are ever reloaded at run time, the owner has to become
stable per token, for example an HMAC of the token under a key generated at
process start. Under `--allow-unauthenticated` (loopback only) every caller
is `anonymous`, one shared owner. A lookup by any other owner is the same
`unknown_job` (Tasks: rmcp's `invalid_params` "unknown task") as an unknown
id. Ids are UUID v4.

**Limits.** Unfinished jobs per principal and in all (`server_busy`). A
results budget (at the defaults about 11 reservations fit in 128 MiB, so
the budget binds before the total cap of 16): a job reserves the largest result it can have
(`max_output_bytes` as base64, the stdout limit and 4 KiB) when it is
accepted, and once finished keeps what it holds (its base64 artifacts and
stdout, or its error, plus 4 KiB); a job that does not fit is
`results_budget_exceeded`, after results already fetched are dropped,
oldest first. One principal may reserve and hold at most its share
(`PSCAT_MCP_JOB_RESULTS_PER_PRINCIPAL_BYTES`), so it cannot fill the budget
for the others. It is a separate setting, not a fraction, so an operator
can state it in bytes beside the budget; by default it is the budget less
one largest result (about 117 of 128 MiB), so another principal can always
start a job while a single principal (stdio, one token) keeps nearly all
of the budget; a budget under two largest results cannot protect a second
principal whatever the share, so then it is the whole budget. A share
must hold one largest result and be at most the budget (equal turns it
off), checked at startup when jobs are on. A refusal names the limit in
`error.input` (`code` `results_per_principal_bytes` or
`results_budget_bytes`, `limit_bytes`, `reached_bytes`). Eviction order:
the requester's own fetched results first, oldest first, then anyone
else's; a principal over its share gives up only its own. A job that
cannot fit even after every fetched result is dropped is refused before
anything is dropped. A cancelled job has no result to deliver, so once it
has stopped it is evictable like a fetched one: its 4 KiB of bookkeeping
stays charged (which bounds how many records a start-and-cancel loop can
leave behind) but is dropped first when the share or the budget is short,
so cancelling never leaves a principal at its share unable to start. The
charge is kept rather than released at cancel time for that bound, and the
record stays visible (`render_status` says `cancelled`) until room is
needed or the TTL passes; `cancel_job` treats a job dropped between two
of its polls as cancelled. Unfetched results (done or failed) are never
dropped early. A finished job is kept for the result TTL after it
finished; the store is swept on every access.

**States.** `queued`, `running`, `done`, `failed` (the render's own error:
a PostScript error, a limit, the deadline), `cancelled`. In the Tasks
extension queued and running are `working`; done and failed are both
`completed`, the result being the tool's result with or without `isError`
(the extension keeps `failed` for JSON-RPC errors); cancelled is
`cancelled`. A task's `ttlMs` is the job deadline plus the result TTL, its
`pollIntervalMs` 1000.

## Agent guidance (prompts and resources)

`src/guide.rs` serves the PSCat plugin's skills (issue #39). It runs no
PostScript and starts no worker: everything it returns is text compiled into
the binary.

**Where the text comes from.** `guide/skills/` is a byte-for-byte copy of
`plugin/skills/` at the PSCat commit `Cargo.lock` pins, with PSCat's
`LICENSE-MIT` and `LICENSE-APACHE` beside it and `guide/SOURCE.json`
recording the repository, tag, version, commit, path, licence, copyright and
file list. `scripts/vendor-guide.py` writes all of it. It finds the checkout
through `cargo metadata`, so it reads the same checkout the build compiles,
and it refuses any file that PSCat's own licence inventory
(`licenses/assets.json`, first match wins) does not list as MIT OR
Apache-2.0. All 14 files are covered by its catch-all `code` entry, with
copyright "Jerome Banks and the pscat contributors". `scripts/bump-pscat.sh`
runs it after every pin move, and the **Bump PSCat** workflow commits
`guide/` with `git add -A`, so added and removed files are kept.

The files are vendored, not read from the git dependency at build time.
Cargo's git checkout happens to hold the whole repository, `plugin/`
included. PSCat's `include` allow-list keeps `plugin/` out of the packaged
crate, though, so the day PSCat moves to crates.io an embed from the
dependency would silently lose the skills. The vendored copy can't drift
unnoticed either: `tests/guide.rs` compares it byte for byte with the
checkout's `plugin/skills/`, and checks that `SOURCE.json` names the
`Cargo.lock` commit and the `Cargo.toml` tag.

`build.rs` lists `guide/skills/` and writes `$OUT_DIR/guide_files.rs`, one
`include_str!` per file, so a bump that adds a file is served without a code
change. An empty or missing `guide/skills/` fails the build.

**What is served.**

* `resources/list` and `resources/read`: one `text/markdown` resource per
  file at `pscat://guide/<path under plugin/skills>`. The URIs mirror the
  layout, so relative links such as `references/paint.md` and
  `../SKILL.md` resolve against them. This is also the shape the SEP-2640
  skills extension (rmcp's `enable_skills`) expects of a `SKILL.md` URI;
  this server doesn't declare that extension. The descriptions are neutral
  ("<title> (plugin/skills/<path> from PSCat <tag>, with a note ...)"), not
  the skills' "Use when ..." frontmatter: Codex shows resource descriptions
  to its model, where they would act as instructions.
* `prompts/list` and `prompts/get`: `diagram`, `generative_art` and
  `chart`. Each returns one user text message: a task written here (from
  the prompt's arguments), then one guide document exactly as
  `resources/read` serves it. All arguments are optional, because Claude Code
  passes one whitespace-separated token per argument. Each argument is
  checked: only declared names, at most `PSCAT_MCP_MAX_PROMPT_ARGUMENT_BYTES` (8 KiB), and `format`, `kind`, `seed`
  and `palette` from fixed sets or shapes. One document per prompt keeps a
  reply under 24 KB; `pscat` and `psart` together would be about 29 KB.
* Errors are JSON-RPC errors, not tool results: an unknown URI is `-32002`
  (resource not found, with `data.uri`; rmcp sends `-32602` instead to a
  2026-07-28 client, which is what that version specifies); an unknown prompt, an unknown
  argument or a bad value is `-32602` (invalid params, with `data.name` and
  `data.argument`).

**The note.** PSCat's text was written for the plugin, which runs the
`pscat` CLI on the user's machine. The decision is to serve it verbatim,
traceable to the PSCat revision, rather than rewrite it. Each served document
is a Markdown blockquote written by this server, a `---` rule, then the file
unchanged. The note says:

* where the text comes from (path, tag, full commit, copyright, licence);
* this server's tools, from the tool router as this transport configured
  it, and which of the plugin's ten are missing (`eval_postscript`, and the
  four render job tools where async jobs are off, as on HTTP by default),
  with `validate_postscript` standing in for `eval_postscript`;
* how `pscat` CLI options map to tool arguments (and `pscat handwrite`,
  `chalk` and `sheet` to their tools, when present), and that the others
  have no equivalent;
* that output is inline only (no `out_path`, nothing saved), reads are
  confined to `lib/`, and `render_postscript` lacks the plugin's `bands`,
  `bbox` and `out_path` (taken from its input schema);
* the render deadline;
* one line for each of a few files (`FILE_NOTES`; a unit test checks that
  each key is a vendored file).

Because the lists come from the router and the schema, the render job tools
(#33) are listed as available where async jobs are on (stdio, or HTTP with
`--async-jobs`) and as missing where they are off, as `review_sheet` (#38)
dropped out when it landed. The
served text is the note followed by the file, so a test can check that the
file is a byte-exact suffix.

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
| Output bytes | worker (artifact cap: `output_too_large`, `error.input.code` `output_bytes`) + server (stdout cap, kill) | 8 MiB |
| SVG returned as text (all pages together; at most the output cap) | server, when it builds the result: up to the limit each page is an embedded text resource, above it an embedded blob resource (base64). Not a failure; the worker holds SVG to the output cap, from the SVG text's length before any page is encoded (`output_too_large`, `error.input.code` `output_bytes`) | 40 KiB |
| Pages returned | worker | 10 |
| Program stdout returned | worker (truncates) | 16 KiB |
| Concurrent renders | server semaphore, queue timeout → `server_busy` | min(CPUs, 4) |
| Client cancel / disconnect | dropping the future kills the worker (`kill_on_drop`) | — |
| Async job deadline | server: SIGKILL of the worker; PSCat's time limit and `RLIMIT_CPU` follow it | 10 min |
| Async job steps | PSCat `Limits::max_steps` | 25·10⁹ |
| Async jobs running | job semaphore, below the render slots; then a shared render slot, no queue timeout | half the slots |
| Async jobs held | per principal and in all (unfinished), `server_busy` | 4, 16 |
| Async job results | byte budget (largest result reserved while a job runs), TTL after it finishes | 128 MiB, 1 h |

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
  any request and nothing is stored between requests, except async jobs
  when `--async-jobs` turns them on (see [Async jobs](#async-jobs)).

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

* **Another transport** (REST): call `RenderService::render`/`validate`
  (and `start_job`, `jobs()`, `cancel_job` for async jobs).
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
