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
| Crate | `pscat` 0.7.0-rc.5 (`default-features = false`) |
| Source | `https://github.com/PSCatLabs/pscat`, tag `v0.7.0-rc.5` |
| Commit | `cbbf8fed3002c4f865fab0992559c1db38bb7c7c` (recorded in `Cargo.lock`) |

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
| `handwrite` | Writes text as a handwriting-style note (PSCat's `pscat handwrite`, the `/HandScript` face): hand-drawn letters with per-letter variation, word-wrapped, on a page whose height is fitted to the text. Args as in the PSCat plugin: `text` (required, at most `PSCAT_MCP_MAX_HANDWRITE_BYTES`), `size` points 0–2000 (default 36), `width` points 1–8000 (default 612), `paper` `plain`\|`ruled` (default plain), `ink` `"R,G,B"` each 0–1 (default `0.13,0.14,0.34`), `jitter` a whole number 0–10000 (default 16), `seed` (default 1985; the same arguments give the same bytes), `dpi` 9–1200 (default 72). PNG only. The face has `a`–`z` (capitals are written as lowercase), digits and `. , - ' ! ?`; any other character leaves a gap, and text with none of them is `invalid_request`. These are hand-drawn styles for notes, cards and art, not for imitating a specific person's handwriting. The note's height is known only after the text is laid out, in the worker: a note past `PSCAT_MCP_MAX_PIXELS` is `too_many_pixels`, past 8000 pixels (or 8000 points) a side `invalid_dimensions`, each before anything is drawn and with `error.input.code` `height`. Returns the note like `render_postscript` returns a page, with the same envelope. | same |
| `review_sheet` | Composes one labelled PNG from several programs, each rendered at several resolutions, to compare candidates side by side (PSCat's `pscat sheet`; the layout, label and caption strips, zoom and frames are PSCat's own). Args as in the PSCat plugin, without `out_path`: `entries` (required, 1 to `PSCAT_MCP_MAX_SHEET_CELLS` rows), each with `source` (required), `label` (at most 200 characters; default `program N`), `width`/`height` in points 1–8000 and a `dpi` list; top-level `width`/`height` (default 612×792), `dpi` (default `[72]`), `labels` and `sizes` (default true). A `dpi` item is a number 9–1200, or `{"dpi": 9, "zoom": 4}` to draw that render enlarged with nearest-neighbour sampling (2–16). Each cell is drawn at its true pixel size with a hairline, the label above the row and `WxH px` under each cell; its text line gives the cell's size in points and its dpi, as `64x64 pt at 36 dpi = 32x32 px`; a program that shows several pages gives its last one. A cell whose program fails (a PostScript error, or its own step, time or memory limit) is drawn as far as it got and the sheet still succeeds. Every budget (cells, source bytes in all and per entry, each cell's sides, the cells' pixels, the sheet's pixels and sides, time enough for every cell) is checked before a worker starts, with `error.input.code` naming it (`cells`, `sources`, `source`, `deadline`, `cell_side`, `cell_pixels`, `sheet_side`, `sheet_pixels`). The whole sheet is one worker job in one render slot, with its own deadline (`PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`); each cell runs on a fresh interpreter with the render's step and time limits and at most an equal share of the time left. Returns one PNG image block and a text line per cell; `structuredContent` has the sheet's size and, for every cell, its row and column, dpi, zoom, render size in pixels and points (`width_pt`, `height_pt`), place on the sheet and, if it failed, its error code and a bounded PSCat report (under 24 KB with 32 failed cells). | same |
| `validate_postscript` | Tokenizer pass, then executes the program in the same sandbox and discards the output. Returns the PostScript error (name, offending command, line), lint findings (blank page, stack leak, unbalanced `gsave`), page count and stdout. | same |
| `describe_art_capabilities` | Lists the creative capabilities of the linked PSCat build: fonts, Type 3 faces, mood palettes, page templates, artkit and style-pack procedures, native operators, config dials. The whole catalog is about 280 KB, so a reply is one bounded view (under 24 KB, in both `content` and `structuredContent`, which carry the same information; some clients, Claude Code among them, give the model only the `structuredContent` JSON). No arguments: an index of every kind with its names, and installed font packs. `kind` (`font`, `type3_face`, `palette`, `template`, `procedure`, `operator`, `dial`): that kind's names with a one-line description each, 100 at a time (`offset`, `limit` 1–100). `names` (up to 20, optionally narrowed by `kind`): full entries with parameters, defining library file, `run` lines that load it, example and availability; unknown names are listed under `not_found`, and names that do not fit the reply under `omitted`. Runs no PostScript. The catalog is read once in a worker process, so it matches what renders can reach, and kept for the life of the server; later calls take no render slot. Server file paths are not reported. | same |
| `get_renderer_info` | PSCat version, formats, limits in force, capability notes. Runs nothing. | same |
| `start_render` | Starts `render_postscript` as a background job, for a render past the sync deadline: the same arguments (checked here, before a job is accepted), and it returns a `job_id` (a random UUID) at once with state `queued`. The job runs under the job deadline (`PSCAT_MCP_JOB_TIMEOUT_MS`, 10 minutes) and every other render limit. Only with async jobs on (always over stdio; `PSCAT_MCP_ASYNC_JOBS` over HTTP). A cap or the results budget refuses it with `server_busy` or `results_budget_exceeded`. | not read-only, not idempotent, non-destructive, closed-world |
| `render_status` | A job's `state` (`queued`, `running`, `done`, `failed`, `cancelled`), `elapsed_seconds`, `timeout_seconds`, and `job_error` (code and message) for a failed job or `expires_in_seconds` for a finished one. Under 2 KB. An unknown, expired or other client's `job_id` is `unknown_job`. | read-only, idempotent |
| `render_result` | A finished job's result: what `render_postscript` returns (the same content blocks and envelope, within the same output caps), or the failed render's error with `isError`. A queued or running job is `job_not_finished`, a cancelled one `cancelled`. It can be fetched again until the job expires. | read-only, idempotent |
| `cancel_render` | Cancels a job: a queued one never starts, a running one's worker is killed. Returns the job's status (`cancelled` once it has stopped); a finished job is left as it is. | not read-only, idempotent, non-destructive |

`render_postscript`, `chalkboard_math`, `handwrite`, `review_sheet` and
`start_render` also take `delivery` (see [Artifact links](#artifact-links)).

### Output formats

| Format | Returned as | MIME type |
|---|---|---|
| `png` | one MCP `image` content block per page | `image/png` |
| `svg` | one embedded text resource per page while all pages together are at most `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES` (40 KiB); above that, one embedded blob resource (base64) per page, up to `PSCAT_MCP_MAX_OUTPUT_BYTES` | `image/svg+xml` |
| `pdf` | one embedded blob resource holding every page | `application/pdf` |

Every result also has a short text summary and `structuredContent`
(declared with an `outputSchema`) with `ok`, sizes, page count, program
stdout and the renderer version, and for each artifact its MIME type, size
and `returned_as` (`image`, `text` or `blob`). Each artifact's size is given
in device pixels (`width_px`, `height_px`) and in points (`width_pt`,
`height_pt`, 1/72 inch): the page as drawn, after the default size and any
`setpagedevice`, so a 400×200 pt board at 144 dpi is 800×400 px. Each
artifact also gives the `dpi` it was rendered at, the scale between the two
(`px = pt × dpi / 72`). The text summary states all three, as
`400x200 pt at 144 dpi = 800x400 px`. Artifact bytes are not duplicated
there.

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
`worker_failed`, for async jobs `unknown_job`, `job_not_finished` and
`results_budget_exceeded`, and for Cloud Storage links
`storage_unavailable` (see [Artifact links](#artifact-links)). A failed
render never returns an image.

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
   with `PSCAT_MCP_TASKS=true`. The server then advertises the extension.
   A `tools/call` of `render_postscript`, `chalkboard_math` or `handwrite`
   that declares it in its client capabilities runs synchronously at
   first; only a call still running after `PSCAT_MCP_TASK_AFTER_MS` (2 s by
   default) becomes a task, and the client gets a task handle. `tasks/get`
   returns the tool's result once it is done, and `tasks/cancel` kills the
   worker. A call that does not declare the extension gets today's
   synchronous result. A task is a job: the explicit tools see it too.
   `review_sheet` always answers synchronously: its plan shares out its own
   deadline (`PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`) between its cells.

   **Only long calls become tasks.** The extension leaves the choice to the
   server, call by call: it may answer a Tasks client with the plain result,
   and the client must accept either. So a call that finishes within the
   threshold returns its result directly, as for any other client: it never
   counts against the job caps, reserves nothing from the results budget and
   needs no running-job permit, and it is held to the sync deadline. A call
   still running at the threshold becomes a task with the same worker and
   render slot (no second render), and from then on it is a job: it takes a
   running-job permit, counts against the caps, reserves its largest result
   and runs to the job deadline. Its worker starts under the job limits (a
   worker's limits are fixed when it starts), and until it becomes a task
   the server holds it to the sync deadline itself. If it cannot become a
   task at the threshold (no running-job permit free, the client's or the
   server's job cap, the budget), it carries on as a synchronous call and is
   stopped at the sync deadline with `timeout`. A `notifications/cancelled`
   for the call kills its worker until it has become a task; after that
   `tasks/cancel` does. `PSCAT_MCP_TASK_AFTER_MS=0` makes every such call a
   task at once.

How it behaves:

* **Ownership.** A job belongs to the client that started it: the bearer
  token over HTTP (its position in the configured list, never the token),
  the local user over stdio. Any other client gets the same `unknown_job`
  (or "unknown task") as for an id that never existed. Tokens are read once
  at startup, so a position names the same token for the life of the
  process (and its jobs). With `PSCAT_MCP_ALLOW_UNAUTHENTICATED` (loopback
  only) and async jobs, every caller is `anonymous` and they all share one
  owner: any of them can see, fetch and cancel the others' jobs, and the
  per-client cap is shared. (The explicit `none` auth mode, which can run off
  loopback, refuses to start with async jobs for this reason.)
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
  `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES` (`results_budget_exceeded`). One
  client may reserve and hold at most `PSCAT_MCP_JOB_RESULTS_PER_PRINCIPAL_BYTES`
  of it, by default the budget less one largest result (about 117 of the
  128 MiB, so another client can always start a job, and a single client
  or the stdio user has nearly all of it; with a budget under two largest
  results, all of it). A job over that share is
  `results_budget_exceeded` with `error.input.code`
  `results_per_principal_bytes` (and `limit_bytes`, `reached_bytes`); the
  server's own budget being full has `error.input.code`
  `results_budget_bytes`. At the defaults a reservation is about 11 MiB,
  so the 128 MiB budget holds about 11 unfinished jobs: past that, `results_budget_exceeded` comes before the
  total cap of 16 (`server_busy`), and the budget, not the cap, is what
  bounds memory. Raise the budget to about 180 MiB for the cap to bind
  first, or lower `PSCAT_MCP_MAX_OUTPUT_BYTES` to shrink each reservation. A
  finished job is kept for `PSCAT_MCP_JOB_RESULT_TTL_MS` (an hour, as in
  the plugin) and can be fetched again until then; when the budget or the
  client's share is short, results already fetched are dropped first, oldest
  first, the requesting client's own before anyone else's (a client over its
  share gives up only its own); a cancelled job, which has no result, counts
  as fetched once it has stopped, so cancelling frees a client's share. A job
  that would be refused anyway drops nothing.
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
| rmcp 3.9 client | yes over stdio, when built with `ClientCapabilities::builder().enable_tasks()`; over Streamable HTTP only when it also connects with the 2026-07-28 discover lifecycle (see the paragraph below the table) | verified: `tests/jobs.rs` |

So the Tasks path is off by default and the explicit tools are the way in
for the clients in use today.

Over Streamable HTTP, a client gets MCP Tasks only if it connects with the
2026-07-28 discover lifecycle. A client that starts with the legacy
`initialize` handshake, such as rmcp's default `serve`, gets plain
synchronous results even with `--tasks` on, because its Tasks capability
never reaches this stateless server. For rmcp, connect with
`serve_with_lifecycle(.., ClientLifecycleMode::Discover { preferred_versions: [V_2026_07_28] })`.
`tests/jobs.rs::over_http_an_rmcp_tasks_client_gets_a_task` pins this.

## Artifact links

Some clients do not show the model an inline image (ChatGPT, issue #138),
so the four render tools can return their output as links instead of, or as
well as, inline content blocks. The `delivery` argument chooses:

| `delivery` | Result |
|---|---|
| `inline` | today's result, byte for byte: images, text or blob resources. The default, unless the operator set `PSCAT_MCP_ARTIFACT_DELIVERY`. |
| `url` | no bytes: one `resource_link` block per artifact, and a short text block naming every link. |
| `both` | the inline blocks and the links. |
| `auto` | links when the output (all artifacts, before base64) is over `PSCAT_MCP_ARTIFACT_THRESHOLD_BYTES`, otherwise inline. With no link store it is inline. |

With links, `structuredContent` has the call's `render_id` (random, a UUID
v4) and, for each entry of `artifacts` (the manifest: one per page, or one
PDF document), `download_url` and `expires_at` (RFC 3339, UTC), with
`returned_as` `link` when the bytes are not inline. `review_sheet` has
`render_id`, `download_url`, `expires_at` and `returned_as` at the top level.
The link is in all three places (the `resource_link`, `structuredContent` and
the text) because clients differ in which they show the model. Diagnostics
stay inline whatever the delivery: errors, page counts, sizes, the cells of a
sheet and the program's stdout. A failed render stores nothing.

Links need a store, `PSCAT_MCP_ARTIFACT_LINKS`:

* `off` (the default): `url` and `both` fail with `invalid_request` and
  `error.input.code` `delivery`; nothing is ever silently inline instead.
* `memory`: the bytes are kept in the server process for
  `PSCAT_MCP_ARTIFACT_URL_TTL_SECS` (30 minutes) and served by
  `resources/read` of `pscat://render/<render_id>/<n>` (`n` from 1, in the
  manifest's order), as a blob resource, to the client that made them only.
  Another client's link, an expired one and one that never existed get the
  same JSON-RPC "resource not found" error, which does not repeat the URI.
  They are not listed by `resources/list`. A `pscat://` link is an MCP
  resource, not something a browser can fetch.
* `gcs`: the server uploads the output to a Cloud Storage bucket and links
  each object by an `https:` V4 signed URL, which a browser, ChatGPT or any
  HTTP client can open until it expires. See [Cloud Storage links](#cloud-storage-links).

**Limits of the memory store.** Like async jobs it is in one process: lost
on a restart, and invisible to any other instance, so over HTTP it works only
on a single instance (see [Long renders](#long-renders-async-jobs)). Each
client may hold at most `PSCAT_MCP_ARTIFACT_QUOTA_BYTES` (32 MiB) of stored
output at once; over it a call is refused with `results_budget_exceeded` and
`error.input.code` `artifact_quota_bytes` (with `limit_bytes` and
`reached_bytes`) until earlier links expire. All clients together share the
async-job results budget (`PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES`, also when
async jobs are off): stored output counts against it like an unfetched job
result and is never dropped before its TTL, and a call that would pass it is
refused with `error.input.code` `results_budget_bytes`. **The server refuses
to start with the `none` auth method and memory links**, as it does with
`none` and async jobs: every anonymous caller is the one principal
`anonymous`, so one could fill the budget the links share with authenticated
clients, and read the others' links. With `PSCAT_MCP_ALLOW_UNAUTHENTICATED` (loopback only) memory
links still start, and every caller shares the one `anonymous` quota; over
stdio there is one principal, the local user.

### Cloud Storage links

With `PSCAT_MCP_ARTIFACT_LINKS=gcs` and `PSCAT_MCP_ARTIFACT_BUCKET`, a call
whose delivery comes to links uploads each artifact, from the server process
only, to `<PSCAT_MCP_ARTIFACT_PREFIX><render_id>/<n>.<ext>` (`renders/` by
default; `n` from 1 in the manifest's order; `png`, `svg` or `pdf`). The
result has the same shape as with memory links: `render_id`, a
`download_url` and `expires_at` on each `artifacts` entry, a
`resource_link` block per artifact, and a short text block, except that every
link is the `https://storage.googleapis.com/<bucket>/<object>?X-Goog-...`
signed URL itself. `resources/read` serves nothing for them.

* **Uploads** are one `multipart` JSON-API upload per object with
  `ifGenerationMatch=0`, so an existing object is never overwritten, and
  metadata `contentType` (the artifact's MIME type) and
  `contentDisposition` `inline; filename="pscat-<n>.<ext>"`. The server never
  lists, reads or deletes objects; the bucket's lifecycle rule removes them
  (see [`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md#cloud-storage-artifact-links)).
* **Signed URLs** are V4 (`GOOG4-RSA-SHA256`), valid for
  `PSCAT_MCP_ARTIFACT_URL_TTL_SECS` (30 minutes by default, at most 7 days),
  and signed without a key: the server asks the IAM Credentials API's
  `signBlob` to sign with the account's Google-managed key. Its access token
  comes from the metadata server only (cached until shortly before it
  expires); no key, key file or credential variable is read.
  `PSCAT_MCP_ARTIFACT_SIGNER` names the signing account; unset, it is the
  account the metadata server reports (the Cloud Run runtime account).
* **Failures.** If the credentials, an upload or a signature fails (each
  call has its own timeout, `PSCAT_MCP_ARTIFACT_GCS_TIMEOUT_MS`), `delivery`
  `url` returns `storage_unavailable`, a retryable error whose message says
  to retry or use `delivery: "inline"`. `both` and `auto` return the output
  inline instead, with a note in the text and in
  `structuredContent.delivery_note`. The server logs one WARN line with the
  code, the stage (`credentials`, `upload`, `signing`) and at most an HTTP
  status: never a URL, a token or a response body. An upload that fails
  part-way may leave earlier objects of that call in the bucket until the
  lifecycle rule removes them.
* **Quota.** Each client may upload `PSCAT_MCP_ARTIFACT_QUOTA_BYTES`
  (the bytes plus 1 KiB a call) within one TTL, refused with
  `results_budget_exceeded` and `error.input.code` `artifact_quota_bytes`,
  as with memory links. The count is in memory and **per server instance**,
  so with several instances a client may upload that much on each. It is
  not bounded by the results budget: nothing is kept in memory.
* **The `none` auth method is refused at startup with `gcs`**: every
  anonymous caller is one principal, sharing one quota, and anyone who can
  reach the server could upload to the bucket and mint signed URLs. With
  `PSCAT_MCP_ALLOW_UNAUTHENTICATED` (loopback only) `gcs` still starts, and
  every local caller shares the one `anonymous` quota and can mint signed
  URLs.

**Async jobs and Tasks.** `start_render` takes `delivery` too (an unusable
one is refused before the job is accepted), and it is applied when the result
is fetched: the first `render_result` (or `tasks/get`) stores the output and
later fetches return the same links until they expire, then store it again.
A Tasks call that finishes before `PSCAT_MCP_TASK_AFTER_MS` returns its
result with the delivery applied at once.

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
  are added or turned off. The `pscat` and `psart` documents also say which
  font-pack faces this server has: none when `PSCAT_MCP_FONT_PACK_DIR` is
  unset, otherwise the packs the art catalog lists as installed (the same
  list as `get_renderer_info`'s `font_packs.installed`).
* An unknown URI is JSON-RPC error `-32002` (resource not found; `-32602`
  for a 2026-07-28 client, as that version specifies); an unknown
  prompt, an unknown argument or a bad argument value is `-32602` (invalid
  params). Nothing here starts a worker, except the font-pack line when
  `PSCAT_MCP_FONT_PACK_DIR` is set: it reads the installed packs from the
  art catalog, a worker run once and kept, on the first guide request.
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

## Font packs

PSCat ships a few dozen faces inside its binary. The rest are downloadable
packs: `extras` (display and monospaced faces) and `cjk-thai` (Korean,
Japanese and Thai, about 70 MB). The worker runs with an empty environment,
so it does not see packs installed for the user. To give it some, install
them into a directory of your own and point the server at it:

```bash
PSCAT_HOME=/srv/pscat-data pscat fonts install --pack cjk-thai   # the PSCat CLI
PSCAT_MCP_FONT_PACK_DIR=/srv/pscat-data pscat-mcp-server serve-http ...
```

The directory is a PSCat data root: packs are in `DIR/fonts/packs/<pack>/`
(not `DIR` itself, and not `.../fonts/packs`). The server passes it to each
worker as `PSCAT_HOME`, which PSCat documents as the first place it looks,
and sets nothing else: the server's own `HOME`, `XDG_*`, `PSCAT_HOME` and
`PSCAT_ROOT` are never passed on.

* **Opt-in.** Unset, the worker is started exactly as before.
* **Startup check.** The server refuses to start if the path is missing or
  is not a directory, and warns if it has no `fonts/` directory.
* **Read-only, and not readable by programs.** The directory is not added to
  the programs' read roots, which stay empty: `(path) (r) file`, `run` and
  `filenameforall` still cannot reach it, with `..` or a symlink or without.
  PSCat's font lookup reads the pack itself, as it does for the compiled-in
  fonts. Nothing in the worker writes to it (no write roots, `RLIMIT_FSIZE=0`);
  mount it read-only as well. Its contents are trusted operator content, as
  the binary is: a `fonts/catalog/` directory in it is an override that wins
  over packs, and a font file in it (or a symlink to one) is parsed by PSCat.
* **Reporting.** `describe_art_capabilities` lists the installed packs and
  their faces, and `get_renderer_info` has a `font_packs.installed` list of
  pack names when the setting is on. The directory's path is never reported.
* **Memory.** A face is read whole into the worker's heap when a program
  first selects it (Noto Sans JP is 9.5 MB), which counts toward
  `PSCAT_MCP_MAX_MEMORY_BYTES`.

In the container, mount the directory read-only and set the variable; see
[docs/CLOUD_RUN.md](docs/CLOUD_RUN.md#font-packs).

### Where the worker looks for font files

Font files come from these places and no others. The worker's environment is
empty, its working directory is `/`, and it finds nothing through the
server's own `HOME`, `XDG_*`, `APPDATA`, `PSCAT_HOME` or `PSCAT_ROOT`.

Font overrides (`fonts/catalog/<family>/<stem>.ttf`), searched first, in
this order:

1. `$PSCAT_HOME/fonts/catalog/`, only when `PSCAT_MCP_FONT_PACK_DIR` is set;
2. `<directory of the server binary>/fonts/catalog/`. In the container that
   is `/usr/local/bin/fonts/catalog`; in a development build,
   `target/release/fonts/catalog` or `target/debug/fonts/catalog`;
3. `/fonts/catalog/`, because the worker's working directory is `/`.

Installed packs (`<root>/fonts/packs/<pack>/`), searched after the overrides:

4. `$PSCAT_HOME/fonts/packs/`, only when the setting is on;
5. `<directory of the server binary>/fonts/packs/`.

Nothing else is searched:

* **No per-user data directory.** PSCat's per-user data directory comes from
  `HOME`, `XDG_DATA_HOME` or `APPDATA`, which are all unset in the worker, so
  that step finds nothing.
* **No checkout.** The server sets `PSCAT_NO_CHECKOUT=1`, which turns off
  PSCat's debug-build checkout heuristics.
* **Operator-controlled.** Every location above is set by the operator: the
  install path of the binary, the fixed working directory, and the font-pack
  setting. A client cannot choose one, and a program cannot read the
  directories: programs' read roots stay empty, as in
  [The font-pack directory](docs/ARCHITECTURE.md#the-font-pack-directory).

To add a face beside the binary, copy the font into a `fonts/catalog/<family>/`
directory next to it. Each render starts a new worker, so the next render
finds it. `describe_art_capabilities` keeps its catalog for the life of the
server, so restart the server to see the face listed there; the override is
reported under `font_packs.overrides`, without its path.

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
* `GET /healthz` and `GET /health`: unauthenticated liveness check,
  `{"status":"ok"}`. Both paths serve the same handler; `/health` is the one
  to use from outside Cloud Run (see `docs/CLOUD_RUN.md`).

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
| `PSCAT_MCP_AUTH` | unset | `--auth`: the auth methods accepted on `/mcp`, a comma list tried in turn (a request passes if any accepts it): `bearer` (the tokens above), `oauth` (the `PSCAT_MCP_OAUTH_*` settings below) and `none` (no authentication, **staging only and deprecated**, with the guard rails below; refused together with `oauth`). An empty, unknown or repeated list is refused. Unset: bearer tokens, or `PSCAT_MCP_ALLOW_UNAUTHENTICATED` on loopback, as before; `none` is never on by default. When set, `PSCAT_MCP_ALLOW_UNAUTHENTICATED` is ignored (and still refused off loopback). See [Authentication](#authentication) |
| `PSCAT_MCP_OAUTH_ISSUER` | — | `oauth`: the authorization server's issuer URL (`https`); tokens' `iss` must equal it exactly, trailing `/` included. Required for `oauth` |
| `PSCAT_MCP_OAUTH_RESOURCE` | — | `oauth`: this server's public URL as clients connect to it, e.g. `https://mcp.example.com/mcp` (RFC 8707 resource); tokens' `aud` must contain it. Required for `oauth` |
| `PSCAT_MCP_OAUTH_ALLOWED_USERS` / `PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE` | — | `oauth`: who may use the server, `sub:<subject>` or `email:<address>` entries (comma list / one per line, `#` comments). Email entries match only when the token says `email_verified: true`. At least one entry is required for `oauth` |
| `PSCAT_MCP_OAUTH_SCOPES` | empty | `oauth`: scopes every token must carry (comma list), also published in the metadata and the challenge. Empty: no scope check |
| `PSCAT_MCP_OAUTH_JWKS_URL` | discovered | `oauth`: the issuer's JWKS URL. Unset: read from the issuer's metadata (RFC 8414, then OpenID Connect discovery) |
| `PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS` | 5000 | `oauth`: timeout of one JWKS or discovery fetch, and of a whole key lookup |
| `PSCAT_MCP_OAUTH_JWKS_TTL_SECS` | 600 | `oauth`: how long fetched keys are used before they are fetched again |
| `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS` | 30 | `oauth`: least time between two JWKS fetch attempts (an unknown `kid`, an expired cache or a failing issuer) |
| `PSCAT_MCP_OAUTH_LEEWAY_SECS` | 60 | `oauth`: clock-skew leeway for `exp` and `nbf` |
| `PSCAT_MCP_AUTH_NONE_UNTIL` | unset | with `none`: an RFC 3339 time (`2026-12-01T00:00:00Z`) after which anonymous requests get `403 unauthenticated_access_expired`. A time that has passed at startup does not stop the server: `none` is disabled (a warning is logged) and the other methods keep working; a malformed time still refuses to start |
| `PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS` | unset (any source) | with `none`: comma list of source CIDRs allowed to make anonymous requests (`203.0.113.0/24,2001:db8::/32`; a bare address is one host); others get `403 source_not_allowed` |
| `PSCAT_MCP_AUTH_NONE_RATE_LIMIT` / `PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS` | 60 / 60 | with `none`: anonymous requests one client address may make per window (a fixed window; IPv6 clients per `/64`), before `429 rate_limited` with `Retry-After`. This is on top of the render concurrency cap, per server instance |
| `PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS` | 0 | with `none`: trusted proxies in front of the server. 0: the client is the TCP peer, and `X-Forwarded-For` is ignored. N: the client is the N-th entry from the right of `X-Forwarded-For`; fewer entries, or one that is not an address, is an unknown source and refused. Cloud Run: see [`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md) |
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
| `PSCAT_MCP_FONT_PACK_DIR` | unset | `--font-pack-dir`: a PSCat data root holding font packs (`DIR/fonts/packs/<pack>/`, what `PSCAT_HOME=DIR pscat fonts install` writes). The workers get it as `PSCAT_HOME` and nothing else of the environment; unset, only the compiled-in fonts exist. The server refuses to start if the path is missing or not a directory. Read-only: programs still cannot read or write there. See [Font packs](#font-packs) |
| `PSCAT_MCP_RENDER_TIMEOUT_MS` | 20000 | wall-clock deadline; worker is SIGKILLed after it |
| `PSCAT_MCP_MAX_STEPS` | 5000000000 | PSCat step limit |
| `PSCAT_MCP_MAX_MEMORY_BYTES` | 536870912 | PSCat heap-growth limit per render |
| `PSCAT_MCP_MAX_PIXELS` | 16000000 | device pixels per page |
| `PSCAT_MCP_MAX_PAGES` | 10 | pages per render |
| `PSCAT_MCP_MAX_OUTPUT_BYTES` | 8388608 | inline output per render (PNG, PDF, and SVG as well) |
| `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES` | 40960 (40 KiB, or the output limit if that is smaller) | SVG per render returned as text resources, all pages together; a larger SVG is returned as blob resources (base64), held to `PSCAT_MCP_MAX_OUTPUT_BYTES`. 0 returns every SVG as a blob. May not exceed `PSCAT_MCP_MAX_OUTPUT_BYTES` (the server refuses to start). The old name `PSCAT_MCP_MAX_SVG_BYTES` (`--max-svg-bytes`) is a deprecated alias: it works and logs a warning at startup, and setting both names refuses to start |
| `PSCAT_MCP_MAX_CONCURRENT_RENDERS` | min(CPUs, 4) | renders at once. On Cloud Run's first-generation environment (gVisor) the CPU count can exceed the service's `--cpu`, so set it explicitly, equal to `--cpu` (see [`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md)) |
| `PSCAT_MCP_QUEUE_TIMEOUT_MS` | 10000 | wait for a slot before `server_busy` |
| `PSCAT_MCP_ASYNC_JOBS` | `false` | `serve-http` only: serve async jobs (`start_render` and companions). In memory: one always-on instance only (see [Long renders](#long-renders-async-jobs)); always on over stdio |
| `PSCAT_MCP_TASKS` | `false` | also serve the MCP Tasks extension on top of the jobs (needs async jobs) |
| `PSCAT_MCP_TASK_AFTER_MS` | 2000 | with `PSCAT_MCP_TASKS`: how long a Tasks client's call runs (from when its worker starts) before it becomes a task; a quicker call returns its result directly and never touches the job caps or budget. Must be below `PSCAT_MCP_RENDER_TIMEOUT_MS`; 0 makes every eligible call a task at once. Above 0, the job limits must be no tighter than the sync ones (`PSCAT_MCP_JOB_TIMEOUT_MS` ≥ `PSCAT_MCP_RENDER_TIMEOUT_MS`, the job step limit ≥ `PSCAT_MCP_MAX_STEPS`), since such a call's worker starts under them |
| `PSCAT_MCP_JOB_TIMEOUT_MS` | 600000 | wall-clock deadline of one job's worker (SIGKILL after it); PSCat's time limit and the CPU rlimit follow it |
| `PSCAT_MCP_JOB_MAX_STEPS` | 25000000000 (0 if `PSCAT_MCP_MAX_STEPS` is 0) | PSCat step limit of a job |
| `PSCAT_MCP_MAX_RUNNING_JOBS` | half the render slots, at least 1 | jobs running at once (more wait queued); must stay below `PSCAT_MCP_MAX_CONCURRENT_RENDERS` (1 with one slot) |
| `PSCAT_MCP_MAX_JOBS_PER_PRINCIPAL` | 4 | queued or running jobs one client (token, or the stdio user) may have |
| `PSCAT_MCP_MAX_JOBS` | 16 | queued or running jobs in all |
| `PSCAT_MCP_JOB_RESULT_TTL_MS` | 3600000 | how long a finished job and its result are kept |
| `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES` | 134217728 | memory for job results, all jobs together; at least one largest result (`PSCAT_MCP_MAX_OUTPUT_BYTES` × 4/3 plus stdout and 4 KiB); job settings are checked only when jobs are on |
| `PSCAT_MCP_JOB_RESULTS_PER_PRINCIPAL_BYTES` | the budget less one largest result (the whole budget when that holds fewer than two) | job-result bytes one client (token, or the stdio user) may have reserved and held, so one client cannot fill the budget for the others (`results_budget_exceeded`, `error.input.code` `results_per_principal_bytes`); at least one largest result and at most the budget (equal to the budget turns the share off); checked only when jobs are on |
| `PSCAT_MCP_ARTIFACT_LINKS` | `off` | where `delivery` `url`/`both` links point (see [Artifact links](#artifact-links)): `off`, `memory` (this process, `resources/read` of `pscat://render/<render_id>/<n>`; one instance only, lost on restart) or `gcs` (Cloud Storage uploads and `https` V4 signed URLs, see [Cloud Storage links](#cloud-storage-links)); both are refused with the `none` auth method |
| `PSCAT_MCP_ARTIFACT_DELIVERY` | `inline` | the `delivery` of a call that gives none: `inline`, `url`, `both` or `auto`; anything but `inline` needs links on (else the server refuses to start). An argument overrides it |
| `PSCAT_MCP_ARTIFACT_THRESHOLD_BYTES` | 1048576 | with `delivery` `auto`: output over this many bytes (all artifacts, before base64) is returned as links |
| `PSCAT_MCP_ARTIFACT_URL_TTL_SECS` | 1800 | how long a link works; the memory store drops the bytes then; with `gcs`, the signed URL's lifetime, at most 604800 (7 days) |
| `PSCAT_MCP_ARTIFACT_QUOTA_BYTES` | 33554432 | stored output one client (token, or the stdio user) may hold at once, or with `gcs` upload within one TTL, counted per server instance (`results_budget_exceeded`, `error.input.code` `artifact_quota_bytes`); at least `PSCAT_MCP_MAX_OUTPUT_BYTES` plus 1 KiB; with `memory`, at most `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES`, which bounds all clients together |
| `PSCAT_MCP_ARTIFACT_BUCKET` | unset | `gcs`: the bucket objects are uploaded to; required with `gcs` |
| `PSCAT_MCP_ARTIFACT_PREFIX` | `renders/` | `gcs`: the object name prefix (objects are `<prefix><render_id>/<n>.<ext>`): letters, digits, `.`, `_`, `-` and `/`, ending in `/`, with no leading `/`, empty segment or `..` |
| `PSCAT_MCP_ARTIFACT_SIGNER` | the metadata server's default account | `gcs`: the service account whose Google-managed key signs the URLs, through IAM Credentials `signBlob`; the runtime account needs the Token Creator role on it |
| `PSCAT_MCP_ARTIFACT_GCS_TIMEOUT_MS` | 10000 | `gcs`: the timeout of each call to the metadata server, IAM Credentials or Cloud Storage |
| `PSCAT_MCP_ARTIFACT_GCS_METADATA_ENDPOINT` / `_IAM_ENDPOINT` / `_STORAGE_ENDPOINT` | unset | **for tests only**, hidden from `--help`: the base URLs of the metadata server, IAM Credentials and Storage uploads, to point `gcs` at a local fake; `https`, or `http` on a loopback host, with no path. Never set them in a deployment. Signed URLs always name `https://storage.googleapis.com` |
| `PSCAT_MCP_LOG_FORMAT` | `json` (http) / `pretty` (stdio) | log format; `RUST_LOG` sets the level and, when set, replaces the default filter `info`; the `rmcp` crate's own targets are held to WARN and ERROR whatever `RUST_LOG` says, because rmcp logs whole JSON-RPC messages below WARN (the client's `initialize` parameters, requests with their arguments, results with their artifact links), so `RUST_LOG=trace` or `rmcp=trace` logs none of them; our own `pscat_mcp` targets follow `RUST_LOG` (`pscat_mcp=debug` works) |
| `PSCAT_MCP_CAPTURE_FILE` | unset (off) | debug, `serve-http` only: append every `/mcp` request (allow-listed headers, no tokens, no query string, body cut at 64 KiB) as a JSON line to this file, to record a client's traffic for the replay fixtures; the file also holds the PostScript sources sent, so it is refused unless the bind address is loopback, and never for production |

## Authentication

Which methods to run for which MCP client (ChatGPT, Claude, Codex and
others) is discussed in [`docs/AUTH_STRATEGY.md`](docs/AUTH_STRATEGY.md),
approved by the owner on 2026-11-17: WorkOS AuthKit is the identity provider
and production runs `bearer,oauth`. The step-by-step AuthKit setup, the
settings this server needs and the sign-in failure modes are in
[`docs/OAUTH_WORKOS.md`](docs/OAUTH_WORKOS.md); `scripts/oauth-check.py`
checks an access token against this server's rules before a client is
involved.

With `PSCAT_MCP_AUTH` unset (or listing `bearer`), the HTTP server will
not start without at least one bearer token, unless
`PSCAT_MCP_ALLOW_UNAUTHENTICATED=true` is set **and** the bind address is
loopback (with `PSCAT_MCP_AUTH` unset). An `oauth`-only server needs no
token, but its own settings (below), and a `none` server needs none
(staging only, below). Authentication uses only the request headers and runs before rmcp
sees the request, so an unauthenticated request cannot reach a render. A
rejected request's body is read and discarded (up to 8 MiB and 5 s, as for
an oversized one), so a client still uploading gets the `401` rather than a
connection reset.
Tokens are stored as SHA-256 digests, compared in constant time, and never
logged. Mount tokens as a file (`PSCAT_MCP_AUTH_TOKEN_FILE`) rather than
passing them on the command line.

**Several methods side by side.** `PSCAT_MCP_AUTH` (`--auth`) lists the
auth methods `/mcp` accepts, such as `bearer`, `oauth` or `bearer,oauth`.
Each method is tried in turn and a request passes if any of them accepts it.
A method that finds no credential of its kind passes the request on to the
next one. The no-auth mode (`none`) is consulted only when no other
method found a credential, so a wrong token is never let in as anonymous. A
refused request gets `401` (or `403` or `429`, below) with a JSON body (`error`,
`message`) and a `WWW-Authenticate` header that names every accepted scheme,
for bearer tokens `Bearer realm="pscat-mcp"`. Startup refuses an empty or
unknown list. With `PSCAT_MCP_AUTH` unset, the server behaves as described
above.

**OAuth (`oauth`).** For clients that cannot send a fixed header, such as
ChatGPT's connectors, the server is an OAuth 2.1 *resource server* as the MCP
authorization spec describes. It holds no passwords and runs no login page:
an external authorization server (an identity provider; the server works with
any standards-based issuer, and WorkOS AuthKit is the one chosen for the
hosted deployment, see [docs/OAUTH_WORKOS.md](docs/OAUTH_WORKOS.md) and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#oauth-resource-server)) signs
users in and issues JWT access tokens, and the server only checks them.

* A request without a token gets `401` with
  `WWW-Authenticate: Bearer realm="pscat-mcp", resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`
  (plus `scope="…"` when `PSCAT_MCP_OAUTH_SCOPES` is set). The origin and
  path come from `PSCAT_MCP_OAUTH_RESOURCE`, never from the request.
* `GET /.well-known/oauth-protected-resource/mcp` and
  `GET /.well-known/oauth-protected-resource` (RFC 9728) are served without
  authentication, only with `oauth` enabled. They name the resource, the
  issuer (`authorization_servers`) and the scopes.
* A token is accepted when its signature verifies against the issuer's JWKS
  (asymmetric algorithms only: RS/PS 256 to 512, ES256, ES384, EdDSA;
  `none` and `HS*` are refused, and the header's `kid` must name a key), its
  `iss` is the issuer, its `aud` contains the resource URL, it is within
  `exp` and `nbf` (with leeway), it carries every configured scope, and its
  user is on the allowlist. A bad, expired or foreign token gets `401` with
  `error="invalid_token"`. A valid token without a required scope gets
  `403` with `error="insufficient_scope"`. A user who signs in but is not
  on the allowlist gets `403` `not_allowed`, with no `error` parameter, so a
  client does not start a new consent loop that cannot help. Being able to
  sign in at the issuer is never enough by itself.
* **Email allowlist entries need the email in the access token.** An entry
  `email:<address>` matches only when the access token carries `email` and
  `email_verified: true` (a JSON boolean). Many providers put those claims
  in the ID token only; then use `sub:<subject>` entries, or have the
  provider add the claims to access tokens.
* With `bearer,oauth`, a static token is accepted by `bearer` without the
  issuer being contacted, and a JWT by `oauth`. Both use the `Bearer`
  scheme, so the challenge is one merged header,
  `Bearer realm="pscat-mcp", resource_metadata="…"`. A token both reject
  gets `401` `invalid_token` (bearer's wording, `invalid bearer token`,
  since it is first in the list) with `error="invalid_token"` in the
  challenge. A JWT of a user who is not allowed gets `403`, which outranks
  bearer's `401`.
* The JWKS is fetched over HTTPS on the first token that needs it, cached,
  and fetched again at most once per `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS`
  (an unknown `kid`, an expired cache or a failing issuer), each fetch
  bounded by `PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS` and 256 KiB. If the issuer is
  unreachable, keys already fetched stay in use; tokens with a key not yet
  seen get `401`. Requests without a JWT never wait for the issuer, and an
  issuer outage does not stop the server from starting.
* The principal of a request is `oauth:` plus 128 bits of a SHA-256 over
  issuer and subject, so jobs belong to the user, and no subject or email
  reaches the logs or job records.

**Connecting ChatGPT.** With developer mode on in ChatGPT, add a connector
(an app) with the server URL, `PSCAT_MCP_OAUTH_RESOURCE` (such as
`https://mcp.example.com/mcp`), and authentication **OAuth**. ChatGPT reads
the metadata, registers itself with the issuer (Dynamic Client
Registration; whether a manually entered client ID and secret work instead
has not been checked) and opens the provider's sign-in page. ChatGPT cannot send a static bearer header, so
OAuth is the way to connect it to a server that requires authentication.
Menu names change; OpenAI's developer-mode documentation has the current
steps.

**Connecting claude.ai.** Add a custom connector (Settings, Connectors) with
the same URL. Claude discovers the issuer from the metadata, registers
itself or uses the OAuth client ID and secret given in the connector's
advanced settings, and signs the user in (the steps follow Anthropic's
custom-connector documentation and have not been tried against this
server). Claude Code and scripts can keep
using bearer tokens beside it (`PSCAT_MCP_AUTH=bearer,oauth`).

Either way the provider must issue the access token with the audience set
to `PSCAT_MCP_OAUTH_RESOURCE` (RFC 8707's `resource` parameter, which MCP
clients send). An end-to-end sign-in from ChatGPT or claude.ai has not been
tried yet: it needs AuthKit configured and a deployment
([`docs/OAUTH_WORKOS.md`](docs/OAUTH_WORKOS.md)).

**No authentication, for staging (`none`).** `PSCAT_MCP_AUTH=none` (or
`bearer,none`) lets a request that carries no credential in as the
principal `anonymous`. It is never on by default, and the server logs a
loud warning at startup. **It exposes render CPU time and quota to anyone
who can reach the URL**, so use it only for a staging deployment.

`none` is **deprecated**: it is a stopgap that goes away once OAuth is
accepted in ChatGPT (tracked in #154). It is still available for now.
`oauth` and `none` are never enabled together, and the server **refuses to
start** with both (`oauth,none`, `bearer,oauth,none`): with both, a request
with no credential is answered as anonymous, so a client never gets the
`401` that starts OAuth sign-in. Drop `none`; `bearer,none` and
`bearer,oauth` are unaffected.

Why it exists: ChatGPT's custom MCP connector can't send a fixed
`Authorization` header. It offers only OAuth or "No authentication". `none` is
the stopgap for trying ChatGPT against a staging service before an identity
provider for `oauth` is set up. Claude, scripts and CI keep using bearer tokens beside it:
`bearer,none` admits a valid token as the token's principal, a request with
no `Authorization` header as anonymous, and a wrong or malformed token as
`401`, never as anonymous.

While `none` is on, every anonymous request is checked (a request with a
bearer token never is) in this order:

1. **Expiry**: with `PSCAT_MCP_AUTH_NONE_UNTIL`, a running server answers
   `403 unauthenticated_access_expired` once the time passes (a Cloud Run
   instance can outlive it). A time that has already passed at startup does
   not refuse to start (a scale-to-zero cold start must not take bearer
   clients down): the server starts with `none` disabled, logs a warning,
   lists only the other methods in `auth_methods`, and answers a request
   with no credential the same `403`. With `none` as the only method, every
   request gets that `403`. A malformed time still refuses to start.
2. **Source allowlist**: with `PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS`, an address
   outside it gets `403 source_not_allowed`.
3. **Rate limit**: `PSCAT_MCP_AUTH_NONE_RATE_LIMIT` requests per
   `PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS` per client address, then `429
   rate_limited` with a `Retry-After`. The limit is kept per server
   instance, so several instances multiply it. Every resource limit
   (`PSCAT_MCP_MAX_*`, the render concurrency cap and its queue) applies
   unchanged.

The client address is **not** read blindly from `X-Forwarded-For`, which a
client can set to anything. By default it is the TCP peer address and the
header is ignored. Behind a proxy, set `PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS` to
the number of proxies that append to the header; the address is then that
entry counted from the right, and everything left of it, which the client
controls, is never used. The server must then be reachable only through those
proxies. Behind Google's front end, hops 0 would make every client one
address (the front end's), so the limit and the allowlist would mean
nothing: see [`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md#no-authentication-for-staging).

Because every anonymous caller is the same principal, one could poll, fetch
or cancel another's async job, so the server **refuses to start with `none`
and async jobs** (`PSCAT_MCP_ASYNC_JOBS`; MCP Tasks need them too), and for
the same reason with `none` and in-memory artifact links
(`PSCAT_MCP_ARTIFACT_LINKS=memory`: one anonymous client could fill the
results budget they share with authenticated clients). Off
loopback, `none` is the only way to run without a token:
`--allow-unauthenticated` alone is still refused unless the bind address is
loopback. Anonymous requests carry `auth_method=none` in the logs; a
rejection logs only its `reason` (`source_not_allowed`, `rate_limited`,
`unauthenticated_access_expired`), never a client address.

**Log fields.** The startup lines carry `auth_methods`, the enabled methods
(for example `bearer,oauth`). Each `/mcp` request's span carries
`auth_method`, the method that accepted it (`bearer`, `oauth`, or `none`:
under `--allow-unauthenticated` or the explicit `none` mode), so the
request's response line can be counted per method. A refused request logs
`rejected unauthenticated request` with `reason` (`missing_credentials`,
`malformed_credentials`, `invalid_token`, from `oauth` `insufficient_scope`
or `not_allowed`, from `none` `source_not_allowed`, `rate_limited` or
`unauthenticated_access_expired`) and `rejected_by`,
the methods that rejected a credential (empty when none was presented).
`oauth` also logs `oauth: token rejected` with a fixed `reason` (such as
`expired`, `wrong_audience`, `bad_signature`, `unknown_kid`,
`jwks_unavailable` or `not_allowed`), and a warning when a JWKS fetch fails.
Credentials, subjects and email addresses are never logged.

## Security model

PostScript is a programming language, so every program is treated as
untrusted code. Each render or validation:

1. is checked **before execution**: source size, format, dpi, page size
   (≤ 8000 px a side) and pixel count, and that `halftone` goes only with PNG;
2. runs in a **new worker process** (`render-worker`, the same binary) with
   an empty environment, cwd `/`, its own process group, and rlimits: CPU
   time, no file writes (`RLIMIT_FSIZE=0`), 64 file descriptors, no core
   dumps, and on Linux an address-space cap;
3. on Linux, runs under a **seccomp filter** the worker installs before it
   reads its job: it cannot create a socket of any kind (so it cannot
   reach the network or a cloud metadata server), use io_uring, `ptrace`
   or `process_vm_*` another process, take another process's descriptors
   (`pidfd_getfd`) or load BPF; those calls fail with `EPERM`. A worker
   that cannot install the filter refuses the job (`worker_failed`). The
   server makes itself non-dumpable, so the worker cannot read or write the
   server's memory through `/proc` either;
4. runs under **PSCat's own limits**: steps, time and heap growth (exact per
   process via PSCat's counting allocator), plus a pixel cap on
   `setpagedevice`; file reads are confined to PSCat's built-in `lib/` (also
   with a font-pack directory configured), and there are no write roots;
5. is **SIGKILLed** at the wall-clock deadline, or when the client cancels
   or disconnects, even if it is stuck inside a single long operator;
6. has its output size, page count and stdout capped.

Artifact links (`delivery` `url`/`both`, off by default) change none of
this: the store only ever receives the finished bytes a worker returned, and
the worker gets nothing new. A link's `render_id` is random (a UUID v4), a
link is served only to the client that made it, and links are never logged,
not even in capture mode, where a request naming one is recorded with its id
redacted. rmcp's own DEBUG and TRACE lines (`rmcp::service` and its
transports) dump whole messages, links included, so the log setup holds every
`rmcp` target to WARN and ERROR whatever `RUST_LOG` says (a filter on the
log layer, `src/telemetry.rs`): raising `RUST_LOG` to `trace` or `rmcp=trace`
does not log a link, a `render_id` or artifact bytes (`tests/logging.rs`).
rmcp's WARN and ERROR lines are kept (a rejected Host or Origin, a failed send, a JSON-RPC error response with its error); our errors never echo a link.

With Cloud Storage links (`gcs`) only the server process talks to Google:
it reads an access token from the metadata server, uploads, and has IAM sign
the URLs. The worker is given no credential and no new capability, and on
Linux its seccomp filter refuses every socket, so it cannot reach the
metadata server itself; on Cloud Run the metadata server is otherwise
reachable from every process in the container, so that filter, not the
absence of a key, is the boundary. A signed URL is a bearer capability:
anyone holding it can fetch that one object until it expires, so it is
never logged, never put in an error message, and capture mode records any
`http(s)` URL a client sends with its query string (where the signature
is) redacted. Use a short TTL and a bucket with uniform access, public
access prevention and a 1-day lifecycle rule
([`docs/CLOUD_RUN.md`](docs/CLOUD_RUN.md#cloud-storage-artifact-links)).

PSCat has no operators for processes, network or environment access. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full table (and [docs/CLOUD_RUN.md](docs/CLOUD_RUN.md) for deployment) of which
control is enforced by PSCat and which by the server.

## Current limitations

* **Output over the output limit is refused, links or not.** Output over
  `PSCAT_MCP_MAX_OUTPUT_BYTES` returns `output_too_large`, with
  `error.input.code` `output_bytes` and the limit and the size reached in
  `error.input.limit_bytes` and `reached_bytes`, whatever the `delivery`.
* **Memory links are one instance's.** The memory store
  (`PSCAT_MCP_ARTIFACT_LINKS=memory`) has the async jobs' limits: one
  instance, lost on restart. Its `pscat://` links are MCP resources, which a
  browser cannot open; use `gcs` for `https` links. A client on a protocol
  before 2025-06-18 may not know `resource_link` blocks; the link is also in
  the text and in `structuredContent`.
* **Cloud Storage links are bearer capabilities, and their quota is per
  instance.** Anyone holding a signed URL can fetch the object until it
  expires; it cannot be revoked before then (short of rotating the signer's
  keys or deleting the object). The upload quota is counted in each
  instance's memory. Objects stay in the bucket until its lifecycle rule
  deletes them, whatever the TTL. Only a fake of the Google APIs is used in
  CI; the real-bucket check is in [`docs/LOCAL_TESTING.md`](docs/LOCAL_TESTING.md#real-bucket-check).
* **SVG output is large, so a large SVG is a blob.** PSCat draws text as
  glyph outlines (paths, not `<text>` elements) and a gradient as many
  separate shapes, so an SVG grows quickly: an 88 KB SVG came from a
  600x400 pt scene with a gradient sky, about 60 stars, some curves and
  three short strings (#31), and a program that draws thousands of marks
  is megabytes. Claude Code shows a text result inline only up to about
  50,000 characters and saves a larger one as a JSON array in a `.txt`
  file, whatever `MAX_MCP_OUTPUT_TOKENS` says (#64). So an SVG up to
  `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES` (40 KiB by default, summed over every page of
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
* **The memory limit is lower than PSCat's.** `PSCAT_MCP_MAX_MEMORY_BYTES`
  defaults to 512 MiB, half the 1024 MiB of PSCat's own MCP preset, and
  since PSCat 0.7.0-rc.5 `clip`, `shfill`, Type 1 fonts and the PDF writer
  (to the limit plus 16 MiB) are held to it too. So a heavy clip (tens of
  thousands of curves at a high dpi) or a large PDF can fail here with
  `limit_exceeded` (`limit: memory`) where the plugin draws it; raising the
  limit needs a larger instance (see `docs/CLOUD_RUN.md`).
* **Worker network namespace.** The worker shares the host's or container's
  network namespace, and on Cloud Run the metadata server
  (169.254.169.254, which hands out the service account's token) is
  reachable from it. gVisor (Cloud Run's first generation) does not block
  that. PostScript has no network operators, and on Linux the worker's
  seccomp filter refuses every socket, so even code running in the worker
  through an interpreter bug cannot open a connection. Seccomp cannot
  filter `/proc/<pid>/mem` by path, so on Linux the server also makes
  itself non-dumpable at startup (`PR_SET_DUMPABLE` 0): a worker, which
  runs as the same user, cannot open the server's memory (or `environ`,
  `fd`) to read a token, and the server never writes a core dump. None of
  this applies on macOS (development only).
* **The server process is not filtered.** The server needs the network,
  so code execution in the server itself (not the worker) would reach the
  metadata server and the service account's token. Untrusted input is
  never processed in the server, which keeps that surface to the HTTP,
  MCP and auth code.
* **Survival after a server kill.** If the server is SIGKILLed itself, a
  running worker lives until its CPU or time limit, a few seconds.
* **Fonts.** The fonts compiled into PSCat are available (base-35
  equivalents, Liberation, TeX Gyre). The downloadable font packs (`extras`,
  `cjk-thai`) are available only if the operator sets
  `PSCAT_MCP_FONT_PACK_DIR` (see [Font packs](#font-packs)); the worker's
  environment is otherwise empty, so it does not find a pack installed for
  the user. Without the setting a program that selects a pack face gets
  PSCat's stand-in (Helvetica, or Courier for the monospaced faces), and
  `review_sheet` labels in Korean, Japanese, Chinese or Thai are drawn as
  `?`. `describe_art_capabilities` and, with the setting, `get_renderer_info`
  report the packs the workers can see.
* **Chalkboard vector output is large.** A board's SVG and PDF keep every
  chalk grain as a vector mark: the default board is about 0.3 MB as PNG,
  2.6 MB as PDF and 7 to 10 MB as SVG. A board's SVG is always over the
  default `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES`, so it comes back as a blob, and a
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
  pack: without `PSCAT_MCP_FONT_PACK_DIR` pointing at a directory that holds
  it, they are drawn as `?`. A label is text only, never PostScript.
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
python3 -I -m unittest discover -s scripts/tests -t scripts/tests   # scripts/oauth-check.py tests
```

GitHub Actions runs these on every pull request and push to `main`:

* `.github/workflows/ci.yml`:
  * fmt and clippy;
  * the test suite on Linux and macOS;
  * a `cargo check` with the minimum Rust version in `Cargo.toml`;
  * the agent evals' offline unit tests (no model);
  * the offline unit tests of the server evals' recorder;
  * the offline unit tests of `scripts/oauth-check.py`.
* `.github/workflows/container.yml`: the amd64 image, the smoke test, and
  the server-layer evals against that image.

| Suite | Covers |
|---|---|
| `tests/backend.rs` | real PNG/SVG/PDF renders with pixel checks, a generated page held to its plan (a `handwrite` note's height laid out in the worker within a pixel budget, a note over the budget refused before it is drawn, a page that misses the plan a bad job), deadline kill and no surviving process, cancel kills the worker, `halftone` in the worker job, PSCat step and memory limits (a PDF too big for the memory limit refused while it is written), file read/write refused, output caps, crash/protocol errors, the child's rlimits/env/cwd, the worker's art catalog, a `review_sheet` composed in one job and its exact size checked against the budget before any cell runs |
| `tests/worker_sandbox.rs` | Linux only: a child that installs the worker's seccomp filter gets `EPERM` from `socket` (IPv4, IPv6, Unix, netlink, packet), `socketpair`, `connect` to 169.254.169.254, `bind`, `listen`, `accept`, `io_uring_setup`, `ptrace`, `process_vm_readv` and `pidfd_getfd` (and `PermissionDenied` from std's TCP and UDP), while pipes, files, a 64 MiB allocation and a new thread (which inherits the filter) still work; a child of a process that ran the server's `harden_server` cannot open its parent's `/proc/<pid>/mem` (`EACCES`), nor can the parent of a real `serve-stdio` open the server's; a real `render-worker` started by the backend carries one more seccomp filter than its parent (`Seccomp_filters`, as a container runtime may have installed one already) and `NoNewPrivs: 1`, and holds no socket. Every other suite renders through real workers, so on Linux it runs under the filter too |
| `tests/mcp_stdio.rs` | rmcp client over stdio: initialize, tool list (one `EXPECTED_TOOLS` table), annotations and schemas, the art catalog (index, kind pages and named entries with size ceilings, argument errors, no server paths, load lines usable in a render), PNG image content with pixel checks, `halftone` (pure black-and-white pixels, tone kept, every page, refused for SVG/PDF), SVG/PDF resources, PostScript errors without images, a PDF over the memory limit as `limit_exceeded` (`memory`), pre-execution rejections, argument type errors with the structured envelope, validation, renderer info checked against `Cargo.lock` |
| `tests/svg_cap.rs` | how SVG is returned, over stdio: up to `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES` a text resource, over it a blob resource holding the same SVG byte for byte (`returned_as` in `structuredContent`, every key checked against the output schema, the text summary naming the limit), every page counted together and never mixed, exactly at the limit still text, 0 for every SVG a blob, PNG and PDF unaffected, an SVG over the output cap `output_too_large` with `error.input.code` `output_bytes` and its whole size, the limit in `get_renderer_info` and the tool descriptions, its default pulled down by a small output cap, a limit above the output cap refused at startup, the deprecated alias working with a startup warning, and both names set together refused at startup |
| `tests/chalkboard.rs` | `chalkboard_math` over stdio: the tool's schema matches the plugin's arguments, a default 1600×800 px (800×400 pt) board (dark slate, bright chalk, same image twice), the size reported in points and pixels, `board: false` transparent and an RGB board, PDF inline, SVG as text with the limit raised and as a blob by default, and over the output cap `output_too_large` with board advice, the LaTeX size limit, an unsupported command as `postscript_error` naming it, a stray `$` as `unclosed_math_delimiter`, the check pass's `CKPROBLEM` lines kept out of every reply, bad board configs and the pixel limit before execution, argument type errors with the envelope, and pathological LaTeX (10,000 nested `\frac`, 50,000 nested braces, an `\underbrace` chain) ending in a structured error while the server keeps serving |
| `tests/handwrite.rs` | `handwrite` over stdio: the tool's schema matches the plugin's arguments, a default 612×146 note with ink pixels in the ink colour, ink, size, width, dpi and `paper: ruled` (ruled lines and margin rule, none on plain paper), the same seed twice giving identical bytes, the text limit, a note too tall for the pixel cap or the side limit as a structured error (and a 500 000-line text refused, not killed) with the server serving the next call, the measuring pass under the step limit (and a limit hit inside a megabyte word keeping the reply small), argument errors through the envelope, and empty, whitespace-only, non-Latin and control-character text |
| `tests/review_sheet.rs` | `review_sheet` over stdio: the schema matches the plugin's arguments (no `$ref`, the cell cap in it), a 2×2 sheet of real programs with each cell at its true size, colour, hairline and position checked by pixel probes, each cell byte for byte the `render_postscript` page of its program (72, 144 and 100 dpi), nearest-neighbour zoom, labels and captions turned off (exact sheet sizes), a failing cell drawn as far as it got with its error while the others render, a runaway cell stopped at its own step limit and at its share of the sheet's time while the rest render, the output limit, every budget refused before execution (and nothing executed, by the service's counters), argument errors through the envelope, a reply under 24 KB with 32 failed cells, the worker's own sheet deadline as a `timeout` envelope valid against the output schema (and counted), a sheet at the pixel limit (16,000,000 and 64,000,000) with one cell zoomed 16x, a cell's memory limit giving the same result alone and beside a 62 MB sheet, and odd programs and labels never crashing the server |
| `tests/jobs.rs` | async jobs with the real worker and short deadlines: a job past the sync deadline completes and its PNG is fetched (twice), states seen in order (`queued` behind a running job, `running`, `done`), a failed job keeps its PostScript error, `cancel_render` and `tasks/cancel` kill the worker (by `pgrep` and the counters) and a queued job never starts, the job deadline kills and reports `timeout`, an expired result is `unknown_job`, the per-client and total caps and the results budget with their codes, a client over its share of the budget refused with the limit named in `error.input` while the budget has room (`get_renderer_info` reports the share), argument errors through the envelope, shutdown kills running jobs, the job tools listed only with jobs on (also over HTTP), and the guide note naming them as missing exactly when they are not listed, two bearer tokens over HTTP with a small budget (one fills its share and is refused with `results_per_principal_bytes` while the other still starts, neither sees the other's jobs, and cancelling frees the share at once), a cancelled job never holding a principal at its share (unit tests of the store), another bearer token sees `unknown_job` with the same message as an unknown id, and the Tasks extension: a Tasks-capable rmcp client's slow render becomes a task past `PSCAT_MCP_TASK_AFTER_MS` with the same worker (one worker, one execution, past the sync deadline) and `tasks/get` completes it, its quick calls (render, `handwrite`, a failed render) return the result directly with the job caps, running-job permit and budget all full and untouched, `tasks/cancel` of a promoted task and `notifications/cancelled` before the threshold both kill the worker, a slow call that cannot become a task stops at the sync deadline with `timeout`, a threshold of 0 makes every eligible call a task at once (`handwrite`, a failed render), a plain client or one on a server without `--tasks` gets the sync result, and over HTTP a later `tasks/get` (a new handler) finds the task for its token only, and a Tasks-capable rmcp client over HTTP that drops a call before the threshold kills its worker and makes no job, and a raw TCP client (a hand-written HTTP request, no rmcp) that closes or resets its connection mid-render, on a plain call and on a Tasks call (2026-07-28 shape) before the threshold, leaves no worker (by `pgrep` tag) and no job; a `start_render` job whose SVG is over `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES` (set through the environment, over stdio) ends `done` and `render_result` returns it as a blob resource holding the whole SVG, and `render_result` returns SVG as text or as a blob by the limit, like `render_postscript` |
| `tests/guide.rs` | prompts and resources over stdio: `guide/` is byte for byte `plugin/skills/` of the pinned PSCat checkout in `$CARGO_HOME/git/checkouts` (found without running cargo, so it can't wait on the package-cache lock; the byte comparison is skipped with a message when the checkout isn't there, and fails when `CI` is set), and `guide/SOURCE.json` names the commit in `Cargo.lock`; `initialize` advertises prompts and resources; every resource listed and read (the note, then PSCat's text verbatim; under 24 KB; the note lists exactly this server's tools); every prompt with and without arguments; the font-pack line (none without a pack directory; with the `cjk-thai` fixture, the pack the catalog lists, and each size as read); with a worker that cannot start, the guide says the packs "could not be read just now" and prompts still answer, and once the worker starts the next request lists the packs (a failed catalog read is not kept); unknown URIs and prompts and bad arguments as MCP errors; every runnable `postscript` block in the guides renders, and every library they name loads |
| `tests/font_pack.rs` | the font-pack directory with a 13 KB `cjk-thai` stand-in (`tests/data/font-pack`): the worker finds the pack face and, unset, does not; a `fonts/catalog/` beside a copy of the server binary (the executable-directory case) is found with the setting unset, and the same copy without it is not; the catalog lists the pack and no path (also in `get_renderer_info`); programs cannot read the directory (absolute, relative, `..`, symlink out, `filenameforall`, a planted `lib/`), nor write there; the worker's environment is `PSCAT_HOME` only (a fake worker), and the server's `HOME`, `XDG_DATA_HOME`, `PSCAT_HOME` and `PSCAT_ROOT` are not inherited; a Japanese `review_sheet` label differs from `???` with the pack and is identical without it; a missing path or a file refuses to start; `filename status` answers for the compiled-in `lib/` only (not a planted `lib/`) and is `false` for every other name |
| `tests/mcp_cancel.rs` | `notifications/cancelled` for an in-flight render kills its worker and frees the render slot |
| `tests/logging.rs` | the log filter: over stdio, an `initialize` whose `clientInfo.name` is a marker leaves the marker and rmcp's "Service initialized as server" line out of stderr, also with `RUST_LOG` raising `rmcp` to INFO or TRACE; over HTTP the same line is absent by default and with the override; the error codes in the logs: over HTTP, a memory hog, a step loop, an interpreter time limit and a PostScript error each log a `render finished` line with its wire `code` (and `limit`), a queue timeout logs one `server_busy` line with `queue_wait_ms`, and no source, defined name, error command or token is in any line; artifact links: a `delivery: url` render and the `resources/read` of its link leave neither the `render_id`, a `pscat://render/` link nor the artifact's bytes in the logs (default filter, `pscat_mcp=trace`, `rmcp=trace,pscat_mcp=trace` and `trace`) or in the capture file, which records the read with the id redacted, and a not-found read does not echo the link; the rmcp clamp: no rmcp line below WARN under those filters, and an rmcp WARN (a request with a disallowed `Origin`) still logged at `trace` |
| `tests/artifacts.rs` | artifact delivery (#139) over an in-process pipe and over HTTP: with no store, `delivery` `inline` or `auto`, and with the memory store, no `delivery`, `inline` or `auto` under the threshold, every tool's result (PNG pages, SVG, PDF, `chalkboard_math`, `handwrite`, `review_sheet`) is the same as today's, with no link field; `url`/`both` with no store `invalid_request` (`error.input.code` `delivery`, a message saying to retry with `delivery: "inline"`, `start_render` too); `url` a manifest of one `resource_link` per page (three pages) with `download_url`, `expires_at` and `render_id`, the links named in the text, no bytes inline and `resources/read` returning each page; `both` the same bytes inline and by link for PNG, SVG and PDF; links from every render tool; `auto` over a threshold; diagnostics (stdout, a PostScript error) inline; the operator default and an argument overriding it; the per-client quota refused with `artifact_quota_bytes`; expiry after the TTL with an injected clock, freeing the quota; a `start_render` job with `delivery: url` storing its result once across two `render_result` calls; over HTTP, another bearer token's `resources/read` of a link failing exactly like a never-issued one. A fixture (`tests/data/inline-results.json`, frozen before #139) pins the inline result builders byte for byte, and unit tests in `src/artifacts.rs` cover the store (owner, TTL, quota, the shared budget, link redaction) |
| `tests/artifacts_gcs.rs` | the Cloud Storage store (#140) against a local fake of the metadata server, IAM Credentials and Cloud Storage (no Google endpoint is called; the fake IAM signs a blob as its SHA-256, so the test recomputes the signature): the exact upload (path, `uploadType=multipart&ifGenerationMatch=0`, bearer token, metadata `name`, `contentType`, `contentDisposition`, the bytes) and the exact signed URL for a fixed injected time, built independently in the test, with `expires_at`; a three-page manifest under one `render_id`; the token cached, fetched once by concurrent calls, refreshed 300 s before expiry or at half a short life; the signer from the metadata server once, or the configured one with no fetch; the per-principal quota within the TTL; the metadata server down, Storage 412 and 500, IAM 403, each endpoint hanging past the timeout and nothing listening all `storage_unavailable` with no URL, token, bucket or body in the message; the start-up checks (bucket required, prefix, TTL over 7 days, signer, endpoint overrides, `none` refused); through the tools, `url` links in all three places for `render_postscript`, `handwrite` and `review_sheet`, `resources/read` serving nothing, `inline` uploading nothing, a failed upload or signing an error for `url` and an inline result with `delivery_note` for `both`, `auto` and a `start_render` job, the quota refusing `url` and `both` alike, the tool description; and, through the binary over HTTP at the default filter and `pscat_mcp=trace`, no signature, credential, token, response body or render id in the logs, one WARN line with codes only, and the capture file recording a signed URL sent back in a tool argument and in `Mcp-Name` with its query redacted. Unit tests in `src/gcs.rs` check the canonical request and string-to-sign against Google's conformance vectors |
| `tests/http.rs` | 401 without or with a wrong token (and zero renders executed), rmcp HTTP client render, legacy protocol clients over stateless HTTP, bounded concurrency with correct per-request output, `server_busy`, loopback-only unauthenticated mode, Host and Origin checks, `Accept` handling (`application/json` alone, `*/*`, `application/*` and none are served as JSON; `text/html`, SSE-only and `q=0` stay 406, GET stays 405; 401, 403 and 413 come first), body limit (413 with `Content-Length` and for chunked bodies, with no render; the limit follows the largest of the source, `handwrite` text and `review_sheet` source limits), client disconnect kills the worker and frees the slot, guide prompts and resources through the rmcp client and as raw stateless requests at 2026-07-28 and 2025-06-18 (no worker started) |
| `tests/auth.rs` | several auth methods side by side, with a test-only method beside `bearer`: `PSCAT_MCP_AUTH=bearer` answers every request (status, `WWW-Authenticate`, body) exactly as the default configuration; each method alone and both together, a request one method rejects and the other accepts, a denial naming both challenges; a job started by one method's principal is `unknown_job` to another method's and to another principal of the same method; the binary refuses `oauth` without its settings, an unknown, empty or repeated list and `bearer` without a token, and keeps the loopback rule for `--allow-unauthenticated`; its JSON logs carry `auth_methods` at startup, `auth_method` on an accepted request's response line and `reason`/`rejected_by` on a rejection, and never a token. Unit tests in `src/auth.rs` cover the chain (order, the fallback rule, which rejection is reported) and the challenge merging |
| `scripts/tests/test_oauth_check.py` | `scripts/oauth-check.py` (offline, no model, no server): synthetic unsigned tokens against the checklist that mirrors `src/oauth.rs` (shape, `alg`/`crit`/`kid`, the `kid` and key fit in a JWKS, `iss`, `aud` as string or array, `exp`/`nbf` with leeway, `sub`, scopes, `email_verified` as a JSON boolean, `sub:`/`email:` allowlist entries); JWKS discovery against a loopback server; exit codes; the token and its signature never appear in the output, and a token on the command line is rejected without being echoed |
| `tests/oauth.rs` | the `oauth` method against a local mock authorization server: valid RS256 and ES256 tokens, an audience array and a verified email are accepted; expired, without `exp`, not yet valid, wrong or missing audience, wrong issuer, bad signature, tampered payload, an ES256 header on the RSA key, an HS256 token signed with the public key, `alg: none`, an unknown `kid`, no `sub`, a non-JWT and an oversized token get `401 invalid_token`; a user not on the allowlist (or with an unverified email) gets `403 not_allowed` without an `error` parameter; missing scopes get `403 insufficient_scope`; no PostScript runs for any refused request; `bearer,oauth` accepts a static token without contacting the issuer and a JWT, and answers garbage with one merged challenge; the Protected Resource Metadata is served at both well-known paths without auth (the `resource_metadata` URL of a real 401 included), independent of `Host`, and not at all without `oauth`; a hung issuer bounds JWT requests by the JWKS timeout with one fetch between them while requests without a JWT (static token, no header, garbage; `oauth` first or alone) go straight through; a known key past its TTL is used without waiting while the issuer hangs; unknown `kid`s trigger one JWKS fetch, and a rotated key is picked up; discovery (RFC 8414, OpenID Connect fallback, explicit JWKS URL, a mismatched issuer refused); the binary refuses an incomplete or unsafe configuration without echoing allowlist entries, and its logs carry `auth_method=oauth` and fixed reasons but never a subject, email or token |
| `tests/auth_none.rs` | the explicit no-auth mode (deprecated) and its guard rails, mostly through the real binary over HTTP: `none` alone admits an anonymous request, logged as `auth_method=none` with a startup warning and no client address in the logs; `bearer,none` admits no header as anonymous and a valid token as bearer, and a wrong or malformed token is `401` (in either list order, never anonymous); without `none`, nothing is anonymous and the `PSCAT_MCP_AUTH_NONE_*` settings alone change nothing; only an explicit `none` runs off loopback; the CIDR allowlist (`403 source_not_allowed`, no bare `401`); the rate limit (`429` with `Retry-After`, repeated, a valid token never counted); a forged `X-Forwarded-For` can neither escape the allowlist nor reset or spend another client's limit, with and without a trusted hop; the startup refusals (an invalid time, a bad CIDR, a zero limit, async jobs, Tasks or memory artifact links with `none`; `oauth,none`, `none,oauth` and `bearer,oauth,none` refused while `bearer,none` and `bearer,oauth` start); an already expired time starting with `none` disabled (`bearer,none`: a token is `200`, no header is `403 unauthenticated_access_expired`, the startup log warns and `auth_methods` lists only `bearer`; `none` alone: every request is `403`); and, in-process with an injected clock, the refusal after the expiry of a running server |
| `tests/eval_server.rs` + `evals/server/scenarios/` | server-layer evals, no model: JSON scenario files (the agent evals' format) run with the rmcp client over stdio, over HTTP with a bearer token, over HTTP with async jobs on (`PSCAT_MCP_ASYNC_JOBS=true`, so scenario 66 runs there too) and, with `scripts/container-eval.sh`, in the container image. Steps call a tool or send another request (`resources/read`, `prompts/get`, ...). Exact assertions on `isError` and the error code, JSON-RPC errors, content block and MIME types, PNG size and pixel samples, SVG and PDF signatures, `structuredContent` fields, elapsed-time bounds and the number of renders executed: the first prompt in LOCAL_TESTING, every scenario in `examples/agent-scenarios.md`, and every error code (`timeout`, `limit_exceeded`, `server_busy`, ...) except `cancelled` (`tests/mcp_cancel.rs`) and `storage_unavailable` (`tests/artifacts_gcs.rs`, which needs a fake of the Google APIs). Prints a table and writes JSON and JUnit to `target/eval/`. See [Server-layer evals](docs/LOCAL_TESTING.md#10-server-layer-evals) |
| `tests/eval_replay.rs` + `evals/server/fixtures/` | replay of recorded client traffic against the in-process server: what Claude Code and the rmcp client really sent (headers, `initialize`/`server/discover`, `tools/list`, `tools/call`) plus synthesized cases, each marked real or synthesized; asserts 2xx and valid JSON-RPC. The `Accept: application/json`-only fixture (#17) is an ordinary passing case. Also tests capture mode: no token, query string or unlisted header value reaches the file |
| `evals/agent/` (`scripts/agent-eval.sh`) | local and manual only: real agents (Claude Code headless on the claude.ai subscription, optionally Codex) run the scenarios in `examples/agent-scenarios.md` and the LOCAL_TESTING first prompt over stdio and HTTP, checking tool choice, verbatim arguments, results, error codes and the answer; a billing guard refuses API-key billing. CI runs only its offline tests (guard and transcript checks on recorded fixtures). See [Automated agent evals](docs/LOCAL_TESTING.md#9-automated-agent-evals) |

## Contributing

[`SDLC.md`](SDLC.md) describes how changes reach `main`: issues with
`priority:`, `difficulty:` and `model:` labels, a branch per issue, the
required CI checks, review, and squash merges.

## License

MIT OR Apache-2.0. PSCat is (MIT OR Apache-2.0) AND OFL-1.1 AND LPPL-1.3c
because of its embedded fonts; see the PSCat repository.
