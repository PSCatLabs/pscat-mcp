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
 http.rs ── auth.rs (methods) ─┐        mcp::serve_stdio
    request id, tracing,       │           │
    body limit, Host/Origin    ▼           ▼
                         mcp.rs  (rmcp #[tool] handlers: translate args/results only;
                                  PNG → image block, PDF → blob resource, SVG → text
                                  resources up to PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES, else blobs;
                                  with `delivery` url/both, resource_link blocks to an
                                  artifacts.rs store instead or as well)
                               │
                               ▼
                         render.rs  RenderService
                           • validate_request: size, format, dpi, page px, pixel cap
                           • validate_generate: generate.rs plan (input size, board
                             config), then the same format/page/pixel checks
                             (for a note: its width; the worker checks the height;
                             for a review sheet: every sheet budget, in sheet.rs)
                           • semaphore: bounded concurrent renders + queue timeout,
                             and a per-principal share of the slots
                           • maps worker/backend failures to stable ErrorCodes
                               │  WorkerJob (JSON)
                               ▼
                         backend.rs  SubprocessBackend
                           • spawn `pscat-mcp-server render-worker` per job
                           • env cleared (PSCAT_HOME only if a font-pack dir is
                             configured), cwd "/", rlimits, own process group
                           • stdout/stderr caps, deadline → SIGKILL group + reap
                               │  stdin: job   stdout: WorkerResult (JSON)
                               ▼
                         worker.rs  (child process, no async runtime)
                           • Linux: seccomp filter (sandbox.rs) before the job is
                             read; no sockets, io_uring, ptrace (refuses without);
                             the server itself is non-dumpable
                           • names its filter ("seccomp" | "none") in every
                             WorkerResult (the `sandbox` field)
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
slots so a synchronous call always has one), then for its owner's share of
the slots (the per-principal cap, below), then for a render slot from
the same FIFO semaphore as every other call, without the queue timeout, and
only then counts its deadline. The task is spawned in the span of the
request that started the job, so its log lines (`render started`,
`render finished`) carry that request's `request_id` and, for `oauth`, its
`sub`; so does a promoted Task's. Cancel (`cancel_render`, `tasks/cancel`,
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
is `anonymous`, one shared owner. Other auth methods give ids prefixed
with their name (`oauth:` and 128 bits of a SHA-256 over issuer and
subject, pseudonymous), so ids never collide across methods
(see [Security boundaries](#security-boundaries)). A lookup by any other owner is the same
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

## Artifact delivery

`src/artifacts.rs` (issue #139, part 1 of #98). ChatGPT does not show its
model an inline `image` block (#138), so the render tools can return links.

**The argument.** `delivery` on `render_postscript`, `chalkboard_math`,
`handwrite`, `review_sheet` and `start_render`: `inline`, `url`, `both`,
`auto`. It is a plain string with an `enum` in the schema (no `$ref`), read
in the handler before the render; an unknown value, or `url`/`both` with no
store, is `invalid_request` with `error.input.code` `delivery`, before any
worker starts. With no argument the operator default applies
(`PSCAT_MCP_ARTIFACT_DELIVERY`); `ArtifactConfig::check` refuses to start
with a default other than `inline` when links are off, and checks the
store's own settings (for `gcs`: a bucket, the prefix, a TTL of at most 7
days, the signer and the test-only endpoint overrides). `auto` is decided after the render, on the
artifacts' total size against `PSCAT_MCP_ARTIFACT_THRESHOLD_BYTES`; with no
store it is `inline`.

**Inline is untouched.** `render_success` and `sheet_success` are the
`*_with(.., None)` forms of the builders; every field added for links is an
`Option` skipped when absent, so an inline result is the same JSON as before
#139. `tests/data/inline-results.json`, written by the unmodified builders,
pins it (a unit test in `mcp.rs`), and `tests/artifacts.rs` compares live
results with and without a store.

**With links.** One `ArtifactStore::put` per call stores every artifact
under one random `render_id` (UUID v4, from the OS CSPRNG through `uuid`) and
returns a link per artifact and an expiry. The result then carries each link
three times, because clients differ in what they pass to the model: a
`resource_link` block, `download_url` and `expires_at` on each `artifacts`
entry (the manifest; `render_id` at the top level; `returned_as` `link` when
the bytes are not inline) and a text line. Diagnostics (the envelope's sizes,
page count, stdout, a sheet's cells, any error) stay inline. A failed render
stores nothing; a store refusal is the tool's failure envelope.

**The trait.** `ArtifactStore` has `put` (async, boxed future: a cloud
backend uploads), `read` (bytes for `resources/read`; a signed-URL backend,
which serves nothing itself, keeps the default `None`), `ttl` and
`quota_bytes`. Each `ArtifactLink` has a `uri` (the `resource_link`), a
`download_url` and a `LinkScheme` (`Pscat` or `Https`). Both backends are
built from `ArtifactConfig`: the memory store by `RenderService` over its job
store (`with_artifacts`, rebuilt by `with_jobs`), the Cloud Storage store by
`build_service_with`, because its TLS client can fail the start, and
installed with `with_artifact_store`.

**The memory backend.** `MemoryArtifactStore` keeps the decoded bytes in the
`JobStore`'s ledger (`put_artifacts`, `read_artifact`), so stored output and
async-job results share one memory bound, `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES`,
also when async jobs are off. A stored render is charged its bytes plus
1 KiB, is never evicted for room (a link handed out keeps working until its
TTL, `PSCAT_MCP_ARTIFACT_URL_TTL_SECS`), and counts as held when a job
reserves; a `put` may drop already-fetched job results, as a job's insert
does. Each principal may hold `PSCAT_MCP_ARTIFACT_QUOTA_BYTES`; over it,
`results_budget_exceeded` with `error.input.code` `artifact_quota_bytes`,
and the budget full is `results_budget_bytes`. Expiry runs on an injectable
monotonic clock (`MemoryArtifactStore::with_clock`); expired renders are
swept on every store or job access. `resources/read` of
`pscat://render/<render_id>/<n>` looks the render up by id *and* owner (the
principal, as for jobs), so another principal's id, an expired one and an
unknown one are one JSON-RPC "resource not found" whose message does not
repeat the URI. Render URIs are not listed by `resources/list`.

Its limits are the async jobs': one process, lost on restart, invisible to
other instances. Like async jobs, memory links are refused at startup with
the `none` auth method (`http::check_none_with_artifact_links`): every
anonymous caller is the one principal `anonymous`, so one could fill the
results budget the links share with authenticated clients, and read the
others' links. Under
`--allow-unauthenticated` (loopback only) they start, and every caller shares
the one `anonymous` quota. Over stdio there is one principal, the local user.

**Async jobs and Tasks.** `start_render` resolves `delivery` like the other
tools (a refusal comes before a job is accepted) and records it on the job
(`JobStore::set_delivery`); it is applied when the result is fetched.
`render_result` and `tasks/get` store the result the first time and remember
the `StoredRender` on the job (`remember_links`), so a second fetch returns
the same `render_id` without charging the quota again; once those links have
expired the next fetch stores it again. A Tasks call that finishes before the
threshold applies the delivery to its synchronous result. Applying at fetch
time, rather than refusing `delivery` on `start_render`, keeps an operator
default of `url` working for jobs too.

**Logs.** No log line holds a link, a `render_id` or artifact bytes: the
tool layer logs no result, the store logs nothing (the Cloud Storage store
logs a failure as codes only, below), the not-found error does not echo the
URI, and capture mode records request bodies and headers with every
`pscat://render/<id>` (also JSON-escaped) redacted (`redact_render_links`)
and the query string of every `http(s)` URL replaced by `[redacted]`
(`redact_url_queries`), so a signed URL a client sends back never lands in
the file. Capture mode does not record response bodies at all.
rmcp itself logs whole requests, responses and notifications below WARN
(`rmcp::service` at DEBUG, INFO and TRACE; the raw line and a per-message line
in `rmcp::transport::async_rw` and `...::streamable_http_server::tower`), which
would log links like it logs PostScript sources, and signed URLs (#140) are
bearer capabilities. So `telemetry::rmcp_clamp`, a filter on the log layer
next to the `RUST_LOG` filter, holds every `rmcp` target to WARN and ERROR
whatever `RUST_LOG` says (`RUST_LOG=trace` included); our `pscat_mcp` targets
are untouched. rmcp's WARN and ERROR lines stay (`tests/logging.rs`).

### The Cloud Storage backend

`src/gcs.rs` (#140), `PSCAT_MCP_ARTIFACT_LINKS=gcs`. Only the server
process talks to Google; the worker gets no credential and no new
capability (its seccomp filter refuses sockets on Linux, which is what keeps
it from the metadata server: on Cloud Run that server answers every process
in the container).

`GcsArtifactStore::put`, for one call:

1. **Reserve** the call's charge (its bytes plus 1 KiB, as the memory store
   charges) against the principal's quota: bytes uploaded within the last TTL,
   in an in-memory ledger, so **per instance**. Over it,
   `results_budget_exceeded` with `artifact_quota_bytes`, before any network
   call. A call that fails before its first upload is refunded; one that
   fails after keeps its charge, since its objects may exist.
2. **Credentials.** An access token from the metadata server
   (`GET /computeMetadata/v1/instance/service-accounts/default/token`,
   `Metadata-Flavor: Google`, and the response must carry it too), cached
   behind an async mutex until 300 s before it expires (or half its life, if
   shorter), so one caller refreshes and the rest wait. The signer is
   `PSCAT_MCP_ARTIFACT_SIGNER`, or the metadata server's `default/email`,
   fetched once. No key, key file or credential variable exists.
3. **Upload** each artifact as `<prefix><render_id>/<n>.<ext>` with one
   `multipart` upload (`upload/storage/v1/b/<bucket>/o?uploadType=multipart&ifGenerationMatch=0`).
   Multipart because the metadata part sets `contentType` and
   `contentDisposition` (`inline; filename="pscat-<n>.<ext>"`) in the same
   request: a media upload followed by a metadata `PATCH` would need
   `storage.objects.update`, which the runtime account does not have.
   `ifGenerationMatch=0` makes an existing name a `412`, so nothing is ever
   overwritten (the `render_id` is a fresh UUID v4 anyway). The multipart
   boundary is random and drawn again if the bytes contain it.
4. **Sign** each object's V4 URL: the canonical request (`GET`, the
   path-style `/<bucket>/<object>`, the `X-Goog-Algorithm`, `-Credential`,
   `-Date`, `-Expires` and `-SignedHeaders=host` query, `host:storage.googleapis.com`,
   `UNSIGNED-PAYLOAD`) and the string-to-sign (`GOOG4-RSA-SHA256`, the
   time, the scope `<date>/auto/storage/goog4_request`, the canonical
   request's SHA-256) are built in `v4_string_to_sign`, checked against
   Google's conformance vectors in unit tests; IAM Credentials
   `projects/-/serviceAccounts/<signer>:signBlob` signs the string with the
   account's Google-managed key, and the signature is hex-encoded into
   `X-Goog-Signature`. One clock reading (injectable, whole seconds) gives
   `X-Goog-Date` and `expires_at`. The links point at
   `https://storage.googleapis.com` whatever the test-only storage override
   says, which only redirects uploads.

The HTTP client is the OAuth server's stack (hyper, hyper-rustls with ring
and webpki roots), typed over `Full<Bytes>` for the POSTs; no Google SDK.
Each call has its own timeout (`PSCAT_MCP_ARTIFACT_GCS_TIMEOUT_MS`), reads
at most 64 KiB of response and follows no redirect. Over all of a `put`
(token, signer, every upload and signature, one after the other: a page is
two calls, so a 10-page render is about 22) there is one deadline,
`PSCAT_MCP_ARTIFACT_GCS_DEADLINE_MS` (#149, 20 s by default): `put` wraps
its work in one `tokio::time::timeout`, so a degraded Google costs at most
the deadline, not the sum of the calls' timeouts, which could pass the Cloud
Run request timeout and show the client a platform timeout in place of
`storage_unavailable` and its fallback. The wait for the token lock and the
signer lock is inside it too (a caller behind a stalled refresh would
otherwise wait for each earlier caller's timeout in turn). The deadline
drops the work in flight, which releases a lock it holds; the failed call
returns no link, so objects already uploaded cannot be reached (the
lifecycle rule deletes them), and, as for any failure after an upload began,
the quota charge stays (a deadline before the first upload refunds it). The
calls are sequential on purpose; uploading and signing pages concurrently
would cut the latency but is not done. The three base URLs
(metadata server, IAM Credentials, Storage uploads) can be overridden for
tests only, by hidden flags (`PSCAT_MCP_ARTIFACT_GCS_*_ENDPOINT`) that must
be `https`, or `http` on a loopback host; the built-in metadata default is
plain `http://metadata.google.internal`, as the metadata server requires.

**Failures.** Any failure of those calls (status, timeout, transport, an
unexpected answer) is `storage_unavailable` (`ErrorCode::StorageUnavailable`,
added for this; retryable), logged once as a WARN with `code`, `stage`
(`credentials`, `upload`, `signing`; for a deadline, the stage it was in),
`failure` (`status`, `timeout`, `transport`, `bad_response`, or `deadline`
for the whole-upload deadline) and at most `status`. Its
message names the stage and status and says to retry or use `inline`; it
never holds a URL, the bucket, a token or a response body, and neither does
any `Debug` form (the token and the settings print redacted). In
`mcp.rs`, `store_output` and `job_result` turn `storage_unavailable` into an
inline result for `both` and `auto` (`fall_back_inline`), with a note in the
text and `structuredContent.delivery_note`; for `url` it is the tool's
error. A quota refusal is an error for every delivery. A job's fallback is
delivered: its result counts as fetched and no links are remembered.

**Short links** (#163, `src/short_link.rs`). A V4 URL signs every query
parameter and is about 700 characters, so a client appending `utm_source`
or a model re-typing the signature breaks it. Short links are
opt-in: only with a public origin (`PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN`;
unset or `direct`, nothing changes, and nothing else sets it), `put`
uploads as before but signs nothing: each link is
`https://<origin>/a/<render_id>/<n>.<ext>?e=<unix expiry>&s=<token>`, with
`e` the instant of `expires_at` and the token the first 16 bytes of
HMAC-SHA-256 (`ring::hmac`) over `"pscat-mcp artifact link v1"` and the
four fields, each after a NUL, base64url (22 characters). The key is read
once at startup from `PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE` (32 bytes at
least), with an optional previous key for rotation; `ArtifactConfig::check`
refuses a public origin without it, and `serve-stdio` refuses a public
origin (no HTTP route there). The `LinkScheme::Short` links get their own
sentence in the text block and the tool descriptions.

`http::router` mounts `/a/{*link}` (outside `require_auth`, like the health
and metadata paths) only when the store `serves_short_links()`. The handler
passes the raw path and query to `ArtifactStore::redeem`; nothing is
percent-decoded. `ShortLinks::verify` parses the path strictly (a UUID as
`Uuid::hyphenated` prints it, `n` from 1 to 9999 with no leading zero, an
extension the store uploads), takes the first `e` and the first `s` (its
leading base64url run) and ignores every other parameter, compares the
token in constant time (`subtle`) against the current key, then the
previous one, and only then checks `e` against the store's clock. The
object name is rebuilt from the three verified fields with the prefix, so
no client text reaches the bucket. `GcsArtifactStore::redeem` then mints a
V4 URL at the click time through the same `signBlob` path, for
`min(60 s, e - now)`, and the handler answers `302` with
`Referrer-Policy: no-referrer` and `Cache-Control: no-store`. Refusals are
`404`, `403`, `410` (and `405` for other methods) with fixed bodies and no
IAM call; a signing failure is `503` and one WARN `artifact link redirect
failed` with codes only. The request span records the path as
`/a/[redacted]` (`logged_path`), the handler logs one DEBUG line with the
outcome, and capture mode's `redact_url_queries` also redacts the render id
after `/a/` in a URL path. Without a public origin nothing changes: the
links are the V4 URLs and there is no `/a/` route.

**`none` is refused** with `gcs` by `http::check_none_with_artifact_links`:
every anonymous caller would share one quota, and anyone who can reach the
server could upload to the bucket and mint signed URLs, which are bearer
capabilities. `read` returns `None`: the signed URL is the only way to the
bytes, and `resources/read` of any `http(s)` URI is "resource not found"
with a message that does not echo it (the guide's own not-found error, which
names the URI, is reached only by other URIs).

### The ChatGPT widget

`src/widget.rs` and `src/widget.html` (issue #162), opt-in with
`PSCAT_MCP_CHATGPT_WIDGET` (`ArtifactConfig::chatgpt_widget`, default off).
ChatGPT shows a tool's pictures through an MCP App, a `ui://` HTML resource
named in the tool descriptor, rather than through `image` blocks (#138).

* **Descriptors.** `PscatMcp::with_transport` merges `widget::tool_meta()`
  (`ui.resourceUri` and the `openai/outputTemplate` alias, both
  `ui://pscat/render-result-v1`) into the `_meta` of the tools in
  `widget::TOOLS`: the four link tools and `render_result` when jobs are on.
  Nothing else in the listing changes; off, no tool has `_meta`.
* **Resources.** `list_resources` appends `widget::resource()` after the
  guide's documents, and `read_resource` answers the URI with
  `widget::read`: the page as text, `text/html;profile=mcp-app`, with
  `_meta.ui.csp` (`connectDomains` empty, `resourceDomains` from
  `widget::resource_domains`), `_meta.ui.prefersBorder` and the legacy
  `openai/widgetCSP` (snake_case) beside it. Off, the URI falls through to
  the guide's not-found error like any unknown one. The `resources`
  capability was already advertised for the guide.
* **The policy follows the links.** `gcs`: `https://storage.googleapis.com`,
  preceded by the short-link origin when `PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN`
  is set, because an `<img>` request is checked against the policy again
  after the `302`. `memory` (`pscat://` links) and `off`: none.
* **The page is a constant** (`include_str!`), so no request or result text
  reaches its HTML. Its script reads `window.openai.toolOutput` (and again
  on the `openai:set_globals` event), and also does the MCP Apps handshake:
  `ui/initialize` (protocol `2026-01-26`) to `window.parent`, then
  `ui/notifications/initialized` on a result, then renders the
  `structuredContent` of `ui/notifications/tool-result`; it answers any
  other host request with an empty result and ignores messages not from
  `window.parent`. It builds elements with `textContent` and sets
  `img.src` only for an `https://` link whose `expires_at` has not passed;
  `src/widget.rs` tests this at string level (one inline script, no
  `src=`, `eval`, `innerHTML` or `fetch`).
* **Results are not touched**, so the inline fixture stands; with the
  default `delivery` `inline` the page has no link and says so. Startup
  warns (`widget::startup_warning`, from `build_service_with`) when links are
  `off` or `memory`, and refuses nothing.

A breaking change to the page takes a new URI (`-v2`): hosts cache UI
resources by URI.

## Agent guidance (prompts and resources)

`src/guide.rs` serves the PSCat plugin's skills (issue #39). It runs no
PostScript. Everything it returns is text compiled into the binary, except
that the font-pack line (below) reads the installed packs from the art
catalog, which runs one worker, when a font-pack directory is configured.

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
  each key is a vendored file);
* for the files that name font-pack faces (`PACK_FACE_FILES`: `pscat`'s
  and `psart`'s `SKILL.md` and `psart`'s `styles-and-type.md`), which pack
  faces are reachable. Without `PSCAT_MCP_FONT_PACK_DIR` the line says none
  are. With it, the line lists the packs `font_packs.installed` names. The
  guide is built with the installed packs on the first guide request. A
  catalog that cannot be read gives a line saying so, which is not kept,
  so the next request asks again.

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
| File reads | PSCat read roots: only compiled-in `lib/` (also with a font-pack directory) | — |
| Open files | kernel `RLIMIT_NOFILE` | 64 |
| Sockets, io_uring, ptrace, `process_vm_*`, `pidfd_getfd`, `bpf` | kernel seccomp filter, installed by the worker before it reads its job (**Linux only**): `EPERM` | none allowed |
| Output bytes | worker (artifact cap: `output_too_large`, `error.input.code` `output_bytes`) + server (stdout cap, kill) | 8 MiB |
| SVG returned as text (all pages together; at most the output cap) | server, when it builds the result: up to the limit each page is an embedded text resource, above it an embedded blob resource (base64). Not a failure; the worker holds SVG to the output cap, from the SVG text's length before any page is encoded (`output_too_large`, `error.input.code` `output_bytes`) | 40 KiB |
| Pages returned | worker | 10 |
| Program stdout returned | worker (truncates) | 16 KiB |
| Concurrent renders | server semaphore, queue timeout → `server_busy` | min(CPUs, 4) |
| Renders per principal | one semaphore per principal holding or waiting for a slot, taken before the shared slot within the same queue timeout → `server_busy`, `error.input.code` `renders_per_principal`; async jobs wait for it without a timeout; stdio's local user is exempt. Unset, only OAuth users (`oauth:` ids) are capped, since bearer tokens are often shared; set explicitly, every principal is (all of `none`'s anonymous callers are one, `anonymous`); 0 turns it off | half the slots, rounded up (OAuth users only) |
| Client cancel / disconnect | dropping the future kills the worker (`kill_on_drop`) | — |
| Async job deadline | server: SIGKILL of the worker; PSCat's time limit and `RLIMIT_CPU` follow it | 10 min |
| Async job steps | PSCat `Limits::max_steps` | 25·10⁹ |
| Async jobs running | job semaphore, below the render slots; then the owner's share and a shared render slot, no queue timeout | half the slots |
| Async jobs held | per principal and in all (unfinished), `server_busy` | 4, 16 |
| Async job results | byte budget (largest result reserved while a job runs), TTL after it finishes | 128 MiB, 1 h |
| Stored artifacts (memory links) | per-principal quota; the async job results budget, shared; TTL after storing | 32 MiB, 128 MiB, 30 min |
| Uploaded artifacts (`gcs` links) | per-principal bytes within one TTL, per instance; signed URL lifetime (at most 7 days); each Google call's timeout, one deadline over a whole call's upload, and the 64 KiB response cap | 32 MiB, 30 min, 10 s, 20 s |

PSCat has **no operators for processes, network or environment access**
(see PSCat's `SECURITY.md`). The worker's environment is cleared anyway
(only `PSCAT_NO_CHECKOUT=1` is set), so nothing secret is inherited.

### The font-pack directory

`PSCAT_MCP_FONT_PACK_DIR` (`--font-pack-dir`) is the one opening in that
environment. When set, `SubprocessBackend` adds `PSCAT_HOME=<dir>` to the
worker's environment, and nothing else: PSCat documents `PSCAT_HOME` as the
data root it searches first, and finds packs in `<dir>/fonts/packs/<pack>/`
(`pscat::paths`, `pscat::fontpacks`; both public). It is not a new PSCat API
and `pscat_extras` does not change. Unset, the worker is started exactly as
before; a unit test pins its environment to `PSCAT_NO_CHECKOUT` alone.
The server canonicalises the path at startup (the worker runs in `/`) and
refuses to start if it is missing or not a directory.

What it does and does not loosen:

* **The programs' read roots stay empty.** PSCat's font lookup is not a
  program-chosen path (`src/read_roots.rs` in PSCat says so): it matches
  the requested name against file stems in the font directories, so it
  works without a read root. A program's `file`, `run` and `filenameforall`
  still see nothing on disk: every candidate under `PSCAT_HOME` lies outside
  every root and is skipped, an absolute name or one with `..` is
  `invalidfileaccess`, and a symlink out of the directory leads nowhere
  either. A `lib/artkit.ps` planted in the directory does not shadow the
  compiled-in one, since the disk candidate is skipped. The directory is
  never added to the read roots, so no program can read a pack file.
* **No write access.** No write roots, `RLIMIT_FSIZE=0`; the operator
  should also mount the directory read-only.
* **What `PSCAT_HOME` also means to PSCat.** A `fonts/catalog/` directory
  in it is a font override that wins over packs, and a font file in it is
  parsed by PSCat's font loader. The directory is trusted operator
  content, like the binary; nothing a client sends writes to it.
* **No other inheritance.** The server's `HOME`, `XDG_*`, `PSCAT_HOME`,
  `PSCAT_ROOT` and `PSCAT_LIB` never reach the worker, so PSCat's per-user
  data directory is not searched. Untrusted input is still processed only in
  the worker.
* **Other font roots, all operator-controlled.** Besides `PSCAT_HOME`, the
  worker loads font overrides from `<binary dir>/fonts/catalog/` and
  `/fonts/catalog/` (its working directory is `/`), and packs from
  `<binary dir>/fonts/packs/`. The per-user data directory is empty (the
  worker has no `HOME`, `XDG_*` or `APPDATA`), and `PSCAT_NO_CHECKOUT=1` turns
  off the checkout heuristics. These are trusted like `PSCAT_HOME`: the
  operator chooses them, a client cannot, and programs' read roots stay
  empty. The full order is in the README's "Where the worker looks for font
  files". `tests/font_pack.rs` pins the binary-directory case, and the
  `/fonts/catalog/` case is documented only.
* **Reporting.** The catalog the worker returns lists the packs it found;
  `pscat_extras` strips the paths. `get_renderer_info` reads the pack names
  from that cached catalog (a `font_packs` field, present only when the
  setting is on) and never looks at the directory itself.

Known gaps:

* `RLIMIT_AS` is applied on Linux only. On macOS the memory bound is PSCat's
  allocator limit plus the deadline; a single huge allocation inside one
  operator can overshoot before it is seen.
* If the server process itself is SIGKILLed, a running worker survives
  until its `RLIMIT_CPU` or its own time limit stops it (seconds). In a
  container the whole cgroup is torn down with the instance.
* The worker shares the container's network namespace, and on Cloud Run
  the metadata server (169.254.169.254, which hands out the service
  account's access token) answers every process in it. gVisor (Cloud Run's
  first generation) does not change that: it filters the container's
  system calls against the host kernel, not the container's own network.
  What keeps the worker off the network is described under
  [The worker's system-call filter](#the-workers-system-call-filter); it
  is Linux only, and the server process is not filtered.

### The worker's system-call filter

`src/sandbox.rs`. PostScript has no network operators, so a program cannot
open a socket; the filter is for the case where a bug in the interpreter
gives a program code execution in the worker. The first thing
`worker::run_stdio` does, before it reads a byte of the job and while the
worker has one thread, is:

1. `prctl(PR_SET_NO_NEW_PRIVS, 1)`, which lets an unprivileged process
   install a filter (the container runs with `--cap-drop ALL`);
2. `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER)` with a classic BPF program
   built in Rust from the `libc` crate's definitions (no libseccomp, no
   new dependency);
3. a check that the filter is in force: `socket(AF_INET)` must fail with
   `EPERM` and an ordinary call must still succeed.

If any step fails the worker writes one line to stderr and exits with
status 4 without reading the job; the server reports `worker_failed` with
that line (and logs it). There is no setting to run without the filter.

The program checks the architecture first (`AUDIT_ARCH_X86_64` or
`AUDIT_ARCH_AARCH64`; another value, such as a 32-bit call on a 64-bit
kernel, is denied), on x86_64 denies every x32 call (numbers with bit 30
set, which carry the x86_64 tag), then denies these numbers, taken from
`libc::SYS_*` for the target, with `EPERM`:

| Calls | Why |
|---|---|
| `socket`, `socketpair` | every address family: the worker talks to the server over inherited pipes and needs no socket, not even `AF_UNIX` |
| `connect`, `bind`, `listen`, `accept`, `accept4` | defence in depth; the worker inherits no socket (a test checks) |
| `io_uring_setup`, `io_uring_enter`, `io_uring_register` | io_uring can create and connect sockets itself, out of seccomp's sight |
| `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd` | the worker runs as the server's user: these could reach into the server's memory or copy its descriptors (its sockets) where the kernel's ptrace rules allow it |
| `bpf` | not needed, and a large kernel surface |

Everything else is allowed. A denied call returns `EPERM` rather than
killing the worker, because some library code tries a socket and copes with
failure (glibc's user lookup tries the nscd socket first); a denied call is
not logged. Threads started later (PSCat's band threads) inherit the
filter, and it survives `execve` and cannot be removed.

What the filter does not cover:

* **macOS** (development only): `install` does nothing there.
* **Paths.** Seccomp sees numbers, not paths, so the filter cannot stop
  the worker opening `/proc/<server pid>/mem`, which a process of the same
  user may do where Yama's `ptrace_scope` is 0. The server closes that
  itself: `serve-http` and `serve-stdio` call `sandbox::harden_server`
  (`prctl(PR_SET_DUMPABLE, 0)`) before the runtime starts and before any
  worker exists, and refuse to start if it fails. A non-dumpable process's
  `/proc/<pid>/mem`, `environ` and `fd` need `CAP_SYS_PTRACE`, which the
  container drops; the flag also turns off the server's core dumps. Nothing
  in the server reads its own `/proc` files beyond what a process may always
  read of itself. The worker is dumpable again after `execve`, which only
  exposes the worker to the server.
* **The server process.** It needs the network, so code execution in the
  server would reach the metadata server. Untrusted input is never
  processed there (see [Why a child process](#why-a-child-process)).
* **Other architectures.** The filter knows x86_64 and aarch64; a Linux
  build for anything else fails to compile rather than run unfiltered.
* **Emulation.** Under user-mode emulation of another architecture (qemu,
  Rosetta) the filter may fail to install, and the worker refuses its job,
  or it may install and deny every call, and the worker can neither report
  nor exit cleanly; either way the render fails (`worker_failed` or
  `timeout`). Native containers are what is supported.

The filter uses only `prctl(PR_SET_NO_NEW_PRIVS)`,
`prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER)`, the `arch` and `nr` fields
and the `ALLOW` and `ERRNO` actions, all of which gVisor implements, so it
is expected to work on Cloud Run's first generation as on a regular
kernel. `tests/worker_sandbox.rs` checks it on every Linux test run (CI's
Linux job is x86_64).

### Reporting the sandbox

The controls above fail closed, so a working one leaves no trace. Two
signals give positive evidence (#146):

* **The worker's filter.** `sandbox::install` returns the filter it put in
  force (`WorkerFilter::Seccomp` after the self-check on Linux,
  `WorkerFilter::None` elsewhere), and `worker::run_stdio` writes that fixed
  token (`seccomp` or `none`) in the `sandbox` field of every
  `WorkerResult`, failed jobs included. The backend (`SubprocessBackend`)
  keeps the first token it accepts in a shared `OnceLock`, and only the two
  tokens a worker can send count; anything else is ignored. The first time
  the `OnceLock` is set the backend logs one INFO line, `sandbox: the render
  worker's system-call filter ...`, with a `worker_filter` field, so it
  appears once per server process, never per request. The `RenderBackend`
  trait has a `worker_filter()` method (default `None`) that
  `RenderService::worker_filter` forwards.
* **The server's dumpable flag.** `main` logs one INFO line at startup,
  after `harden_server` succeeded (`sandbox: the server process is
  non-dumpable`; on a system without the flag, `... is dumpable`).
  `sandbox::server_dumpable` reads `PR_GET_DUMPABLE` from the kernel when
  asked, so what `get_renderer_info` reports is what is in force; elsewhere
  than Linux it is `true`.

`get_renderer_info` reports them as `sandbox: {worker_filter,
server_dumpable}`. The server starts no worker for `get_renderer_info`
(it runs no PostScript, and a probe job would count as a render in the
logs and counters), so before the first job has finished `worker_filter` is
`unknown`; after that it is the token the first finished job reported and
does not change. An operator or deploy check that wants positive
evidence renders once, then calls `get_renderer_info`. This is
read-only, small, and the same in `content` and `structuredContent`.

## Security boundaries

* **Authentication** (`auth.rs`): every `/mcp` request passes an
  `Authenticator` before rmcp sees it, which reads only the request head.
  The `bearer` method checks
  `Authorization: Bearer <token>` against SHA-256 digests of configured
  tokens in constant time. Several methods can run side by side (see
  [Auth methods](#auth-methods)). `--allow-unauthenticated` exists for local
  development and the server refuses to start with it on a non-loopback
  address. The explicit `none` method, for staging, is the only way to run
  without a token off loopback, behind guard rails (see
  [No-auth mode](#no-auth-mode-none)).
  `/healthz` and its alias `/health` are unauthenticated and do no work,
  as are, with `oauth` enabled, the Protected Resource Metadata paths
  (`/.well-known/oauth-protected-resource` and the same with the resource
  URL's path appended), which return a fixed document from configuration.
* **DNS rebinding / browsers**: rmcp's `Host` allow-list (loopback by
  default; set `PSCAT_MCP_ALLOWED_HOSTS` for a deployment) and `Origin`
  validation.
* **Logging**: PostScript source is never logged, only its byte length;
  tokens are never logged, nor are artifact links, render ids or artifact
  bytes (see [Artifact delivery](#artifact-delivery)). The one exception is the opt-in capture mode
  (`--capture-file`), which records request bodies and tokens never, and is
  refused unless the bind address is loopback.
  Every `rmcp` target is held to WARN and ERROR by a filter on the log layer
  that `RUST_LOG` cannot lift, so rmcp's dumps of whole messages (the
  `initialize` parameters, which over stdio are the client's own strings,
  requests, results) stay out of the logs; `RUST_LOG` replaces the default
  filter for our own targets (`tests/logging.rs`).
* **Statelessness**: Streamable HTTP runs without sessions
  (`legacy_session_mode = false`, JSON responses), so any instance can serve
  any request and nothing is stored between requests, except async jobs
  when `--async-jobs` turns them on (see [Async jobs](#async-jobs)) and
  stored artifacts when `PSCAT_MCP_ARTIFACT_LINKS=memory` turns them on (see
  [Artifact delivery](#artifact-delivery)). With `gcs` the objects live in
  the bucket, and only the token cache and the upload-quota ledger are per
  instance.

### Auth methods

`PSCAT_MCP_AUTH` (`--auth`) lists the enabled methods in order, for example
`bearer` or `bearer,oauth`. With it unset, the server behaves as before:
bearer tokens, or `--allow-unauthenticated` on loopback. Each method
implements `auth::AuthMethod` and answers one of three outcomes for a
request:

| Outcome | Meaning |
|---|---|
| `Accepted(Principal)` | a valid credential of this kind; the principal's id is the job owner |
| `Rejected(AuthError)` | a credential this method handles, not valid (or not allowed) |
| `NotApplicable` | no credential of this kind (for `bearer`: no `Authorization` header) |

`auth::AuthChain` tries the methods in order, and the first that accepts
admits the request. A *fallback* method (`AuthMethod::is_fallback`, meant
for the no-auth mode) runs only when every other method answered
`NotApplicable`. A token that was presented and rejected therefore stays
rejected, and a valid token is never downgraded to the anonymous owner,
whatever the order of the list. When nothing accepts, the reported error
is the most severe rejection (a `403` over a `401`, then the first in
order), or `missing_credentials`. The `WWW-Authenticate` header merges
every method's challenges by scheme (`auth::Challenges`), so `bearer` and
`oauth`, which both use the `Bearer` scheme, give one challenge:
`Bearer realm="pscat-mcp", resource_metadata="…"`.

The rules for a method:

* Principal ids are disjoint across methods. `bearer` keeps its bare
  `token#N`, other methods prefix their name (`oauth:<hash>`), and only
  the fallback uses `anonymous`. Job ownership and the per-principal
  budgets compare only the id.
* A method never logs or echoes a credential, and it reads only the
  request head, never the body.
* `AuthMethod::check` returns a boxed future, so a method may do bounded
  I/O, such as refreshing a cached JWKS.

Any single method is also an `Authenticator`, a chain of one. The request
span records `auth_method`, the name of the method that accepted the
request, and startup logs `auth_methods`. A method may also give
`Principal::subject`, the identity provider's own user id; only `oauth`
does (the token's `sub`), and the span records it as `sub` (issue #160).
It is for the operator to find a user, nothing else: ownership and budgets
still compare only the id.

An empty, unknown or repeated list is refused at startup.

#### No-auth mode (`none`)

`none` is deprecated (it goes away once OAuth is accepted in ChatGPT) and is
refused at startup together with `oauth`, because the fallback would answer a
request with no credential as anonymous, so a client would never get the `401`
that starts OAuth sign-in. `none` is `auth::NoAuth`, the fallback method named
`none` with the principal `anonymous`. Plain (`NoAuth::default()`), it admits everything:
the loopback `--allow-unauthenticated` mode. With `PSCAT_MCP_AUTH=…,none` it
is built as `NoAuth::guarded(NoneGuard)`; `auth_none` holds the guard rails
and their settings (`NoneAuthConfig`, flattened into `config::AuthConfig`),
and `http::authenticator_with` builds it. Nothing about it is on by default.

`NoneGuard::check` runs for each request that reaches the fallback, which is
one that no other method found a credential in, in this order:

1. **Expiry** (`PSCAT_MCP_AUTH_NONE_UNTIL`): `AuthError::AccessExpired`
   (`403`). The time is checked per request, because an instance can outlive
   it. The clock is injectable (`NoneGuard::with_clock`). A time that has
   already passed at startup does not refuse to start (a scale-to-zero cold
   start must not take the bearer clients down): `http::authenticator_with`
   logs a warning and puts `auth::ExpiredNoAuth` in the chain in place of
   `NoAuth`. It is a fallback too, never admits and reads no clock, and
   answers every request that reaches it `AccessExpired`, so a request with
   no credential gets the same `403` as on a running server whose window just
   closed. `AuthMethod::is_active` is false for it, so `auth_methods` lists
   only the real methods (empty when `none` was the only one: every request
   is then `403`). A malformed time, a bad CIDR, a zero limit and async jobs
   still refuse to start; they are configuration errors.
2. **Source allowlist** (`PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS`):
   `AuthError::SourceNotAllowed` (`403`), also for an address that cannot be
   determined (fail closed). A refused source creates no limiter state.
3. **Rate limit**: a fixed window per client address (IPv6 per `/64`, an
   IPv4-mapped address as IPv4), `AuthError::RateLimited` (`429`), whose
   `retry_after` the HTTP layer sends as `Retry-After`. The table holds at
   most 100,000 clients; when it is full of live windows a new client is
   limited until one expires. It lives in the process: with several
   instances the limit is per instance.

The statuses come from `AuthError::status()`, so a `none` that rejects while
alone in the list never sends a bare `401` without a challenge.

**Client address.** `http::serve_with_capture` serves with
`into_make_service_with_connect_info`, so the guard reads the TCP peer from
the request extensions. `X-Forwarded-For` is read only when
`PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS` is N > 0, and then only the N-th entry from
the right of all the header lines taken as one list: each trusted proxy
appends the address it saw, so entries to the left are the client's and are
never used. Fewer than N entries, or an entry that is not an address
(a port or brackets are tolerated), is an unknown source.

**Async jobs.** All anonymous callers are one principal, so `none` and async
jobs (which MCP Tasks need) are refused together at startup, and so are
`none` and artifact links: in memory (`PSCAT_MCP_ARTIFACT_LINKS=memory`),
which share the results budget with every client, and in Cloud Storage
(`gcs`), whose signed URLs anyone could then mint.

**Logs.** A request `none` admits carries `auth_method=none`. A rejection logs
its `reason` only. The client address is logged at DEBUG at most (so that an
operator can check the hop count on staging), never at INFO, and nothing
else is counted: there is no per-client label.

### OAuth resource server

`oauth` (`src/oauth.rs`, issue #119) makes the server an OAuth 2.1
*resource server* per the MCP authorization spec. It never sees a password
and runs no login page; an external authorization server signs users in
and issues JWT access tokens, and the method only validates them.

```
client ──401 + resource_metadata──► GET /.well-known/oauth-protected-resource/mcp
   │                                 (resource, authorization_servers, scopes)
   ├── discovery, registration, authorization code + PKCE ──► authorization server
   └── POST /mcp, Authorization: Bearer <JWT> ──► oauth: header and signature
                                                  (JWKS, cached), iss, aud,
                                                  exp/nbf, scopes, allowlist
```

* **What the server checks.** The token is at most 8 KiB, a JWS with an
  asymmetric `alg` (RS256/384/512, PS256/384/512, ES256, ES384, EdDSA; `none`
  and `HS*` are refused before a key is looked at, so a public key can never
  serve as an HMAC secret), a `kid` naming a key of the issuer's JWKS whose
  type, curve and own `alg` fit, and no `crit`. Header `jwk`/`jku`/`x5u`
  are ignored. Then `iss` equals the configured issuer byte for byte, `aud`
  contains the configured resource URL (RFC 8707), `exp` is present and
  `exp`/`nbf` hold with a leeway, `sub` is present, every configured scope is
  granted, and the user is on the allowlist (`sub:` entries, or `email:`
  entries when `email_verified` is `true`). Signatures are verified with
  `ring` over the received bytes; JOSE parsing is ours (about 150 lines,
  `serde_json` on the bounded token), which avoids a second crypto backend.
* **Answers.** Bad token: `401` `invalid_token`. Missing scope: `403`
  `insufficient_scope` with `scope`. Not allowlisted: `403` `not_allowed`,
  deliberately without an `error` parameter, since `insufficient_scope`
  would make a client retry consent. `AuthError::status` carries the `403`,
  which the chain ranks above any `401`.
* **JWKS.** Fetched lazily (never at startup, so an issuer outage cannot
  crash-loop the service) from `PSCAT_MCP_OAUTH_JWKS_URL` or the issuer's
  metadata (RFC 8414 with path insertion, then OpenID Connect, as the MCP spec
  orders them; the metadata's `issuer` must match). One fetch at a time,
  its attempt time stamped before it starts, at most one per refetch
  interval, so unknown `kid`s cannot make the server hammer the issuer. The
  whole key lookup, waiting for another request's fetch included, is bounded
  by the JWKS timeout, and a fetch is capped at 256 KiB, `https` only
  (`http` only to a loopback host, for tests), and follows no redirects.
  Keys stay in use when a refresh fails. A request without a JWT never waits.
  The HTTP client is `hyper-util` with `hyper-rustls` on `ring` and the
  compiled-in Mozilla roots (`webpki-roots`), so the image needs no CA bundle
  and no OpenSSL.
* **Metadata.** `AuthMethod::resource_metadata` gives the RFC 9728 document
  and its paths; the router serves them outside the authenticated `/mcp`
  like `/health`. The resource URL, and so the `resource_metadata` URL in
  the challenge, comes from `PSCAT_MCP_OAUTH_RESOURCE`, never from `Host`.
* **Allowlist.** `sub:<subject>`, `email:<address>` (with `email_verified`
  the JSON `true`) and `verified:*`, which admits any token whose
  `email_verified` is `true`, `email` claim or not (issue #160). The list is
  read once at startup; `verified:*` logs a warning and shows in the auth
  summary, and any other `verified:` value is refused without being echoed.
* **Privacy.** The principal (job owner) is `oauth:` plus a hash, and
  rejections log a fixed `reason`. An accepted token's `sub` is logged as
  the request span's `sub` field, so every line of the request, its render
  and an async job it started carries it. That is a deliberate reversal
  (issue #160) of the earlier rule that no per-user label reaches the logs:
  the owner opens the server to anyone with a verified email and needs to
  know who made a request, and the `sub` (an opaque issuer id, looked up in
  the issuer's dashboard) is the least that does it. No email, other claim
  or token is logged; no metric carries a per-user label; the capture file
  holds no principal.

**The identity provider.** The hosted deployment uses WorkOS AuthKit
(owner decision of 2026-11-17, [`AUTH_STRATEGY.md`](AUTH_STRATEGY.md), issue
#136; setup and failure modes in [`OAUTH_WORKOS.md`](OAUTH_WORKOS.md)), but
the method is provider-agnostic: any standards-based issuer works, given its issuer URL
(and OpenID Connect or RFC 8414 discovery, or an explicit JWKS URL). The
tests use a local mock authorization server. A provider for MCP clients
such as ChatGPT and claude.ai must support:

* **client registration** the connector can use: Dynamic Client
  Registration (RFC 7591, which ChatGPT uses), Client ID Metadata Documents,
  or a fixed client whose ID (and secret) is entered in the connector;
* **authorization code with PKCE** (`S256`), with
  `code_challenge_methods_supported` in its metadata;
* **JWT access tokens with a settable audience**: the `resource` parameter
  (RFC 8707) or an API/audience setting, so `aud` is this server's resource
  URL. Opaque access tokens (which need introspection) are not supported;
* optionally `email` and `email_verified` in the access token, for
  allowlisting by email.

Candidates named in #119 include hosted providers that document MCP (Auth0,
WorkOS, Stytch), Google Identity Platform (DCR support to be checked), and a
small self-hosted authorization server in front of GitHub or Google sign-in.
The choice stays with the owner; recording it here is part of that decision.

## Extension points

* **Another transport** (REST): call `RenderService::render`/`validate`
  (and `start_job`, `jobs()`, `cancel_job` for async jobs).
* **A tool whose input is not PostScript**: a `GenerateJob` kind (see
  [Generated programs](#generated-programs)); one whose output is an image
  made from several renders follows `review_sheet`.
* **Another backend** (a pool, a remote sandbox, a test double): implement
  `backend::RenderBackend`. Dropping the returned future must stop the job.
* **Artifact storage**: implement `artifacts::ArtifactStore` (see
  [Artifact delivery](#artifact-delivery)) and build it in
  `RenderService::with_artifacts` (or, if building it can fail, in
  `build_service_with`, as the Cloud Storage store is).
  Output larger than `PSCAT_MCP_MAX_OUTPUT_BYTES` still fails with
  `output_too_large`: links do not raise the output limit.
* **Authentication**: implement `auth::AuthMethod` and list it in `PSCAT_MCP_AUTH` (see [Auth methods](#auth-methods)).
