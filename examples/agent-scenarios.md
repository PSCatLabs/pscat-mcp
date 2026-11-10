# Agent acceptance scenarios

Manual acceptance cases for an MCP client (Claude Code, Claude Desktop, an
agent SDK, or MCP Inspector) connected to `pscat-mcp-server`, over stdio or
Streamable HTTP. Each case gives a prompt to type, the tool call the agent is
expected to make, and the result that counts as a pass.

The server exposes three tools:

| Tool | Arguments | Returns |
| --- | --- | --- |
| `render_postscript` | `source` (required), `format` (`png` default, `svg`, `pdf`), `width`, `height` (points), `dpi` (9–1200, default 72) | One content block per artifact (PNG `image`, SVG text resource, PDF blob resource) + a text summary; `structuredContent` envelope `{ok, format, mime_type, page_count, artifacts[], program_output, steps_used, elapsed_ms, renderer}` |
| `validate_postscript` | `source` | Text summary; `structuredContent` `{ok, valid, syntax{ok,tokens,error?}, execution_error?{name,limit,report}, lint[], page_count, program_output, steps_used, renderer}` |
| `get_renderer_info` | none | PSCat version, formats, limits in force |

Every failure is `isError: true` with a text explanation and
`structuredContent.error = {code, message, postscript?, program_output?}`.
`code` is one of: `invalid_request`, `source_too_large`, `unsupported_format`,
`invalid_dimensions`, `too_many_pixels`, `postscript_error`, `limit_exceeded`,
`timeout`, `too_many_pages`, `output_too_large`, `server_busy`, `cancelled`,
`worker_failed`.

Values below assume the default limits (256 KiB source, 20 s timeout,
16,000,000 pixels, 8000 px per side, 10 pages, 8 MiB output). Every result
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
- `content[1]`: text such as `Rendered 1 page as PNG (400x300 px, … bytes) with PSCat 0.7.0-rc.4 in … ms.`
- `structuredContent`: `{"ok": true, "format": "png", "mime_type": "image/png", "page_count": 1, "artifacts": [{"page": 1, "width_px": 400, "height_px": 300, "bytes": …, "mime_type": "image/png"}], …}`
- The agent displays the image or describes it accurately.

Variant: "…as a PDF" → `"format": "pdf"`; one `resource` block with
`mimeType: "application/pdf"` whose `blob` decodes to bytes starting `%PDF-`;
`artifacts[0]` has no `page` field (one document holds every page).

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

---

## Checklist

| # | Case | Tool | Pass condition |
| --- | --- | --- | --- |
| 1 | Supplied program | `render_postscript` | PNG 400×300, `ok: true` |
| 2 | Diagram from prose | `render_postscript` (± `validate_postscript`) | Legible labelled flowchart; self-corrects on `postscript_error` |
| 3 | Invalid program | `validate_postscript` | `valid: false`, `execution_error.name == "undefined"`, line 4, agent suggests `show` |
| 4 | JPEG | `render_postscript` | `unsupported_format` (or agent declines before calling) |
| 5a | Infinite loop | `render_postscript` | `limit_exceeded` or `timeout`; server still healthy |
| 5b | 10000×10000 page | `render_postscript` | `invalid_dimensions` (5000×5000: `too_many_pixels`) |
| 5c | Huge source | `render_postscript` | `source_too_large` (or HTTP 413) |
