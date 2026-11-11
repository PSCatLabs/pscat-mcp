# Agent acceptance scenarios

Manual acceptance cases for an MCP client (Claude Code, Claude Desktop, an
agent SDK, or MCP Inspector) connected to `pscat-mcp-server`, over stdio or
Streamable HTTP. Each case gives a prompt to type, the tool call the agent is
expected to make, and the result that counts as a pass.

`scripts/agent-eval.sh` runs these cases with Claude Code (and optionally
Codex) and checks the transcripts; see "Automated agent evals" in
[`docs/LOCAL_TESTING.md`](../docs/LOCAL_TESTING.md#9-automated-agent-evals).
The case files are in [`evals/agent/cases/`](../evals/agent/cases/). The
deterministic part of every case here (results, error codes, sizes, pixels)
is also checked without a model by the server-layer scenarios in
[`evals/server/scenarios/`](../evals/server/scenarios/), which run in CI; see
[Server-layer evals](../docs/LOCAL_TESTING.md#10-server-layer-evals).

The server exposes seven tools:

| Tool | Arguments | Returns |
| --- | --- | --- |
| `render_postscript` | `source` (required), `format` (`png` default, `svg`, `pdf`), `width`, `height` (points), `dpi` (9–1200, default 72), `halftone` (boolean, PNG only, default false) | One content block per artifact (PNG `image`, SVG text resource up to 40 KiB in all and blob resource above, PDF blob resource) + a text summary; `structuredContent` envelope `{ok, format, mime_type, page_count, artifacts[], program_output, steps_used, elapsed_ms, renderer}`, each artifact with `returned_as` (`image`, `text`, `blob`) |
| `chalkboard_math` | `latex` (required), `size`, `irregular`, `slope`, `jitter`, `align`, `color`, `board`, `ghosting`, `seed`, `width`, `height` (points, default 800×400), `dpi` (default 144), `format`, `config` | The board as `render_postscript` returns a page, with the same envelope |
| `handwrite` | `text` (required), `size` (points, default 36), `width` (points, default 612), `paper` (`plain` default, `ruled`), `ink` (`"R,G,B"` in 0–1, default `0.13,0.14,0.34`), `jitter` (default 16), `seed` (default 1985), `dpi` (default 72) | The note as a PNG, with the same envelope as `render_postscript`; the page height is fitted to the text |
| `review_sheet` | `entries` (required: each `source`, optional `label`, `width`, `height`, `dpi` list), top-level `width`, `height` (default 612×792), `dpi` (default `[72]`; an item is a number or `{dpi, zoom}`), `labels`, `sizes` (default true) | One PNG `image` (the sheet) + a text line per cell; `structuredContent` `{ok, format, mime_type, width_px, height_px, bytes, failed_cells, cells[]{row, column, dpi, zoom, width_px, height_px, x, y, ok, error?{code, limit?, message}}, steps_used, elapsed_ms, wall_ms, renderer}` |
| `validate_postscript` | `source` | Text summary; `structuredContent` `{ok, valid, syntax{ok,tokens,error?}, execution_error?{name,limit,report}, lint[], page_count, program_output, steps_used, renderer}` |
| `describe_art_capabilities` | none, or `kind`, or `names` (≤ 20), with `kind` `offset`/`limit` | Under 24 KB, same information as text and as `structuredContent` (Claude Code shows the model the JSON). None: `{view:"index", index[]{kind,count,names[]}, details, font_packs, pscat_version, catalog_signature, renderer}`. `kind`: `{view:"kind", kind, total, offset, next_offset?, summaries[]{name,description}}`. `names`: `{view:"names", entries[] (name, kind, description, parameters[], source, load, example, availability), not_found[], omitted[]}` |
| `get_renderer_info` | none | PSCat version, formats, limits in force |

It also offers the PSCat plugin's skills as `text/markdown` resources
(`pscat://guide/...`) and three prompts (`diagram`, `generative_art`,
`chart`); see scenario 10.

Every failure is `isError: true` with a text explanation and
`structuredContent.error = {code, message, postscript?, program_output?, input?}`
(`input` is `chalkboard_math`'s `{code, token?}` for a LaTeX or board error,
`handwrite`'s `{code: "height"}` for a note too tall, the output cap's
`{code: "output_bytes", limit_bytes, reached_bytes}` on `output_too_large`, and `review_sheet`'s
`{code}` naming the budget or argument that refused a sheet: `cells`,
`sources`, `source`, `deadline`, `cell_side`, `cell_pixels`, `sheet_side`,
`sheet_pixels`, `sheet_bytes`, `dpi`, `page`, `arguments`).
`code` is one of: `invalid_request`, `source_too_large`, `unsupported_format`,
`invalid_dimensions`, `too_many_pixels`, `postscript_error`, `limit_exceeded`,
`timeout`, `too_many_pages`, `output_too_large`, `server_busy`, `cancelled`,
`worker_failed`.

Values below assume the default limits (256 KiB source, 20 s timeout,
16,000,000 pixels, 8000 px per side, 10 pages, 8 MiB output, SVG as text up to 40 KiB and as a blob above). Every result
quoted was observed against the `pscat-mcp:smoke` container image (PSCat
0.7.0-rc.4); timings and byte counts will vary.

---

## 1. Render a supplied PostScript program

**Prompt**

> Render this PostScript and show me the image (400×300 points):
> *(paste the contents of [`shapes.ps`](shapes.ps))*

**Expected tool call**

```json
{
  "name": "render_postscript",
  "arguments": {
    "source": "%!PS-Adobe-3.0\n%%Title: shapes.ps ...\nshowpage\n",
    "width": 400,
    "height": 300
  }
}
```

The agent passes the program verbatim (no rewriting) and picks up the page
size from the prompt or the `%%BoundingBox: 0 0 400 300` comment. `format`
may be omitted (PNG is the default).

**Expected result**

- `isError` absent/false.
- `content[0]`: `{"type": "image", "mimeType": "image/png", "data": "<base64>"}`
  — a 400×300 PNG: a red circle, a green rounded square with a blue ring, a
  gold star, and the caption "PSCat shapes".
- `content[1]`: text such as `Rendered 1 page as PNG (400x300 pt = 400x300 px, … bytes) with PSCat 0.7.0-rc.4 in … ms.`
- `structuredContent`: `{"ok": true, "format": "png", "mime_type": "image/png", "page_count": 1, "artifacts": [{"page": 1, "width_px": 400, "height_px": 300, "width_pt": 400, "height_pt": 300, "bytes": …, "mime_type": "image/png"}], …}`
- The agent displays the image or describes it accurately.

Variant: "…as a PDF" → `"format": "pdf"`; one `resource` block with
`mimeType: "application/pdf"` whose `blob` decodes to bytes starting `%PDF-`;
`artifacts[0]` has no `page` field (one document holds every page).

Variant: "…screened like a mono laser printer" → `"halftone": true`; one
`image` block, same size as the normal render, whose pixels are only pure
black and white (a 45° clustered-dot screen; solid black stays black, grey
becomes dots of about the same ink coverage). It is done in the worker, so
`structuredContent` is the usual success envelope with `format: "png"`.
With `"format": "svg"` or `"pdf"` the call is refused before anything runs:
`isError: true`, `error.code == "invalid_request"`, message
`halftone applies to png output only; format svg keeps its vector art`. The
agent retries without `halftone`, or with PNG, and says so.

---

## 2. Produce a diagram from PostScript source

**Prompt**

> Draw a flowchart of how this server handles a render request: the agent
> sends PostScript, the request is validated, a "Within limits?" decision
> either returns a structured error or renders in a worker, and the result
> comes back as PNG/PDF/SVG. Use labelled boxes and arrows, Helvetica text.

**Expected tool call(s)**

The agent writes the PostScript itself — something like
[`flowchart.ps`](flowchart.ps): `/Helvetica findfont 12 scalefont setfont`,
helper procedures for boxes, a diamond and arrows, `stringwidth` to centre
labels, and a final `showpage` — then calls:

```json
{
  "name": "render_postscript",
  "arguments": { "source": "<the generated program>", "width": 360, "height": 420 }
}
```

A careful agent may first call `validate_postscript` with the same `source`
and fix any reported error before rendering.

**Expected result**

- A PNG `image` block, 360×420 (or whatever size the agent chose), showing
  five nodes in a column, a side "Structured error" box on the "no" branch,
  arrows between them, and legible labels.
- If the first attempt raises a PostScript error (for example an operator
  PSCat does not implement, such as `arcto`), the result is
  `isError: true`, `structuredContent.error.code == "postscript_error"`, and
  `error.postscript = {"name": "undefined", "report": "%%[ Error: undefined; OffendingCommand: arcto; Line: N ]%%"}`.
  The agent should read the offending command and line, rewrite that part
  (e.g. with `arc`), and render again without asking the user.

---

## 3. Diagnose an invalid program with `validate_postscript`

**Prompt**

> Why doesn't this PostScript work?
>
> ```postscript
> %!PS
> /Helvetica findfont 24 scalefont setfont
> 72 700 moveto
> (Hello) shw
> 0 0 1 setrgbcolor
> 72 600 200 50 rectfill
> showpage
> ```

**Expected tool call**

```json
{ "name": "validate_postscript", "arguments": { "source": "%!PS\n/Helvetica findfont 24 scalefont setfont\n72 700 moveto\n(Hello) shw\n0 0 1 setrgbcolor\n72 600 200 50 rectfill\nshowpage\n" } }
```

**Expected result**

- `isError` false — validation *ran*; the program is what is invalid.
- `structuredContent`:
  ```json
  {
    "ok": true,
    "valid": false,
    "syntax": { "ok": true, "tokens": 20 },
    "execution_error": {
      "name": "undefined",
      "limit": null,
      "report": "%%[ Error: undefined; OffendingCommand: shw; Line: 4 ]%%"
    },
    "lint": [
      { "check": "blank-page", "message": "page 1 of 1 has no ink — nothing was painted since the previous page ..." },
      { "check": "stack-leak", "message": "1 item(s) left on the operand stack at the end of the program: (Hello) (topmost first)" }
    ],
    "page_count": 1,
    "program_output": "",
    "steps_used": 0,
    "renderer": { … }
  }
  ```
  (`steps_used` is 0 because PSCat's step counter does not count top-level
  tokens, only objects executed inside procedures and loops; validating
  `shapes.ps` reports 2670, the same as rendering it.)
  The text block reads `Not valid. PostScript error: %%[ Error: undefined;
  OffendingCommand: shw; Line: 4 ]%% …` followed by the lint lines.
- The agent explains that `shw` on line 4 is undefined (the lint notes are
  consequences: the error stopped the program before anything was painted,
  leaving the string on the stack), suggests `show`, and may re-validate the
  corrected program: `{"ok": true, "valid": true, "lint": [], "page_count": 1, …}`,
  text `Valid: the program ran without error and produced 1 page(s).`
  The agent eval checks the `validate_postscript` call and that the answer
  names `shw` and `show`; a short answer such as "`shw` should be `show`"
  passes.

Variant (syntax error): `%!PS\n72 72 moveto 100 100 lineto stroke\n{ 1 2 add\nshowpage\n`
yields `syntax: {"ok": false, "tokens": 12, "error": "unterminated procedure: 1 '{' without a matching '}'"}`
and `execution_error.name == "syntaxerror"` (report
`%%[ Error: syntaxerror (unterminated procedure: missing '}'); OffendingCommand: stroke; Line: 5 ]%%`).

Note: `%` starts a comment that runs to the end of the line, so a one-line
program such as `%!PS 0 0 moveto ... showpage` is entirely a comment and
renders a blank page. Put a newline after `%!PS`.

---

## 4. Request an unsupported output format

**Prompt**

> Render `0 0 1 setrgbcolor 0 0 100 100 rectfill showpage` as a JPEG.

**Expected tool call**

```json
{
  "name": "render_postscript",
  "arguments": { "source": "%!PS\n0 0 1 setrgbcolor 0 0 100 100 rectfill showpage", "format": "jpeg" }
}
```

(The tool schema lists `png`, `svg`, `pdf`; an agent that reads it may
instead explain JPEG is unavailable and offer PNG. Either is acceptable. If
it does call with `jpeg`:)

**Expected result**

- `isError: true`, nothing executed (rejected before a worker starts).
- `structuredContent`:
  ```json
  {
    "ok": false,
    "error": {
      "code": "unsupported_format",
      "message": "format \"jpeg\" is not supported; PSCat renders png, svg or pdf"
    },
    "renderer": { … }
  }
  ```
- The agent tells the user and offers PNG (or renders PNG if the user
  agrees).

---

## 5. Attempt excessive resource consumption

Each of these must fail fast with a structured error and leave the server
healthy (the next normal render succeeds; `/healthz` still returns 200 over
HTTP).

### 5a. Infinite loop

**Prompt**

> Render this: `{ } loop showpage`

**Expected tool call**

```json
{ "name": "render_postscript", "arguments": { "source": "%!PS\n{ } loop showpage" } }
```

**Expected result**

- `isError: true` within the configured timeout (default 20 s; PSCat's own
  time limit fires at 90 % of it, and its 5,000,000,000-step limit may fire
  first).
- `structuredContent.error.code` is `limit_exceeded` (PSCat stopped the
  program; `error.postscript.limit` is `"time"` or `"steps"`) or `timeout`
  (the worker process was killed at the wall-clock deadline). Observed with
  the defaults in the container (1 CPU): after ~11 s,
  `{"code": "limit_exceeded", "message": "the program was stopped at the steps limit", "postscript": {"name": "limitexceeded", "limit": "steps", "report": "%%[ Error: limitexceeded (steps); OffendingCommand: loop; Line: 2 ]%%"}}`.
  With `PSCAT_MCP_RENDER_TIMEOUT_MS=3000` it stops after ~3 s at the time
  limit.
- The agent reports that the program never terminates and does not retry
  it unchanged.

### 5b. Oversized page

**Prompt**

> Render `0 0 moveto 10000 10000 lineto stroke showpage` on a
> 10000×10000 point page.

**Expected tool call**

```json
{
  "name": "render_postscript",
  "arguments": { "source": "%!PS\n0 0 moveto 10000 10000 lineto stroke showpage", "width": 10000, "height": 10000 }
}
```

**Expected result**

- `isError: true`, rejected before execution.
- `structuredContent.error.code == "invalid_dimensions"` — 10000 px per side
  exceeds PSCat's 8000 px ceiling — with a message like
  `page is 10000x10000 pixels at 72 dpi; PSCat allows at most 8000 pixels a side`.
- A page within the per-side ceiling but over the pixel budget returns
  `too_many_pixels` instead: `width: 5000, height: 5000` →
  `page is 5000x5000 = 25000000 pixels; the limit is 16000000 pixels (lower the size or dpi)`.
  So does the default Letter page at `dpi: 600` (5100×6600 = 33,660,000 px).
- An agent that reads the `dpi` range in the schema may instead lower
  `dpi` until the page fits and render it. Claude Sonnet 5.5 used 9 dpi for
  10000×10000 (a 1250×1250 PNG) and 36 dpi for 5000×5000, and explained why.
  That is also a pass.
- A program that asks for a huge page itself
  (`<< /PageSize [10000 10000] >> setpagedevice`) is refused inside PSCat
  without allocating it: `postscript_error` with
  `error.postscript.name == "configurationerror"` (offending command
  `setpagedevice`).

### 5c. Huge source

**Prompt**

> Render this 1 MB PostScript file. *(attach or paste a program over
> 256 KiB, e.g. 30,000 lines of `0 0 moveto 1 1 lineto stroke`)*

**Expected tool call**

```json
{ "name": "render_postscript", "arguments": { "source": "<1 MB of PostScript>" } }
```

**Expected result**

- `isError: true`, rejected before execution.
- `structuredContent.error.code == "source_too_large"`, message
  `source is 1048558 bytes; the limit is 262144 bytes` (the actual size).
- Over HTTP, a request body beyond about six times the source limit
  (~1.5 MiB with the defaults) is refused by the transport with
  `HTTP 413 length limit exceeded` before reaching the tool (observed with a
  2.3 MB source); MCP clients surface that as a transport error rather than
  a tool result. Either is a pass as long as no render runs.
- The agent suggests simplifying the program (procedures and loops instead
  of unrolled drawing commands) rather than retrying.
- In practice an agent rarely sends a 285 KB argument at all. In the #44
  baseline, Claude rendered a 10-line shortened copy and said plainly that
  it had. The agent eval accepts that too, as long as the answer says the
  copy is shortened; it fails a shortened render passed off as the file.

---

## 6. Find real names before using a palette or template

**Prompt**

> Make a greeting card with the PSCat page templates. Check which templates
> and palettes this server has first, rather than guessing names.

**Expected tool calls**

```json
{ "name": "describe_art_capabilities", "arguments": {} }
{ "name": "describe_art_capabilities", "arguments": { "kind": "template" } }
{ "name": "describe_art_capabilities", "arguments": { "names": ["pgcard"] } }
```

then `render_postscript` with a `source` that starts with the `load` line of
the template it chose (for `pgcard`: `(lib/artkit.ps) run (lib/pagekit.ps) run`)
and passes a palette name from the catalog.

**Expected result**

- The first call returns `ok: true`, `view: "index"` with the kinds
  `font`, `type3_face`, `palette`, `template`, `procedure`, `operator` and
  `dial`, each with its names (templates `pgcard`, `pgcertificate`,
  `pginvitation`, `pgletter`, `pgposter`; palettes such as `acid`, `brass`).
  The reply is about 5 KB.
- `kind: "template"` lists the five with a one-line description each.
- `names: ["pgcard"]` returns the full entry: its dict keys under
  `parameters` and the `run` lines it needs under `load`. A made-up name in
  `names` comes back under `not_found`, and the call still succeeds.
- No entry or text carries a server file path, and `font_packs.installed`
  has no `path` fields.
- The agent uses only names from the catalog, and the render returns a PNG
  with `ok: true`.

This case is covered by `tests/mcp_stdio.rs` (the three views and their size ceilings, no paths,
`pgcard` rendering from its `load` line); it has not been run through an
agent against the container image.

---

## 7. Write a formula on a chalkboard

**Prompt**

> Write Euler's identity on a chalkboard for me, in yellow chalk.

**Expected tool calls**

```json
{ "name": "chalkboard_math", "arguments": { "latex": "e^{i\\pi} + 1 = 0", "color": "yellow" } }
```

**Expected result**

- One PNG image of 1600×800 pixels (the default board, 800×400 points at
  144 dpi): a dark slate with hand-drawn yellow chalk strokes, eraser marks
  and faint ghosts of earlier formulas. `ok: true`, `format: "png"`,
  about 0.3 MB.
- `board: false` gives chalk alone on a transparent page; `format: "svg"`
  or `"pdf"` comes back inline as an embedded resource. A board's SVG is
  far over the default SVG text limit of 40 KiB, so it is a blob resource
  (a 400x200 board is about 5 MB), and the default board's (7 to 10 MB,
  depending on the formula) can be over the 8 MiB output cap
  (`output_too_large`, `error.input.code` `output_bytes`, with advice to
  use PNG); PNG, or PDF (about 2.6 MB), fits.
- A command outside the subset, for example `\nosuchcommand{x}`, fails
  with `postscript_error`, a message naming the command, and
  `error.input: { "code": "unsupported_command", "token": "\\nosuchcommand" }`.
  The agent rewrites the formula with supported commands instead of
  switching to PostScript.
- A bad board setting (`color: "red"`, `size: 0`) is `invalid_request`
  naming the argument, before anything runs.

This case is covered by `tests/chalkboard.rs` and
`evals/server/scenarios/62-chalkboard-math.json`; the agent case is
`evals/agent/cases/70-chalkboard-math.json`. It has not been run through an
agent against the container image.

---

## 8. Write a handwritten note

**Prompt**

> Write "see you at the lake on saturday!" as a handwritten note on ruled
> paper, in green ink.

**Expected tool calls**

```json
{ "name": "handwrite", "arguments": { "text": "see you at the lake on saturday!", "paper": "ruled", "ink": "0.1,0.5,0.2" } }
```

**Expected result**

- One PNG image: a line of hand-drawn green letters on pale blue ruled
  lines with a red margin rule, 612×146 pixels (612 points wide at 72 dpi;
  the height is fitted to the wrapped text, 146 points for one line and 54
  for each further one). `ok: true`, `format: "png"`, a few KB. The same
  arguments give the same bytes.
- The face has `a` to `z` (capitals come out as lowercase), digits and
  `. , - ' ! ?`; other characters leave a gap. A text with none of them,
  an empty text and a text of only whitespace are `invalid_request`.
- A text that lays out taller than the limits is refused before it is drawn:
  `too_many_pixels` past `PSCAT_MCP_MAX_PIXELS`, `invalid_dimensions` past
  8000 pixels (or points) a side (about 148 lines at the defaults), with
  `error.input.code` `"height"` and a message with the line count and the
  size it would have been. The agent shortens the text, widens the note
  or lowers `size`.
- A text over `PSCAT_MCP_MAX_HANDWRITE_BYTES` is `source_too_large`; a bad
  `paper`, `ink`, `size`, `jitter` or `seed` is `invalid_request`; `dpi` or
  `width` out of range is `invalid_dimensions`.
- The styles are hand-drawn faces for notes, cards and art, not for
  imitating a specific person's handwriting.

This case is covered by `tests/handwrite.rs` and
`evals/server/scenarios/63-handwrite.json`; the agent case is
`evals/agent/cases/80-handwrite.json` (unbaselined). It has not been run
through an agent against the container image.

---

## 9. Compare candidates on a review sheet

**Prompt**

> I have two logo ideas for a 64x64 point icon: a red ring, and a blue
> square with a white square hole in it. Show them side by side on one
> review sheet at 128 pixels and at 32 pixels, with the 32 pixel version
> also blown up 4x so I can see the pixels. Which one reads better small?

**Expected tool calls**

```json
{ "name": "review_sheet", "arguments": {
  "entries": [
    { "source": "0.85 0.2 0.2 setrgbcolor 32 32 26 0 360 arc fill 1 setgray 32 32 12 0 360 arc fill showpage", "label": "ring" },
    { "source": "0.2 0.3 0.8 setrgbcolor 8 8 48 48 rectfill 1 setgray 20 20 24 24 rectfill showpage", "label": "square" }
  ],
  "width": 64, "height": 64,
  "dpi": [144, 36, { "dpi": 36, "zoom": 4 }]
} }
```

**Expected result**

- One PNG image of 363×396 pixels: two rows (`ring`, `square`) of three
  cells each, a 128×128 render, a 32×32 render and the same 32×32 render
  enlarged 4x with hard pixel edges, each with a grey hairline and a
  `128x128 px` / `32x32 px` / `32x32 px (shown at 4x)` caption. `ok: true`,
  `failed_cells: 0`, and `cells[]` gives each cell's size and place
  (`cells[0]` at x 13, y 39). For the same programs, dpis and labels the
  bytes are those `pscat sheet` writes. The agent compares the small renders
  and answers.
- A cell whose program fails is drawn as far as it got: the sheet still
  comes back (`ok: true`), with `failed_cells: 1` and that cell's
  `error: {code: "postscript_error", message: "PostScript error: %%[ Error:
  undefined; OffendingCommand: ..."}`. A runaway cell stops at its own
  limit (`limit_exceeded`, `limit: "steps"` or `"time"`, its share of the
  sheet's time) and the cells after it render.
- A sheet over a budget is refused before anything runs, with
  `error.input.code` naming the budget: more than 32 cells
  (`invalid_request`, `cells`), sources over 256 KiB in all
  (`source_too_large`, `sources`), a cell over 8000 pixels a side
  (`invalid_dimensions`, `cell_side`), cells over 16,000,000 pixels in all
  (`too_many_pixels`, `cell_pixels`), a sheet over 16,000,000 pixels or 8000
  a side (`sheet_pixels`, `sheet_side`). The agent lowers a dpi, a page
  size, a zoom or the number of cells.
- A dpi outside 9–1200 is `invalid_dimensions`; a zoom outside 2–16, a label
  over 200 characters or a dpi that is not a list is `invalid_request`.

This case is covered by `tests/review_sheet.rs` and
`evals/server/scenarios/64-review-sheet.json`; the agent case is
`evals/agent/cases/90-review-sheet.json` (unbaselined). It has not been run
through an agent against the container image.

## 10. Start from a guide prompt

**Prompt**

In Claude Code, pick the server's `chart` prompt with one argument, then
type the data:

> `/mcp__pscat-render__chart bar`
>
> The data: quarterly sign-ups, Q1 8, Q2 15, Q3 12, Q4 19. Label the four
> bars Q1 to Q4 and give the chart a title. Show me the rendered image.

`prompts/get` (`{"name": "chart", "arguments": {"kind": "bar"}}`) returns one
user message. It starts with the task ("Make a paper-ready bar chart with
PSCat's lib/dataviz.ps, and render it with this server's render_postscript
tool..."). Then comes `pscat://guide/psviz/SKILL.md` as `resources/read`
serves it: the server's note, a rule, and PSCat's `psviz` skill unchanged.

**Expected tool calls**

```json
{ "name": "render_postscript", "arguments": { "source": "%!PS\n(lib/dataviz.ps) run\n... [8 15 12 19] 0.22 { pop dvcolor } barchart ...\nshowpage" } }
```

**Expected result**

- A PNG bar chart with four bars and the labels; `ok: true`.
- The agent works from `dataviz.ps` calls in the guide (`setdvframe`,
  `barchart`, `dvaxes`), not from the `pscat --png` command line, which the
  note says this server doesn't have.
- `resources/read` of an unknown `pscat://guide/...` URI is JSON-RPC error
  `-32002` (`-32602` at protocol 2026-07-28); `prompts/get` of an unknown prompt, or `kind: "radar"`, is
  `-32602`.

This case is covered by `tests/guide.rs`, `tests/http.rs` and
`evals/server/scenarios/65-guide-prompts-resources.json`; the agent case is
`evals/agent/cases/91-guide-prompt-chart.json` (unbaselined). It has not
been run through an agent.

---

## 11. Run a long render as a background job

Async jobs are on over stdio; over HTTP only with `PSCAT_MCP_ASYNC_JOBS=true`
(one instance). Without them the four job tools are not listed.

**Prompt**

> Run this PostScript program as a background render job instead of a
> single call (it takes a few seconds on purpose), wait until it has
> finished, fetch its result, and tell me what came back:
>
> ```postscript
> %!PS
> realtime /t0 exch def { realtime t0 sub 3000 gt { exit } if } loop
> 0 0.6 0 setrgbcolor 0 0 200 100 rectfill
> showpage
> ```
>
> Use a 200 x 100 point page.

**Expected tool calls**

```json
{ "name": "start_render", "arguments": { "source": "%!PS\nrealtime ...", "width": 200, "height": 100 } }
{ "name": "render_status", "arguments": { "job_id": "<the job_id>" } }
{ "name": "render_result", "arguments": { "job_id": "<the job_id>" } }
```

**Expected result**

- `start_render` answers at once: `ok: true`, `job_id` (a UUID),
  `state: "queued"`, `timeout_seconds: 600`, and a `next` hint.
- `render_status` goes `queued` → `running` → `done` (about 3 s), with
  `elapsed_seconds`; once finished, `expires_in_seconds` (an hour).
- `render_result` returns exactly what `render_postscript` would: one PNG,
  200×100 pixels, all green, `ok: true`, `format: "png"`. Called while the
  job runs, it is `job_not_finished`.
- `cancel_render` on a running job kills its worker and answers
  `state: "cancelled"`; `render_result` is then `cancelled`.
- A `job_id` that is unknown, expired, or another client's (another bearer
  token over HTTP) is `unknown_job`. Over the per-client cap (4 unfinished
  jobs) or the server's (16), `start_render` is `server_busy`; when results
  waiting to be fetched fill the memory budget, `results_budget_exceeded`.

This case is covered by `tests/jobs.rs` and
`evals/server/scenarios/66-async-job.json`; the agent case is
`evals/agent/cases/95-async-job.json` (stdio only, unbaselined). It has not
been run through an agent.

---

## Checklist

| # | Case | Tool | Pass condition |
| --- | --- | --- | --- |
| 1 | Supplied program | `render_postscript` | PNG 400×300, `ok: true` |
| 1b | Halftone variant | `render_postscript` with `halftone: true` | PNG of the same size, only black and white pixels; with `svg`/`pdf`, `invalid_request` |
| 2 | Diagram from prose | `render_postscript` (± `validate_postscript`) | Legible labelled flowchart; self-corrects on `postscript_error` |
| 3 | Invalid program | `validate_postscript` | `valid: false`, `execution_error.name == "undefined"`, line 4, agent suggests `show` |
| 4 | JPEG | `render_postscript` | `unsupported_format` (or agent declines before calling) |
| 5a | Infinite loop | `render_postscript` | `limit_exceeded` or `timeout`; server still healthy |
| 5b | 10000×10000 page | `render_postscript` | `invalid_dimensions` (5000×5000: `too_many_pixels`) |
| 5c | Huge source | `render_postscript` | `source_too_large` (or HTTP 413) |
| 6 | Real names | `describe_art_capabilities` then `render_postscript` | Catalog lists templates and palettes; render uses a listed template and its `load` line |
| 7 | Chalkboard | `chalkboard_math` | PNG 1600×800 board with yellow chalk; an unsupported command is `postscript_error` naming it |
| 8 | Handwritten note | `handwrite` | PNG 612×146 on ruled paper in green ink; a note too tall for the limits is `invalid_dimensions` or `too_many_pixels` |
| 9 | Review sheet | `review_sheet` | PNG 363×396, two rows of three cells (128 px, 32 px, 32 px at 4x); a failing cell drawn with its error; budgets refused before execution |
| 10 | Guide prompt | `prompts/get` `chart`, then `render_postscript` | Bar chart from `lib/dataviz.ps` renders; unknown URI `-32002`, unknown prompt `-32602` |
| 11 | Background job | `start_render`, `render_status`, `render_result` | job `queued` then `done`; PNG 200×100 green from `render_result`; `job_not_finished` before, `unknown_job` for another client |
