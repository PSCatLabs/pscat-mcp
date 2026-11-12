//! MCP tool definitions (rmcp). Thin: every tool delegates to
//! [`RenderService`](crate::render::RenderService) and only translates
//! arguments in and results out.
//!
//! Tool result shapes:
//!
//! * `render_postscript` — on success, one content block per artifact (PNG
//!   as `image` content; SVG as an embedded text resource, or a blob
//!   resource when all pages together are over `PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES`;
//!   PDF as an embedded blob resource) plus a short text summary. `structuredContent`
//!   is a [`RenderToolOutput`] envelope without the artifact bytes. On
//!   failure, `isError: true`, a text explanation, and the same envelope
//!   with `ok: false` and `error` set; never an image.
//! * `chalkboard_math` — the same content blocks and [`RenderToolOutput`]
//!   envelope as `render_postscript`, for the one board it draws.
//! * `handwrite` — the same content blocks and [`RenderToolOutput`] envelope,
//!   for the one PNG note it draws.
//! * `review_sheet` — one PNG `image` block (the sheet) and a text summary
//!   with a line per cell. `structuredContent` is a [`SheetToolOutput`]
//!   envelope: the sheet's size and every cell's status, size and place on
//!   the sheet, with no image bytes. A failed cell does not fail the call.
//!   On failure, `isError: true`, a text explanation and the envelope with
//!   `ok: false` and `error`.
//! * `validate_postscript` — a [`ValidateToolOutput`] envelope (the
//!   [`ValidationReport`] fields at the top level) plus a text summary.
//! * `describe_art_capabilities` — a [`CapabilitiesToolOutput`] envelope (the
//!   [`CatalogSlice`] fields at the top level) plus the same slice as text.
//! * `get_renderer_info` — a [`RendererInfo`] object.
//! * `start_render`, `render_status`, `cancel_render` — a [`JobToolOutput`]
//!   envelope (the [`JobStatus`] fields at the top level) plus the same as
//!   JSON text. `render_result` — exactly what `render_postscript` returns
//!   for the job's render, or a [`RenderToolOutput`] failure envelope
//!   (`unknown_job`, `job_not_finished`, `cancelled`).
//!
//! The four job tools are listed only when async jobs are on (always over
//! stdio, `--async-jobs` over HTTP). With `--tasks` too, a `tools/call` of
//! `render_postscript`, `chalkboard_math` or `handwrite` from a client that
//! declares the MCP Tasks extension runs synchronously at first; one still
//! running after `--task-after-ms` becomes a job, and the client gets a task
//! handle (`CreateTaskResult`) instead of the result, served by `tasks/get`
//! and `tasks/cancel` from the same job store (see [`crate::jobs`] and
//! [`RenderService::call_or_task`]).
//!
//! Prompts and resources (the PSCat plugin's skills) come from
//! [`crate::guide`]; their handlers below only forward to it.

use std::sync::Arc;

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock,
    CreateTaskResult, DetailedTask, Extensions, GetPromptRequestParams, GetPromptResponse,
    GetTaskParams, GetTaskResult, Implementation, JsonObject, ListPromptsResult,
    ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ResourceContents, ServerCapabilities, ServerConfig, TASKS_EXTENSION_ID, Task, TaskPayload,
    TaskStatus, UpdateTaskParams,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::build_info::{RendererIdentity, SERVER_VERSION};
use crate::catalog::{CatalogQuery, CatalogSlice};
use crate::config::{MAX_DPI, MIN_DPI, PSCAT_MAX_SIDE_PX};
use crate::generate::{ChalkboardParams, GenerateJob};
use crate::guide::{Guide, PackFaces, ServerFacts};
use crate::handwrite::HandwriteParams;
use crate::jobs::{JobResult, JobState, JobStatus, LOCAL_PRINCIPAL};
use crate::render::{
    ArtCatalog, DEFAULT_PAGE_POINTS, ErrorCode, GenerateRequest, JobRequest, RenderError,
    RenderOutput, RenderRequest, RenderService, TaskCall, ValidationReport, cancelled,
};
use crate::sheet::{
    MAX_LABEL_CHARS, MAX_PAGE_POINTS, MAX_ZOOM, SheetCellReport, SheetEntryParams, SheetParams,
};
use crate::worker::OutputFormat;

/// Arguments of `render_postscript`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RenderArgs {
    /// PostScript program text. The program draws on the page and ends each
    /// page with `showpage`; a program that never calls `showpage` yields
    /// its current page.
    pub source: String,
    /// Output format: "png" (one image per page, the default), "svg" (one
    /// SVG document per page) or "pdf" (one PDF document holding every
    /// page).
    #[schemars(extend("enum" = ["png", "svg", "pdf"], "default" = "png"))]
    pub format: Option<String>,
    /// Page width in PostScript points (1 pt = 1/72 inch). Defaults to 612
    /// (US Letter) when neither width nor height is given.
    pub width: Option<u32>,
    /// Page height in points. Defaults to 792 (US Letter).
    pub height: Option<u32>,
    /// Resolution in dots per inch, 9 to 1200. Default 72, i.e. one pixel
    /// per point. Device pixels = points * dpi / 72.
    pub dpi: Option<f64>,
    /// Screen the raster like a mono laser printer: a 45 degree clustered-dot
    /// screen of pure black and white pixels. PNG only; with "svg" or "pdf"
    /// the call fails with invalid_request. Default false.
    #[schemars(extend("default" = false))]
    pub halftone: Option<bool>,
}

impl RenderArgs {
    fn into_request(self) -> RenderRequest {
        RenderRequest {
            source: self.source,
            format: self.format,
            width: self.width,
            height: self.height,
            dpi: self.dpi,
            halftone: self.halftone.unwrap_or(false),
        }
    }
}

/// Arguments of `render_status`, `render_result` and `cancel_render`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JobIdArgs {
    /// The job_id start_render returned.
    pub job_id: String,
}

/// `structuredContent` of `start_render`, `render_status` and
/// `cancel_render`. When the call worked, `ok` is true and the job's
/// [`JobStatus`] fields are present (a failed job is still `ok`: its error
/// is `job_error`). Otherwise `ok` is false and `error` says why.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct JobToolOutput {
    /// True when the call worked (whatever the job's state).
    pub ok: bool,
    /// The job.
    #[serde(flatten)]
    pub job: Option<JobStatus>,
    /// What to call next, for `start_render`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Why the call failed, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RenderError>,
    /// The renderer that handled the request.
    pub renderer: RendererIdentity,
}

/// Arguments of `chalkboard_math`. Defaults, ranges and the way the board
/// arguments combine are PSCat's (`pscat chalk`, the plugin's tool).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ChalkboardArgs {
    /// The formula, as typed in a LaTeX document (single backslashes).
    pub latex: String,
    /// Writing size in points, before any shrink to fit the board. Default 64.
    #[schemars(extend("exclusiveMinimum" = 0, "maximum" = 1000))]
    pub size: Option<f64>,
    /// How far the layout departs from clean TeX: 0 clean and level, 1 a
    /// steady hand (default), up to 5.
    #[schemars(range(min = 0, max = 5))]
    pub irregular: Option<f64>,
    /// Baseline tilt in degrees. Default: a small one picked from the seed.
    #[schemars(range(min = -45, max = 45))]
    pub slope: Option<f64>,
    /// Each glyph's wobble as a fraction of the size. Default 0.016.
    #[schemars(range(min = 0, max = 0.5))]
    pub jitter: Option<f64>,
    /// Where the lines sit across the board. Default "center".
    #[schemars(extend("enum" = ["center", "left"]))]
    pub align: Option<String>,
    /// Chalk colour. Default "white".
    #[schemars(extend("enum" = ["white", "yellow", "pink", "blue", "green", "orange"]))]
    pub color: Option<String>,
    /// The board: true (a slate, the default), false (chalk alone on a
    /// transparent page), "slate", "greenboard", [r, g, b] in 0..1, or an
    /// object of board keys (tone, haze, ghosting, swipes, wipes, smudges,
    /// ledge).
    #[serde(default)]
    #[schemars(schema_with = "board_schema")]
    pub board: Option<Value>,
    /// How much of an earlier lecture's erased writing shows, 0 to 1
    /// (default 0.4); false for none. Needs a board.
    #[serde(default)]
    #[schemars(schema_with = "ghosting_schema")]
    pub ghosting: Option<Value>,
    /// Seed for the layout, the chalk and the board; the same seed gives the
    /// same image. Default 1.
    #[schemars(range(min = -1_000_000_000, max = 1_000_000_000))]
    pub seed: Option<i64>,
    /// Board width in points. Default 800.
    #[schemars(range(min = 1, max = 8000))]
    pub width: Option<u32>,
    /// Board height in points. Default 400.
    #[schemars(range(min = 1, max = 8000))]
    pub height: Option<u32>,
    /// Resolution in dots per inch, 9 to 1200. Default 144 (the default
    /// board is 800x400 points, 1600x800 pixels). The result reports the
    /// board in points and in pixels.
    pub dpi: Option<f64>,
    /// Output format: "png" (the default), "svg" or "pdf".
    #[schemars(extend("enum" = ["png", "svg", "pdf"], "default" = "png"))]
    pub format: Option<String>,
    /// The full board config, as `pscat chalk --json` takes it: groups
    /// `layout` (size, irregular, slope, jitter, align, margin, width, height,
    /// dpi), `chalk` (color, strength, pressure, tilt, dust, width), `board`
    /// (or false) and `seed`. The arguments above are laid over it.
    pub config: Option<serde_json::Map<String, Value>>,
}

fn board_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "description": "The board: true (a slate, the default), false (chalk alone on a \
                        transparent page), \"slate\", \"greenboard\", [r, g, b] in 0..1, or an \
                        object of board keys (tone, haze, ghosting, swipes, wipes, smudges, ledge).",
        "anyOf": [
            { "type": "boolean" },
            { "type": "string", "enum": ["slate", "greenboard"] },
            {
                "type": "array",
                "items": { "type": "number", "minimum": 0, "maximum": 1 },
                "minItems": 3,
                "maxItems": 3
            },
            { "type": "object" }
        ]
    })
}

fn ghosting_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "description": "How much of an earlier lecture's erased writing shows, 0 to 1 \
                        (default 0.4); false for none. Needs a board.",
        "anyOf": [
            { "type": "number", "minimum": 0, "maximum": 1 },
            { "type": "boolean" }
        ]
    })
}

impl ChalkboardArgs {
    /// The generate request: the LaTeX, the format, and every board
    /// argument that was given, as the JSON PSCat's config reader takes.
    fn into_request(self) -> GenerateRequest {
        let mut board = serde_json::Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(v) = value.filter(|v| !v.is_null()) {
                board.insert(key.to_string(), v);
            }
        };
        put("size", self.size.map(Value::from));
        put("irregular", self.irregular.map(Value::from));
        put("slope", self.slope.map(Value::from));
        put("jitter", self.jitter.map(Value::from));
        put("align", self.align.map(Value::from));
        put("color", self.color.map(Value::from));
        put("board", self.board);
        put("ghosting", self.ghosting);
        put("seed", self.seed.map(Value::from));
        put("width", self.width.map(Value::from));
        put("height", self.height.map(Value::from));
        put("dpi", self.dpi.map(Value::from));
        put("config", self.config.map(Value::Object));
        GenerateRequest {
            job: GenerateJob::ChalkboardMath(ChalkboardParams {
                latex: self.latex,
                board,
            }),
            format: self.format,
        }
    }
}

/// Arguments of `handwrite`. Names, defaults and ranges are those of the
/// PSCat plugin's tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HandwriteArgs {
    /// The note to write. Newlines force line breaks.
    pub text: String,
    /// Font size in points. Default 36.
    #[schemars(range(min = 0, max = 2000))]
    pub size: Option<f64>,
    /// Page width in points. Default 612.
    #[schemars(range(min = 1, max = 8000))]
    pub width: Option<u32>,
    /// Paper style: "plain" (the default) or "ruled".
    #[schemars(extend("enum" = ["plain", "ruled"], "default" = "plain"))]
    pub paper: Option<String>,
    /// Ink colour as "R,G,B", each from 0 to 1. Default "0.13,0.14,0.34".
    pub ink: Option<String>,
    /// Ink wobble in glyph units; 0 is calm. A whole number. Default 16.
    #[serde(default)]
    #[schemars(schema_with = "jitter_schema", transform = drop_default)]
    pub jitter: Option<f64>,
    /// Random seed; the same seed gives the same note. Default 1985.
    #[schemars(range(min = -2147483648_i64, max = 2147483647_i64))]
    pub seed: Option<i64>,
    /// Resolution in dots per inch, 9 to 1200. Default 72.
    pub dpi: Option<f64>,
}

/// `jitter` is a whole number: PSCat's handwriting font takes it with `mod`.
/// Optional, so the type admits null, as the other optional numbers' do.
fn jitter_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["integer", "null"], "minimum": 0, "maximum": 10000 })
}

/// Drops the `default` that `#[serde(default)]` adds, to match the other
/// optional arguments' schemas.
fn drop_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}

impl HandwriteArgs {
    /// The generate request: a PNG note.
    fn into_request(self) -> GenerateRequest {
        GenerateRequest {
            job: GenerateJob::Handwrite(HandwriteParams {
                text: self.text,
                size: self.size,
                width: self.width,
                paper: self.paper,
                ink: self.ink,
                jitter: self.jitter,
                seed: self.seed,
                dpi: self.dpi,
            }),
            format: None,
        }
    }
}

/// Arguments of `review_sheet`. Names, defaults and caps are those of the
/// PSCat plugin's tool, without `out_path`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReviewSheetArgs {
    /// The rows, top to bottom, one program each.
    #[schemars(schema_with = "sheet_entries_schema")]
    pub entries: Vec<SheetEntryParams>,
    /// Page width in points for entries that give none. Default 612.
    #[schemars(range(min = 1, max = 8000))]
    pub width: Option<u32>,
    /// Page height in points for entries that give none. Default 792.
    #[schemars(range(min = 1, max = 8000))]
    pub height: Option<u32>,
    /// One cell per item, left to right, for entries that give no list.
    #[serde(default)]
    #[schemars(schema_with = "sheet_dpi_default_schema")]
    pub dpi: Option<Value>,
    /// Draw the row labels. Default true.
    pub labels: Option<bool>,
    /// Draw the pixel-size caption under each cell. Default true.
    pub sizes: Option<bool>,
}

/// The schema of a dpi list, written out where it is used rather than as a
/// `$ref` (as the plugin does: some clients do not resolve references in a
/// tool's input schema).
fn sheet_dpi_schema(description: &str) -> Value {
    serde_json::json!({
        "type": "array",
        "minItems": 1,
        "description": description,
        "items": {
            "anyOf": [
                { "type": "number", "minimum": 9, "maximum": 1200 },
                {
                    "type": "object",
                    "properties": {
                        "dpi": { "type": "number", "minimum": 9, "maximum": 1200 },
                        "zoom": { "type": "integer", "minimum": 2, "maximum": MAX_ZOOM }
                    },
                    "required": ["dpi"]
                }
            ]
        }
    })
}

fn to_schema(v: Value) -> schemars::Schema {
    schemars::Schema::try_from(v).unwrap_or_default()
}

fn sheet_dpi_default_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    to_schema(sheet_dpi_schema(
        "One cell per item, left to right: a dpi (9 to 1200), or {\"dpi\": 9, \"zoom\": 4} \
         to draw that render enlarged 4x. For entries that give no list. Default [72].",
    ))
}

fn sheet_entries_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    to_schema(serde_json::json!({
        "type": "array",
        "minItems": 1,
        "description": "The rows, top to bottom, one program each.",
        "items": {
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "The PostScript program." },
                "label": {
                    "type": "string",
                    "maxLength": MAX_LABEL_CHARS,
                    "description": "The row label. Default \"program N\", N counting from 1."
                },
                "width": {
                    "type": "integer", "minimum": 1, "maximum": MAX_PAGE_POINTS,
                    "description": "Page width in points. Default: the top-level width."
                },
                "height": {
                    "type": "integer", "minimum": 1, "maximum": MAX_PAGE_POINTS,
                    "description": "Page height in points. Default: the top-level height."
                },
                "dpi": sheet_dpi_schema(
                    "The cells of this row, left to right. Default: the top-level dpi."
                )
            },
            "required": ["source"]
        }
    }))
}

impl ReviewSheetArgs {
    /// The generate request: a PNG sheet.
    fn into_request(self) -> GenerateRequest {
        GenerateRequest {
            job: GenerateJob::ReviewSheet(SheetParams {
                entries: self.entries,
                width: self.width,
                height: self.height,
                dpi: self.dpi,
                labels: self.labels,
                sizes: self.sizes,
            }),
            format: None,
        }
    }
}

/// Arguments of `validate_postscript`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ValidateArgs {
    /// PostScript program text.
    pub source: String,
}

/// Size and type of one rendered artifact (the bytes are in the content
/// blocks, not here).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtifactSummary {
    /// 1-based page number; absent for a PDF document holding every page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<usize>,
    /// Width in device pixels.
    pub width_px: u32,
    /// Height in device pixels.
    pub height_px: u32,
    /// Width in points (1/72 inch) of the page as drawn, after any default
    /// size and any `setpagedevice`: `width_px * 72 / dpi`, rounded to 0.01
    /// (a fractional page size comes back as its rounded pixels converted
    /// back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_pt: Option<f64>,
    /// Height in points of the page as drawn; see `width_pt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height_pt: Option<f64>,
    /// Resolution in dots per inch the page was rendered at: the scale
    /// between the two sizes, `px = pt * dpi / 72`. Absent with the points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<f64>,
    /// Size of the artifact in bytes (before base64).
    pub bytes: usize,
    /// MIME type of the artifact.
    pub mime_type: String,
    /// How the artifact's content block carries it: `image` (PNG),
    /// `text` (an embedded text resource: SVG up to `svg_blob_threshold_bytes`) or
    /// `blob` (an embedded base64 resource: PDF, and SVG over
    /// `svg_blob_threshold_bytes`).
    pub returned_as: ReturnedAs,
}

/// The kind of content block an artifact is returned in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReturnedAs {
    /// An MCP `image` content block.
    Image,
    /// An embedded text resource.
    Text,
    /// An embedded blob resource (base64).
    Blob,
}

/// `structuredContent` of `render_postscript`, for success and failure
/// alike. On success every field except `error` is present; on failure only
/// `ok`, `error` and `renderer` are.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenderToolOutput {
    /// True when the render succeeded.
    pub ok: bool,
    /// Format produced (`png`, `svg` or `pdf`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// MIME type of each artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Pages the program produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<usize>,
    /// One entry per returned artifact, in content order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<ArtifactSummary>>,
    /// Program stdout (`print`, `=`, `==`), possibly truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_output: Option<String>,
    /// Whether `program_output` was cut at the server's limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_output_truncated: Option<bool>,
    /// Interpreter steps used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_used: Option<u64>,
    /// Time spent executing the program, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// The failure, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RenderError>,
    /// The renderer that handled the request.
    pub renderer: RendererIdentity,
}

/// `structuredContent` of `review_sheet`, for success and failure alike. On
/// success every field except `error` is present; on failure only `ok`,
/// `error` and `renderer` are.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SheetToolOutput {
    /// True when the sheet was composed (some cells may still have failed:
    /// see `failed_cells`).
    pub ok: bool,
    /// Always `png`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Always `image/png`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The sheet's width in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_px: Option<u32>,
    /// The sheet's height in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height_px: Option<u32>,
    /// Size of the PNG in bytes (before base64).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    /// Cells whose program did not run to the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_cells: Option<usize>,
    /// Every cell, row by row, left to right: its dpi, zoom, render size,
    /// place on the sheet, and error if it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cells: Option<Vec<SheetCellReport>>,
    /// Interpreter steps, all cells together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_used: Option<u64>,
    /// Time the cells' programs ran, all together, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Time the whole sheet took, worker start and encoding included, in
    /// milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// The failure, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RenderError>,
    /// The renderer that handled the request.
    pub renderer: RendererIdentity,
}

/// `structuredContent` of `validate_postscript`. When validation ran, `ok`
/// is true and the [`ValidationReport`] fields are present (`valid` says
/// whether the program itself is sound). When the request could not be
/// validated (too large, timeout, busy), `ok` is false and `error` is set.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidateToolOutput {
    /// True when validation ran (regardless of whether the program is valid).
    pub ok: bool,
    /// The validation report.
    #[serde(flatten)]
    pub report: Option<ValidationReport>,
    /// Why validation could not run, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RenderError>,
    /// The renderer that handled the request.
    pub renderer: RendererIdentity,
}

/// Arguments of `describe_art_capabilities`. With none, the reply is an
/// index of every kind with its names.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CapabilitiesArgs {
    /// List one kind: its names with a one-line description each, a page at a
    /// time (see `offset` and `limit`).
    #[schemars(extend("enum" = ["font", "type3_face", "palette", "template", "procedure", "operator", "dial"]))]
    pub kind: Option<String>,
    /// Exact names (up to 20) to return in full: description, parameters,
    /// library file, `run` lines to load it, example and availability. With
    /// `kind`, only entries of that kind. Unknown names are listed under
    /// `not_found`; names that do not fit the reply under `omitted`.
    #[schemars(length(min = 1, max = 20))]
    pub names: Option<Vec<String>>,
    /// First entry of a `kind` listing, default 0.
    #[schemars(range(min = 0))]
    pub offset: Option<u32>,
    /// Entries in a `kind` listing, 1 to 100, default 100.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
}

/// `structuredContent` of `describe_art_capabilities`. When the catalog was
/// read, `ok` is true and the [`CatalogSlice`] fields are present. Otherwise
/// `ok` is false and `error` is set.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CapabilitiesToolOutput {
    /// True when the catalog was read.
    pub ok: bool,
    /// The part of the catalog that was asked for.
    #[serde(flatten)]
    pub catalog: Option<CatalogSlice>,
    /// Why the catalog could not be read, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RenderError>,
    /// The renderer that handled the request.
    pub renderer: RendererIdentity,
}

/// One output format.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormatInfo {
    /// Name accepted by `render_postscript`'s `format` argument.
    pub name: String,
    /// MIME type of each artifact.
    pub mime_type: String,
    /// How the artifacts are returned.
    pub returned_as: String,
}

/// Limits applied to every render and validation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LimitsInfo {
    /// Largest PostScript source accepted, in bytes.
    pub max_source_bytes: usize,
    /// Largest `chalkboard_math` LaTeX accepted, in bytes.
    pub max_latex_bytes: usize,
    /// Largest `handwrite` text accepted, in bytes.
    pub max_handwrite_bytes: usize,
    /// Most cells in one `review_sheet`.
    pub max_sheet_cells: usize,
    /// Most bytes of source in one `review_sheet`, all entries together.
    pub max_sheet_source_bytes: usize,
    /// Most pixels the cells of one `review_sheet` render, together.
    pub max_sheet_cell_pixels: u64,
    /// Most pixels of a composed `review_sheet`.
    pub max_sheet_pixels: u64,
    /// Wall-clock deadline for one whole `review_sheet`, in milliseconds.
    pub sheet_timeout_ms: u64,
    /// Wall-clock deadline for one render, in milliseconds.
    pub render_timeout_ms: u64,
    /// Most device pixels on one page (width x height).
    pub max_pixels: u64,
    /// Most device pixels on one side of a page.
    pub max_side_px: u32,
    /// Most pages one render may return.
    pub max_pages: usize,
    /// Most bytes of rendered output (all pages, before base64).
    pub max_output_bytes: usize,
    /// Most bytes of SVG (all pages together) returned as embedded text
    /// resources; at most `max_output_bytes`. A larger SVG is returned as
    /// embedded blob resources (base64), up to `max_output_bytes`.
    pub svg_blob_threshold_bytes: usize,
    /// Most bytes of program stdout returned.
    pub max_program_output_bytes: usize,
    /// Smallest accepted dpi.
    pub min_dpi: f64,
    /// Largest accepted dpi.
    pub max_dpi: f64,
    /// Renders this server runs at once.
    pub max_concurrent_renders: usize,
    /// Default page size in points when none is given.
    pub default_page_points: [u32; 2],
}

/// Async job settings, in `get_renderer_info`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct JobsInfo {
    /// Whether `start_render` and its companions are served.
    pub enabled: bool,
    /// Whether the MCP Tasks extension is served.
    pub tasks: bool,
    /// With Tasks: how long a call runs, in milliseconds, before it
    /// becomes a task (0: at once).
    pub task_after_ms: u64,
    /// Wall-clock deadline of one job's worker, in milliseconds.
    pub job_timeout_ms: u64,
    /// PSCat's step limit for a job; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_max_steps: Option<u64>,
    /// Jobs running at once (more wait queued).
    pub max_running_jobs: usize,
    /// Unfinished jobs one client may have.
    pub max_jobs_per_principal: usize,
    /// Unfinished jobs on the server.
    pub max_jobs: usize,
    /// How long a finished job is kept, in milliseconds.
    pub result_ttl_ms: u64,
    /// Bytes of results held, all jobs together.
    pub results_budget_bytes: usize,
    /// Bytes of that budget one client may have reserved and held.
    pub results_per_principal_bytes: usize,
}

/// Result of `get_renderer_info`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RendererInfo {
    /// The PostScript interpreter.
    pub renderer: RendererIdentity,
    /// This MCP server's version.
    pub server_version: String,
    /// Output formats.
    pub formats: Vec<FormatInfo>,
    /// Limits in force.
    pub limits: LimitsInfo,
    /// Async jobs.
    pub jobs: JobsInfo,
    /// What a program can and cannot reach.
    pub capabilities: Vec<String>,
    /// Font packs, present only when the operator configured a font-pack
    /// directory (`PSCAT_MCP_FONT_PACK_DIR`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_packs: Option<FontPackInfo>,
}

/// The font packs the workers can use, in `get_renderer_info`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FontPackInfo {
    /// Names of the installed packs, such as `cjk-thai`. The directory
    /// itself is not reported. Absent if the catalog could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Vec<String>>,
}

/// The names of the font packs in a catalog, in its order.
fn installed_packs(catalog: &ArtCatalog) -> Vec<String> {
    catalog
        .font_packs
        .installed
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

/// Which transport a handler serves. It decides who owns an async job: the
/// one local user over stdio, the authenticated principal over HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// One local user (`serve-stdio`, or an in-process pipe).
    Stdio,
    /// Streamable HTTP behind the auth layer: every request must carry the
    /// authenticated `Principal`, or it cannot own or see a job.
    Http,
}

/// The MCP server handler shared by the stdio and HTTP transports.
#[derive(Clone)]
pub struct PscatMcp {
    service: Arc<RenderService>,
    transport: Transport,
    tool_router: ToolRouter<Self>,
    /// The guide, built on first use (see [`PscatMcp::guide`]).
    guide: Arc<tokio::sync::OnceCell<Arc<Guide>>>,
    /// What the guide is built from, before the installed font packs are
    /// known.
    guide_facts: Arc<ServerFacts>,
}

impl From<CapabilitiesArgs> for CatalogQuery {
    fn from(a: CapabilitiesArgs) -> Self {
        CatalogQuery {
            kind: a.kind,
            names: a.names,
            offset: a.offset,
            limit: a.limit,
        }
    }
}

impl PscatMcp {
    /// A stdio handler over `service`: every call is the one local user's.
    /// HTTP uses [`PscatMcp::for_http`].
    pub fn new(service: Arc<RenderService>) -> Self {
        Self::with_transport(service, Transport::Stdio)
    }

    /// An HTTP handler over `service`: job calls need the authenticated
    /// principal the auth layer puts in the request.
    pub fn for_http(service: Arc<RenderService>) -> Self {
        Self::with_transport(service, Transport::Http)
    }

    /// A handler over `service` for `transport`.
    pub fn with_transport(service: Arc<RenderService>, transport: Transport) -> Self {
        let mut tool_router = Self::tool_router();
        let limits = service.limits();
        if let Some(route) = tool_router.map.get_mut("render_postscript") {
            let base = route.attr.description.clone().unwrap_or_default();
            route.attr.description = Some(
                format!(
                    "{base} Limits on this server: source up to {} bytes; at most {} pixels per \
                     page and {PSCAT_MAX_SIDE_PX} pixels a side; at most {} pages; output up to {} bytes \
                     (over it, output_too_large); svg up to {} bytes in all is returned as text \
                     resources, larger as blob resources (base64); {} ms per render.",
                    limits.max_source_bytes,
                    limits.max_pixels,
                    limits.max_pages,
                    limits.max_output_bytes,
                    limits.svg_blob_threshold_bytes(),
                    limits.render_timeout_ms
                )
                .into(),
            );
        }
        if let Some(route) = tool_router.map.get_mut("chalkboard_math") {
            let base = route.attr.description.clone().unwrap_or_default();
            route.attr.description = Some(
                format!(
                    "{base} Limits on this server: latex up to {} bytes; at most {} pixels per \
                     board and {PSCAT_MAX_SIDE_PX} pixels a side; output up to {} bytes (over it, \
                     output_too_large); svg up to {} bytes is returned as a text resource, larger \
                     as a blob resource (base64), and a board's SVG runs to megabytes; {} ms per \
                     board.",
                    limits.max_latex_bytes,
                    limits.max_pixels,
                    limits.max_output_bytes,
                    limits.svg_blob_threshold_bytes(),
                    limits.render_timeout_ms
                )
                .into(),
            );
        }
        if let Some(route) = tool_router.map.get_mut("handwrite") {
            let base = route.attr.description.clone().unwrap_or_default();
            route.attr.description = Some(
                format!(
                    "{base} Limits on this server: text up to {} bytes; at most {} pixels per \
                     note and {PSCAT_MAX_SIDE_PX} pixels a side; {} ms per note.",
                    limits.max_handwrite_bytes, limits.max_pixels, limits.render_timeout_ms
                )
                .into(),
            );
        }
        if let Some(route) = tool_router.map.get_mut("review_sheet") {
            let base = route.attr.description.clone().unwrap_or_default();
            let per_cell_ms = limits.interp_time_limit().map_or(String::new(), |t| {
                format!(" and at most {} ms", t.as_millis())
            });
            route.attr.description = Some(
                format!(
                    "{base} Limits on this server: at most {} cells; sources up to {} bytes in \
                     all and {} each; the cells render at most {} pixels in all; the sheet at \
                     most {} pixels and {PSCAT_MAX_SIDE_PX} a side; labels up to \
                     {MAX_LABEL_CHARS} characters; {} ms for the whole sheet, each cell the \
                     render's step limit{per_cell_ms}, never more than an equal share of the \
                     time left (a cell past its limit fails with limit_exceeded); the PNG up to \
                     {} bytes.",
                    limits.max_sheet_cells,
                    limits.sheet_source_bytes(),
                    limits.max_source_bytes,
                    limits.sheet_cell_pixels(),
                    limits.sheet_pixels(),
                    limits.sheet_timeout_ms(),
                    limits.max_output_bytes,
                )
                .into(),
            );
            let schema = Arc::make_mut(&mut route.attr.input_schema);
            if let Some(Value::Object(entries)) = schema
                .get_mut("properties")
                .and_then(|p| p.get_mut("entries"))
            {
                entries.insert("maxItems".into(), limits.max_sheet_cells.into());
            }
        }
        let jobs = service.jobs();
        if jobs.enabled() {
            let c = jobs.config();
            if let Some(route) = tool_router.map.get_mut("render_postscript") {
                let base = route.attr.description.clone().unwrap_or_default();
                route.attr.description = Some(
                    format!(
                        "{base} For a render that may take longer, start_render runs the same \
                         arguments as a job of up to {} ms.",
                        c.job_timeout_ms
                    )
                    .into(),
                );
            }
            if let Some(route) = tool_router.map.get_mut("start_render") {
                let base = route.attr.description.clone().unwrap_or_default();
                route.attr.description = Some(
                    format!(
                        "{base} Limits on this server: a job's worker may run {} ms{}, under \
                         the same memory, pixel, page, output and SVG limits as render_postscript; \
                         {} jobs run at once and more wait queued; at most {} unfinished jobs \
                         per client and {} in all; a finished job is kept {} ms.",
                        c.job_timeout_ms,
                        c.job_limits(limits)
                            .steps()
                            .map_or(String::new(), |n| format!(" and {n} interpreter steps")),
                        c.running_jobs(limits),
                        c.max_jobs_per_principal,
                        c.max_jobs,
                        c.job_result_ttl_ms
                    )
                    .into(),
                );
            }
        } else {
            for name in JOB_TOOLS {
                tool_router.remove_route(name);
            }
        }
        for route in tool_router.map.values_mut() {
            strip_uint_formats(Arc::make_mut(&mut route.attr.input_schema));
            if let Some(schema) = route.attr.output_schema.as_mut() {
                strip_uint_formats(Arc::make_mut(schema));
            }
        }
        let facts = ServerFacts {
            tools: tool_router
                .list_all()
                .into_iter()
                .map(|t| t.name.to_string())
                .collect(),
            render_arguments: tool_router
                .get("render_postscript")
                .and_then(|t| t.input_schema.get("properties"))
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default(),
            render_timeout_ms: limits.render_timeout_ms,
            job_timeout_ms: jobs.enabled().then_some(jobs.config().job_timeout_ms),
            max_prompt_argument_bytes: limits.max_prompt_argument_bytes,
            // Known now only when no pack directory is configured; otherwise
            // `guide()` reads the installed packs from the catalog first.
            pack_faces: if service.font_packs_configured() {
                PackFaces::Configured { installed: None }
            } else {
                PackFaces::NotConfigured
            },
        };
        PscatMcp {
            service,
            transport,
            tool_router,
            guide: Arc::new(tokio::sync::OnceCell::new()),
            guide_facts: Arc::new(facts),
        }
    }

    /// The underlying service.
    pub fn service(&self) -> &Arc<RenderService> {
        &self.service
    }

    /// The view of the art catalog `query` asks for. Arguments are checked
    /// before any worker starts; the catalog itself is kept after the first
    /// fetch.
    async fn capabilities_slice(&self, query: &CatalogQuery) -> Result<CatalogSlice, RenderError> {
        query.check()?;
        let catalog = self.service.capabilities().await?;
        crate::catalog::slice(&catalog, query)
    }

    /// The guide. With no font-pack directory it is built once, with no
    /// worker. With one, the installed packs come from the art catalog (a
    /// worker, run once and kept); a catalog that cannot be read gives a
    /// guide that says so, which is not kept, so the next call asks again.
    async fn guide(&self) -> Arc<Guide> {
        if let Some(guide) = self.guide.get() {
            return guide.clone();
        }
        if !self.service.font_packs_configured() {
            return self.build_guide(None).await;
        }
        match self.service.capabilities().await {
            Ok(catalog) => self.build_guide(Some(installed_packs(&catalog))).await,
            Err(_) => Arc::new(Guide::new(&self.guide_facts_with(None))),
        }
    }

    /// Build and keep the guide, with `installed` as its pack facts
    /// (`None` when no pack directory is configured).
    async fn build_guide(&self, installed: Option<Vec<String>>) -> Arc<Guide> {
        let facts = self.guide_facts_with(installed);
        self.guide
            .get_or_init(|| async { Arc::new(Guide::new(&facts)) })
            .await
            .clone()
    }

    /// `guide_facts` with the pack facts for `installed`.
    fn guide_facts_with(&self, installed: Option<Vec<String>>) -> ServerFacts {
        let mut facts = (*self.guide_facts).clone();
        if self.service.font_packs_configured() {
            facts.pack_faces = PackFaces::Configured { installed };
        }
        facts
    }

    async fn renderer_info(&self) -> RendererInfo {
        let l = self.service.limits();
        // The packs come from the catalog the worker reads (the server never
        // looks at the directory); fetched once, then kept.
        let font_packs = if self.service.font_packs_configured() {
            Some(FontPackInfo {
                installed: self
                    .service
                    .capabilities()
                    .await
                    .ok()
                    .map(|c| installed_packs(&c)),
            })
        } else {
            None
        };
        let returned_as = |f: OutputFormat| match f {
            OutputFormat::Png => "one image content block per page",
            OutputFormat::Svg => {
                "one embedded text resource per page, or one embedded blob resource (base64) per \
                 page when all pages together are over svg_blob_threshold_bytes"
            }
            OutputFormat::Pdf => "one embedded blob resource holding every page",
        };
        RendererInfo {
            renderer: RendererIdentity::current(),
            server_version: SERVER_VERSION.to_string(),
            formats: OutputFormat::ALL
                .into_iter()
                .map(|f| FormatInfo {
                    name: f.name().to_string(),
                    mime_type: f.mime_type().to_string(),
                    returned_as: returned_as(f).to_string(),
                })
                .collect(),
            limits: LimitsInfo {
                max_source_bytes: l.max_source_bytes,
                max_latex_bytes: l.max_latex_bytes,
                max_handwrite_bytes: l.max_handwrite_bytes,
                max_sheet_cells: l.max_sheet_cells,
                max_sheet_source_bytes: l.sheet_source_bytes(),
                max_sheet_cell_pixels: l.sheet_cell_pixels(),
                max_sheet_pixels: l.sheet_pixels(),
                sheet_timeout_ms: l.sheet_timeout_ms(),
                render_timeout_ms: l.render_timeout_ms,
                max_pixels: l.max_pixels,
                max_side_px: PSCAT_MAX_SIDE_PX,
                max_pages: l.max_pages,
                max_output_bytes: l.max_output_bytes,
                svg_blob_threshold_bytes: l.svg_blob_threshold_bytes(),
                max_program_output_bytes: l.max_program_output_bytes,
                min_dpi: MIN_DPI,
                max_dpi: MAX_DPI,
                max_concurrent_renders: l.concurrency(),
                default_page_points: [DEFAULT_PAGE_POINTS.0, DEFAULT_PAGE_POINTS.1],
            },
            jobs: {
                let jobs = self.service.jobs();
                let c = jobs.config();
                JobsInfo {
                    enabled: jobs.enabled(),
                    tasks: jobs.tasks_enabled(),
                    task_after_ms: c.task_after_ms,
                    job_timeout_ms: c.job_timeout_ms,
                    job_max_steps: c.job_limits(l).steps(),
                    max_running_jobs: c.running_jobs(l),
                    max_jobs_per_principal: c.max_jobs_per_principal,
                    max_jobs: c.max_jobs,
                    result_ttl_ms: c.job_result_ttl_ms,
                    results_budget_bytes: c.job_results_budget_bytes,
                    results_per_principal_bytes: c.principal_share_bytes(l),
                }
            },
            capabilities: [
                "File reads are confined to PSCat's built-in lib/ directory (for example \
                 `(lib/artkit.ps) run`); other paths fail with invalidfileaccess."
                    .into(),
                "Programs cannot write files.".into(),
                "Programs have no network access.".into(),
                "Each render runs in its own short-lived worker process with step, time and \
                 memory limits."
                    .into(),
            ]
            .into_iter()
            .chain(font_packs.is_some().then(|| {
                "Fonts also come from the font packs the server operator installed (see \
                 font_packs); programs cannot read the pack files themselves."
                    .to_string()
            }))
            .collect(),
            font_packs,
        }
    }
}

/// Removes the `format` keyword schemars gives unsigned integers (`uint`,
/// `uint8` to `uint64`) from a tool schema, recursively. Those formats are
/// not JSON Schema or OpenAPI formats, so clients that validate schemas
/// (Claude Code) warn `unknown format "uint32" ignored` for each one. The
/// `minimum: 0` schemars emits next to them stays, so the schema still
/// says the value is a non-negative integer.
pub(crate) fn strip_uint_formats(schema: &mut serde_json::Map<String, serde_json::Value>) {
    if schema
        .get("format")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_uint_format)
    {
        schema.remove("format");
    }
    for value in schema.values_mut() {
        strip_uint_formats_in(value);
    }
}

fn strip_uint_formats_in(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => strip_uint_formats(map),
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_uint_formats_in),
        _ => {}
    }
}

fn is_uint_format(format: &str) -> bool {
    matches!(
        format,
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" | "uint128"
    )
}

/// Every tool only reads its arguments (if any) and runs the program (if any) in a
/// sandboxed worker that cannot write files or reach the network, so they
/// are annotated read-only, non-destructive, idempotent (the same input
/// renders the same output) and closed-world.
#[tool_router]
impl PscatMcp {
    #[tool(
        name = "render_postscript",
        title = "Render PostScript",
        description = "Runs a PostScript program in the PSCat interpreter and returns the pages it \
                       produced as PNG images (default), SVG documents or a PDF document, with a \
                       short text summary (the size of each page in points and in pixels, and \
                       the dpi) and \
                       the program's stdout. The program runs in a sandbox: reading files is refused except for PSCat's built-in lib/ \
                       (for example `(lib/artkit.ps) run`), and nothing can be written. A \
                       PostScript error returns the error name, offending command and line \
                       instead of an image.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RenderToolOutput>(),
        annotations(
            title = "Render PostScript",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn render_postscript(
        &self,
        Parameters(args): Parameters<RenderArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let outcome = tokio::select! {
            r = self.service.render(args.into_request()) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(out) => render_success(out, self.service.limits().svg_blob_threshold_bytes()),
            Err(err) => render_failure(err),
        }
    }

    #[tool(
        name = "chalkboard_math",
        title = "Chalkboard math",
        description = "Writes a LaTeX formula on a chalkboard in hand-drawn chalk strokes, with \
                       eraser marks and the ghost of earlier writing, and returns the board as a \
                       PNG image (default), an SVG document or a PDF document. `latex` is typed \
                       as in a LaTeX document, with single backslashes: `\\\\` breaks rows, `&` \
                       aligns them, and an align or equation wrapper or $...$ is accepted. The \
                       TeX subset is PSCat mathkit's (fractions, roots, sums, integrals, Greek \
                       letters, matrices, cases, accents); a command outside it fails with \
                       postscript_error naming it, and a bad board setting with invalid_request. \
                       The default board is 800x400 points at 144 dpi (1600x800 pixels); the \
                       result gives the board's size in points (`width_pt`, `height_pt`), in pixels \
                       and its dpi. SVG and PDF keep every chalk grain as a vector mark: the default board is \
                       about 0.3 MB as PNG, 2.6 MB as PDF and 7 to 10 MB as SVG. The same \
                       arguments give the same image. The input is LaTeX only, never PostScript.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RenderToolOutput>(),
        annotations(
            title = "Chalkboard math",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chalkboard_math(
        &self,
        Parameters(args): Parameters<ChalkboardArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let outcome = tokio::select! {
            r = self.service.generate(args.into_request()) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(out) => render_success(out, self.service.limits().svg_blob_threshold_bytes()),
            Err(err) => failure_with("Chalkboard failed", err),
        }
    }

    #[tool(
        name = "handwrite",
        title = "Handwriting-style note",
        description = "Writes text as a handwriting-style note: a hand-drawn script with \
                       per-letter variation, word-wrapped to `width`, on a page whose height \
                       fits the text. Returns the note as a PNG image. These are hand-drawn \
                       styles for notes, cards and art, not for imitating a specific person's \
                       handwriting or presenting writing as someone else's. The script has \
                       the letters a to z (capitals are written as lowercase), the digits 0 to \
                       9, and . , - ' ! ?; other characters leave a gap. Newlines force line \
                       breaks. `paper` ruled adds ruled lines and a margin rule. `jitter` is a whole \
                       number from 0 to 10000 (default 16). The same \
                       arguments give the same image. The default note is 612 points wide at \
                       72 dpi, 36 point type in dark blue ink; a one-line note is 146 points \
                       tall and each further line adds 54 points. A note whose height would \
                       pass the pixel or side limit fails with too_many_pixels or \
                       invalid_dimensions, before it is drawn. The input is text only, never \
                       PostScript.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RenderToolOutput>(),
        annotations(
            title = "Handwriting-style note",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn handwrite(
        &self,
        Parameters(args): Parameters<HandwriteArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let outcome = tokio::select! {
            r = self.service.generate(args.into_request()) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(out) => render_success(out, self.service.limits().svg_blob_threshold_bytes()),
            Err(err) => failure_with("Handwrite failed", err),
        }
    }

    #[tool(
        name = "review_sheet",
        title = "Review sheet",
        description = "Composes one labelled PNG from several PostScript programs, each rendered \
                       at one or more resolutions, to compare candidates (logos, icons, a seed's \
                       variants) side by side. Each entry is a row and each item of its dpi \
                       list a cell, drawn at its true pixel size (a 48 px render stays 48 px \
                       beside a 512 px one), centred in its column with a hairline around it, \
                       the row label above and a pixel-size caption (`48x48 px`) under each \
                       cell. A dpi item {\"dpi\": 9, \"zoom\": 4} draws that render enlarged 4x \
                       with no smoothing (whole factors 2 to 16). A program that shows several \
                       pages gives its last one. A cell whose program fails is drawn as far as \
                       it got; structuredContent lists every cell with its size in pixels and in \
                       points, its dpi, its place on the sheet and, for a failed cell, its \
                       error code and PSCat's report. \
                       Labels are drawn in PSCat's fonts: Latin with accents, and Korean, \
                       Japanese, Chinese and Thai only where the cjk-thai font pack is \
                       installed (otherwise as ?). Programs run in the same sandbox as \
                       render_postscript, one after another in one worker.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SheetToolOutput>(),
        annotations(
            title = "Review sheet",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn review_sheet(
        &self,
        Parameters(args): Parameters<ReviewSheetArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let outcome = tokio::select! {
            r = self.service.generate(args.into_request()) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(out) => sheet_success(out),
            Err(err) => sheet_failure(err),
        }
    }

    #[tool(
        name = "validate_postscript",
        title = "Validate PostScript",
        description = "Checks a PostScript program without returning an image. It runs a \
                       tokenizer pass and then executes the program in the same sandbox and \
                       with the same limits as render_postscript, discarding the output. It \
                       returns any PostScript error (name, offending command, line), lint \
                       findings (blank page, operand-stack leak, unbalanced gsave), the page \
                       count and the program's stdout.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ValidateToolOutput>(),
        annotations(
            title = "Validate PostScript",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn validate_postscript(
        &self,
        Parameters(args): Parameters<ValidateArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let outcome = tokio::select! {
            r = self.service.validate(args.source) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(report) => {
                let text = validation_text(&report);
                let envelope = ValidateToolOutput {
                    ok: true,
                    report: Some(report),
                    error: None,
                    renderer: RendererIdentity::current(),
                };
                with_structured(
                    CallToolResult::success(vec![ContentBlock::text(text)]),
                    &envelope,
                )
            }
            Err(err) => validate_failure(err),
        }
    }

    #[tool(
        name = "describe_art_capabilities",
        title = "List art capabilities",
        description = "Lists the creative capabilities of the linked PSCat build: fonts, Type 3 \
                       faces, mood palettes, page templates, artkit and style-pack procedures, \
                       native operators and config dials. With no arguments it returns an index \
                       of every kind with its names. `kind` returns that kind's names with a \
                       one-line description each, 100 at a time (`offset`, `limit`). `names` \
                       (up to 20) returns full entries: parameters, defining library file, `run` \
                       lines to load it, example, availability; unknown names are listed under \
                       `not_found`. A reply is under 24 KB. Runs no PostScript and does not \
                       report server file paths.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CapabilitiesToolOutput>(),
        annotations(
            title = "List art capabilities",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn describe_art_capabilities(
        &self,
        Parameters(args): Parameters<CapabilitiesArgs>,
        ct: CancellationToken,
    ) -> CallToolResult {
        let query = CatalogQuery::from(args);
        let outcome = tokio::select! {
            r = self.capabilities_slice(&query) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(slice) => {
                let text = slice.text();
                let envelope = CapabilitiesToolOutput {
                    ok: true,
                    catalog: Some(slice),
                    error: None,
                    renderer: RendererIdentity::current(),
                };
                with_structured(
                    CallToolResult::success(vec![ContentBlock::text(text)]),
                    &envelope,
                )
            }
            Err(err) => capabilities_failure(err),
        }
    }

    #[tool(
        name = "start_render",
        title = "Start a render job",
        description = "Starts render_postscript as a background job, for a render that may take \
                       longer than one call: the same arguments, and it returns a job_id at \
                       once. render_status(job_id) reports the state (queued, running, done, \
                       failed or cancelled); render_result(job_id) then returns what \
                       render_postscript would have (the pages, or the error); cancel_render \
                       (job_id) stops it. Arguments are checked here, before a job is \
                       accepted. A job belongs to the client that started it.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<JobToolOutput>(),
        annotations(
            title = "Start a render job",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn start_render(
        &self,
        Parameters(args): Parameters<RenderArgs>,
        ext: Extensions,
    ) -> CallToolResult {
        let started = principal(self.transport, &ext).and_then(|owner| {
            self.service
                .start_job(&owner, JobRequest::Render(args.into_request()))
        });
        match started {
            Ok(status) => job_success(
                status,
                Some(
                    "poll render_status with this job_id until its state is done, failed or \
                     cancelled, then call render_result"
                        .into(),
                ),
            ),
            Err(err) => job_failure("Job not started", err),
        }
    }

    #[tool(
        name = "render_status",
        title = "Render job status",
        description = "The state of a start_render job: queued, running, done, failed or \
                       cancelled, with elapsed_seconds and the job's timeout_seconds; for a \
                       failed job, job_error (its code and message); for a finished one, \
                       expires_in_seconds. Answers at once, while renders run. An unknown, \
                       expired or other client's job_id is unknown_job.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<JobToolOutput>(),
        annotations(
            title = "Render job status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn render_status(
        &self,
        Parameters(args): Parameters<JobIdArgs>,
        ext: Extensions,
    ) -> CallToolResult {
        match principal(self.transport, &ext)
            .and_then(|owner| self.service.jobs().status(&owner, &args.job_id))
        {
            Ok(status) => job_success(status, None),
            Err(err) => job_failure("No job status", err),
        }
    }

    #[tool(
        name = "render_result",
        title = "Render job result",
        description = "The result of a finished start_render job: what render_postscript \
                       returns, the pages as content blocks, or a failed render's error with \
                       isError. A job still queued or running is job_not_finished; a cancelled \
                       one, cancelled; an unknown, expired or other client's job_id, \
                       unknown_job. The result can be fetched again until the job expires.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RenderToolOutput>(),
        annotations(
            title = "Render job result",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn render_result(
        &self,
        Parameters(args): Parameters<JobIdArgs>,
        ext: Extensions,
    ) -> CallToolResult {
        match principal(self.transport, &ext)
            .and_then(|owner| self.service.jobs().result(&owner, &args.job_id))
        {
            Ok(result) => job_result(result, self.service.limits().svg_blob_threshold_bytes()),
            Err(err) => failure_with("No render result", err),
        }
    }

    #[tool(
        name = "cancel_render",
        title = "Cancel a render job",
        description = "Cancels a start_render job: a queued job never starts, a running one's \
                       worker is stopped. Returns the job's status, cancelled once it has \
                       stopped. A finished job is left as it is.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<JobToolOutput>(),
        annotations(
            title = "Cancel a render job",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn cancel_render(
        &self,
        Parameters(args): Parameters<JobIdArgs>,
        ext: Extensions,
    ) -> CallToolResult {
        let owner = match principal(self.transport, &ext) {
            Ok(owner) => owner,
            Err(err) => return job_failure("Job not cancelled", err),
        };
        match self.service.cancel_job(&owner, &args.job_id).await {
            Ok(status) => job_success(status, None),
            Err(err) => job_failure("Job not cancelled", err),
        }
    }

    #[tool(
        name = "get_renderer_info",
        title = "PSCat renderer info",
        description = "Reports the PSCat renderer version, this server's version, the supported \
                       output formats with their MIME types, the limits in force (source size, \
                       timeout, pixels, pages, output size, dpi range, concurrency) and what \
                       programs can access, and the installed font packs when the server has \
                       a font-pack directory. Runs no PostScript.",
        annotations(
            title = "PSCat renderer info",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_renderer_info(&self) -> Json<RendererInfo> {
        Json(self.renderer_info().await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PscatMcp {
    // Written out (instead of generated by `tool_handler`) so that argument
    // deserialization failures get the same structured envelope as every
    // other failure. See [`with_argument_error_envelope`].
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.clone();
        let wants_task = self.service.jobs().tasks_enabled()
            && context
                .client_capabilities()
                .is_some_and(|caps| caps.supports_tasks());
        // A task-eligible call from a Tasks client runs synchronously first
        // and becomes a task only past the task threshold (see
        // `RenderService::call_or_task`). A request with no owner (HTTP
        // without a principal) cannot own a task, so it takes the
        // synchronous path below.
        if wants_task
            && let Some(job) = task_request(&request)
            && let Ok(owner) = principal(self.transport, &context.extensions)
        {
            return Ok(
                match self.service.call_or_task(&owner, job, &context.ct).await {
                    TaskCall::Task(status) => CallToolResponse::Task(CreateTaskResult::new(
                        task_of(&status, self.service.jobs().config()),
                    )),
                    // Finished before the threshold, or refused before a job
                    // exists: what the tool returns synchronously.
                    TaskCall::Done(outcome) => CallToolResponse::Complete(job_result(
                        JobResult {
                            tool: tool.to_string(),
                            outcome: outcome.map(Arc::new).map_err(Arc::new),
                        },
                        self.service.limits().svg_blob_threshold_bytes(),
                    )),
                },
            );
        }
        let tcc = ToolCallContext::new(self, request, context);
        Ok(match self.tool_router.call(tcc).await? {
            CallToolResponse::Complete(result) => {
                CallToolResponse::Complete(with_argument_error_envelope(&tool, result))
            }
            other => other,
        })
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        let jobs = self.service.jobs();
        let (status, result) = principal(self.transport, &context.extensions)
            .and_then(|owner| jobs.status_and_result(&owner, &request.task_id))
            .map_err(|_| unknown_task(&request.task_id))?;
        let payload = match (status.state, result) {
            (JobState::Cancelled, _) => TaskPayload::Cancelled,
            // A render that failed is still a completed tool call, with
            // `isError: true` (SEP-2663: `failed` is for JSON-RPC errors).
            (_, Some(result)) => TaskPayload::Completed {
                result: result_object(&job_result(
                    result,
                    self.service.limits().svg_blob_threshold_bytes(),
                )),
            },
            _ => TaskPayload::Working,
        };
        Ok(GetTaskResult::new(DetailedTask::new(
            task_of(&status, jobs.config()),
            payload,
        )))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        principal(self.transport, &context.extensions)
            .and_then(|owner| self.service.jobs().request_cancel(&owner, &request.task_id))
            .map_err(|_| unknown_task(&request.task_id))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        // A render never asks for input, so every response is for an
        // unknown key and is ignored, as the extension requires.
        principal(self.transport, &context.extensions)
            .and_then(|owner| self.service.jobs().status(&owner, &request.task_id))
            .map(|_| ())
            .map_err(|_| unknown_task(&request.task_id))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(
            self.guide().await.prompts(),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        self.guide()
            .await
            .prompt(&request.name, request.arguments.as_ref())
            .map(GetPromptResponse::from)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            self.guide().await.resources(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.guide()
            .await
            .read(&request.uri)
            .map(ReadResourceResponse::from)
    }

    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder()
            .enable_prompts()
            .enable_resources()
            .enable_tools()
            .build();
        if self.service.jobs().tasks_enabled() {
            capabilities
                .extensions
                .get_or_insert_with(Default::default)
                .insert(TASKS_EXTENSION_ID.to_string(), JsonObject::new());
        }
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("pscat-mcp", SERVER_VERSION))
            .with_instructions(
                "Renders PostScript programs to PNG, SVG or PDF with the PSCat interpreter, \
                 validates programs without rendering, writes LaTeX formulas on a chalkboard, writes text \
                 as a handwriting-style note, composes several programs at several sizes into one \
                 labelled review sheet, \
                 and lists the fonts, palettes, page \
                 templates and procedures the interpreter has. Programs run in a sandbox with no file \
                 writes, no network, and file reads limited to PSCat's built-in lib/. The PSCat \
                 plugin's skills (guides to writing PSCat PostScript) are available as text/markdown \
                 resources under pscat://guide/ and as the prompts diagram, generative_art and chart.",
            )
    }
}

/// Serve MCP over stdin/stdout until the client disconnects.
pub async fn serve_stdio(service: Arc<RenderService>) -> anyhow::Result<()> {
    let running = PscatMcp::new(service.clone())
        .serve(rmcp::transport::stdio())
        .await?;
    let reason = running.waiting().await;
    // Jobs are killed, not drained: the client that could fetch them is gone.
    service
        .shutdown_jobs(std::time::Duration::from_secs(2))
        .await;
    tracing::info!(reason = ?reason.as_ref().ok(), "stdio session ended");
    reason?;
    Ok(())
}

/// The tools that exist only while async jobs are on.
const JOB_TOOLS: [&str; 4] = [
    "start_render",
    "render_status",
    "render_result",
    "cancel_render",
];

/// The tools a Tasks-capable client's `tools/call` runs as a task: those
/// whose result is a [`RenderToolOutput`] and whose plan takes the job
/// deadline (`review_sheet`'s plan shares out its own deadline).
const TASK_TOOLS: [&str; 3] = ["render_postscript", "chalkboard_math", "handwrite"];

/// Who is calling, as the owner of async jobs.
///
/// * stdio: the one local user.
/// * HTTP: the `Principal` the auth layer put in the request's extensions
///   (rmcp passes the request's `http::request::Parts` on). A request with
///   no `Parts` or no `Principal` is refused: it never gets the local
///   identity.
///
/// The owner is the principal's id: `token#N`, the token's position in the
/// configured list, or `anonymous` under `--allow-unauthenticated` (loopback
/// only), where every caller shares one owner. This relies on the token list
/// never changing while the process runs: tokens are read once at startup,
/// and a restart loses every job anyway. If tokens are ever reloaded at run
/// time, the owner must become stable per token (for example an HMAC of the
/// token under a key generated at process start), or a job could pass to
/// whichever token takes its old position.
fn principal(transport: Transport, ext: &Extensions) -> Result<String, RenderError> {
    match transport {
        Transport::Stdio => Ok(LOCAL_PRINCIPAL.to_string()),
        Transport::Http => ext
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<crate::auth::Principal>())
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                RenderError::new(
                    ErrorCode::InvalidRequest,
                    "this request has no authenticated client, so it cannot own a job",
                )
            }),
    }
}

/// The job a task-eligible `tools/call` asks for, or `None` when it is not
/// eligible or its arguments do not parse (the synchronous path then gives
/// the argument error its usual envelope).
fn task_request(request: &CallToolRequestParams) -> Option<JobRequest> {
    if !TASK_TOOLS.contains(&request.name.as_ref()) {
        return None;
    }
    let args = Value::Object(request.arguments.clone().unwrap_or_default());
    match request.name.as_ref() {
        "render_postscript" => serde_json::from_value::<RenderArgs>(args)
            .ok()
            .map(|a| JobRequest::Render(a.into_request())),
        "chalkboard_math" => serde_json::from_value::<ChalkboardArgs>(args)
            .ok()
            .map(|a| JobRequest::Generate(a.into_request())),
        "handwrite" => serde_json::from_value::<HandwriteArgs>(args)
            .ok()
            .map(|a| JobRequest::Generate(a.into_request())),
        _ => None,
    }
}

/// Suggested `tasks/get` interval.
const TASK_POLL_INTERVAL_MS: u64 = 1000;

/// The Tasks extension's view of a job.
fn task_of(status: &JobStatus, config: &crate::config::JobsConfig) -> Task {
    let stamp = |t: std::time::SystemTime| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339();
    let task_status = match status.state {
        JobState::Queued | JobState::Running => TaskStatus::Working,
        JobState::Done | JobState::Failed => TaskStatus::Completed,
        JobState::Cancelled => TaskStatus::Cancelled,
    };
    let message = match status.state {
        _ if status.cancel_requested => "cancel requested",
        JobState::Queued => "queued: waiting for a render slot",
        JobState::Running => "rendering",
        JobState::Done => "done",
        JobState::Failed => "the render failed; the result has the error",
        JobState::Cancelled => "cancelled",
    };
    // The longest a task can be kept: its run, then the result TTL (time
    // queued comes on top).
    let ttl_ms = config
        .job_timeout_ms
        .saturating_add(config.job_result_ttl_ms);
    Task::new(
        status.job_id.clone(),
        task_status,
        stamp(status.created_at),
        stamp(status.updated_at),
    )
    .with_status_message(message)
    .with_ttl_ms(ttl_ms)
    .with_poll_interval_ms(TASK_POLL_INTERVAL_MS)
}

/// The same "unknown task" error rmcp's own task store gives, for an
/// unknown, expired or other client's task.
fn unknown_task(id: &str) -> ErrorData {
    let shown: String = id.chars().take(64).collect();
    ErrorData::invalid_params(format!("unknown task: {shown}"), None)
}

fn result_object(result: &CallToolResult) -> JsonObject {
    match serde_json::to_value(result) {
        Ok(Value::Object(map)) => map,
        _ => JsonObject::new(),
    }
}

/// What the job's tool would have returned.
fn job_result(result: JobResult, svg_text_max: usize) -> CallToolResult {
    match result.outcome {
        Ok(out) => render_success((*out).clone(), svg_text_max),
        Err(err) => failure_with(failure_lead(&result.tool), (*err).clone()),
    }
}

/// The first words of a failed render tool's text.
fn failure_lead(tool: &str) -> &'static str {
    match tool {
        "chalkboard_math" => "Chalkboard failed",
        "handwrite" => "Handwrite failed",
        _ => "Render failed",
    }
}

fn job_success(status: JobStatus, next: Option<String>) -> CallToolResult {
    let envelope = JobToolOutput {
        ok: true,
        job: Some(status),
        next,
        error: None,
        renderer: RendererIdentity::current(),
    };
    let text = serde_json::to_value(&envelope)
        .map(|mut v| {
            if let Some(m) = v.as_object_mut() {
                m.remove("renderer");
            }
            v.to_string()
        })
        .unwrap_or_default();
    with_structured(
        CallToolResult::success(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

fn job_failure(lead: &str, err: RenderError) -> CallToolResult {
    let text = error_text(lead, &err);
    let envelope = JobToolOutput {
        ok: false,
        job: None,
        next: None,
        error: Some(err),
        renderer: RendererIdentity::current(),
    };
    with_structured(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

fn code_name(code: ErrorCode) -> String {
    match serde_json::to_value(code) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{code:?}"),
    }
}

fn with_structured<T: Serialize>(mut result: CallToolResult, value: &T) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(v) => result.structured_content = Some(v),
        Err(e) => tracing::error!(error = %e, "serializing structured content failed"),
    }
    result
}

fn render_uri(id: &uuid::Uuid, page: Option<usize>) -> String {
    match page {
        Some(n) => format!("urn:pscat:render:{id}:page:{n}"),
        None => format!("urn:pscat:render:{id}:document"),
    }
}

/// The result of a successful render. SVG goes as embedded text resources
/// when all pages together are at most `svg_text_max` bytes, and as blob
/// resources (base64) above that, so a client saves it as a file the way
/// it does a PDF (docs/LOCAL_TESTING.md, section 11). One result never
/// mixes the two.
fn render_success(out: RenderOutput, svg_text_max: usize) -> CallToolResult {
    let mime = out.format.mime_type();
    let id = uuid::Uuid::new_v4();
    let b64 = base64::engine::general_purpose::STANDARD;
    let svg_total = out
        .artifacts
        .iter()
        .fold(0usize, |n, a| n.saturating_add(a.byte_len));
    let svg_as_text = svg_total <= svg_text_max;
    let mut blocks = Vec::with_capacity(out.artifacts.len() + 1);
    let mut summaries = Vec::with_capacity(out.artifacts.len());
    for a in &out.artifacts {
        let uri = render_uri(&id, a.page);
        let blob =
            || ResourceContents::blob(a.data_base64.clone(), uri.clone()).with_mime_type(mime);
        let (block, returned_as) = match out.format {
            OutputFormat::Png => (
                ContentBlock::image(a.data_base64.clone(), mime),
                ReturnedAs::Image,
            ),
            OutputFormat::Svg if svg_as_text => match b64
                .decode(&a.data_base64)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
            {
                Some(svg) => (
                    ContentBlock::resource(
                        ResourceContents::text(svg, uri.clone()).with_mime_type(mime),
                    ),
                    ReturnedAs::Text,
                ),
                None => (ContentBlock::resource(blob()), ReturnedAs::Blob),
            },
            OutputFormat::Svg | OutputFormat::Pdf => {
                (ContentBlock::resource(blob()), ReturnedAs::Blob)
            }
        };
        summaries.push(ArtifactSummary {
            page: a.page,
            width_px: a.width_px,
            height_px: a.height_px,
            width_pt: a.width_pt,
            height_pt: a.height_pt,
            dpi: a.dpi,
            bytes: a.byte_len,
            mime_type: mime.to_string(),
            returned_as,
        });
        blocks.push(block);
    }
    let renderer = RendererIdentity::current();
    let mut text = success_text(&out, &renderer);
    if out.format == OutputFormat::Svg && !svg_as_text {
        text.push_str(&format!(
            "\nThe SVG is over {svg_text_max} bytes (PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES), so each page is a \
             blob resource (base64) rather than text."
        ));
    }
    blocks.push(ContentBlock::text(text));
    let envelope = RenderToolOutput {
        ok: true,
        format: Some(out.format.name().to_string()),
        mime_type: Some(mime.to_string()),
        page_count: Some(out.page_count),
        artifacts: Some(summaries),
        program_output: Some(out.program_output),
        program_output_truncated: Some(out.program_output_truncated),
        steps_used: Some(out.steps_used),
        elapsed_ms: Some(out.elapsed_ms),
        error: None,
        renderer,
    };
    with_structured(CallToolResult::success(blocks), &envelope)
}

fn success_text(out: &RenderOutput, renderer: &RendererIdentity) -> String {
    let total: usize = out.artifacts.iter().map(|a| a.byte_len).sum();
    let dims = out
        .artifacts
        .iter()
        .map(|a| match (a.width_pt, a.height_pt, a.dpi) {
            (Some(w), Some(h), Some(dpi)) => {
                format!(
                    "{w}x{h} pt at {dpi} dpi = {}x{} px",
                    a.width_px, a.height_px
                )
            }
            _ => format!("{}x{} px", a.width_px, a.height_px),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let pages = if out.page_count == 1 { "page" } else { "pages" };
    let mut text = format!(
        "Rendered {} {pages} as {} ({dims}, {total} bytes) with PSCat {} in {} ms.",
        out.page_count,
        out.format.name().to_uppercase(),
        renderer.version,
        out.elapsed_ms
    );
    if !out.program_output.is_empty() {
        text.push_str("\nProgram output:\n");
        text.push_str(&out.program_output);
        if out.program_output_truncated {
            text.push_str("\n[program output truncated]");
        }
    }
    text
}

fn render_failure(err: RenderError) -> CallToolResult {
    failure_with("Render failed", err)
}

/// The [`RenderToolOutput`] failure envelope, with `lead` starting the text.
fn failure_with(lead: &str, err: RenderError) -> CallToolResult {
    let text = error_text(lead, &err);
    let envelope = RenderToolOutput {
        ok: false,
        format: None,
        mime_type: None,
        page_count: None,
        artifacts: None,
        program_output: None,
        program_output_truncated: None,
        steps_used: None,
        elapsed_ms: None,
        error: Some(err),
        renderer: RendererIdentity::current(),
    };
    with_structured(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

fn sheet_success(out: RenderOutput) -> CallToolResult {
    let (Some(report), Some(png)) = (out.sheet, out.artifacts.first()) else {
        return sheet_failure(RenderError::new(
            ErrorCode::WorkerFailed,
            "the renderer returned no sheet",
        ));
    };
    let renderer = RendererIdentity::current();
    let mut text = format!(
        "Composed a review sheet of {} cells ({} failed) as PNG ({}x{} px, {} bytes) with \
         PSCat {} in {} ms.",
        report.cells.len(),
        report.failed_cells,
        report.width_px,
        report.height_px,
        png.byte_len,
        renderer.version,
        out.wall_ms
    );
    for c in &report.cells {
        let zoom = if c.zoom > 1 {
            format!(", shown at {}x", c.zoom)
        } else {
            String::new()
        };
        let size = match (c.width_pt, c.height_pt) {
            (Some(w), Some(h)) => format!("{w}x{h} pt at {} dpi = ", c.dpi),
            _ => format!("{} dpi, ", c.dpi),
        };
        text.push_str(&format!(
            "\nrow {} column {}: {size}{}x{} px{zoom}, at ({}, {}): ",
            c.row, c.column, c.width_px, c.height_px, c.x, c.y
        ));
        match &c.error {
            None => text.push_str("ok"),
            Some(e) => {
                text.push_str(&code_name(e.code));
                if let Some(limit) = &e.limit {
                    text.push_str(&format!(" ({limit})"));
                }
                text.push_str(&format!(": {}", e.message));
            }
        }
    }
    let blocks = vec![
        ContentBlock::image(png.data_base64.clone(), OutputFormat::Png.mime_type()),
        ContentBlock::text(text),
    ];
    let envelope = SheetToolOutput {
        ok: true,
        format: Some(OutputFormat::Png.name().to_string()),
        mime_type: Some(OutputFormat::Png.mime_type().to_string()),
        width_px: Some(report.width_px),
        height_px: Some(report.height_px),
        bytes: Some(png.byte_len),
        failed_cells: Some(report.failed_cells),
        cells: Some(report.cells),
        steps_used: Some(out.steps_used),
        elapsed_ms: Some(out.elapsed_ms),
        wall_ms: Some(out.wall_ms),
        error: None,
        renderer,
    };
    with_structured(CallToolResult::success(blocks), &envelope)
}

fn sheet_failure(err: RenderError) -> CallToolResult {
    let text = error_text("Review sheet failed", &err);
    let envelope = SheetToolOutput {
        ok: false,
        format: None,
        mime_type: None,
        width_px: None,
        height_px: None,
        bytes: None,
        failed_cells: None,
        cells: None,
        steps_used: None,
        elapsed_ms: None,
        wall_ms: None,
        error: Some(err),
        renderer: RendererIdentity::current(),
    };
    with_structured(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

fn validate_failure(err: RenderError) -> CallToolResult {
    let text = error_text("Validation could not run", &err);
    let envelope = ValidateToolOutput {
        ok: false,
        report: None,
        error: Some(err),
        renderer: RendererIdentity::current(),
    };
    with_structured(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

/// Prefix of the message rmcp 3.9 puts in the text-only error result it
/// returns when a tool's arguments do not deserialize (`router/tool.rs`,
/// `into_tool_argument_error`). That happens before the tool body runs.
const RMCP_ARGUMENT_ERROR_PREFIX: &str = "failed to deserialize parameters:";

/// Give an argument deserialization failure (wrong type, negative width,
/// missing `source`) the tool's usual failure envelope with code
/// `invalid_request`. Any other result is returned unchanged.
fn with_argument_error_envelope(tool: &str, result: CallToolResult) -> CallToolResult {
    if result.is_error != Some(true) || result.structured_content.is_some() {
        return result;
    }
    let Some(detail) = result
        .content
        .iter()
        .find_map(|c| c.as_text())
        .and_then(|t| t.text.strip_prefix(RMCP_ARGUMENT_ERROR_PREFIX))
    else {
        return result;
    };
    // serde echoes the offending value back; keep a huge one out of the reply.
    const MAX_DETAIL_CHARS: usize = 300;
    let detail = detail.trim();
    let detail = match detail.char_indices().nth(MAX_DETAIL_CHARS) {
        Some((cut, _)) => format!("{}…", &detail[..cut]),
        None => detail.to_string(),
    };
    let err = RenderError::new(
        ErrorCode::InvalidRequest,
        format!("invalid arguments: {detail}"),
    );
    // A new tool that takes arguments needs an arm here, or its argument
    // errors come back as rmcp's bare text result, without the envelope.
    match tool {
        "render_postscript" => render_failure(err),
        "chalkboard_math" => failure_with("Chalkboard failed", err),
        "handwrite" => failure_with("Handwrite failed", err),
        "review_sheet" => sheet_failure(err),
        "validate_postscript" => validate_failure(err),
        "describe_art_capabilities" => capabilities_failure(err),
        "start_render" => job_failure("Job not started", err),
        "render_status" => job_failure("No job status", err),
        "render_result" => failure_with("No render result", err),
        "cancel_render" => job_failure("Job not cancelled", err),
        _ => result,
    }
}

fn capabilities_failure(err: RenderError) -> CallToolResult {
    let text = error_text("Capabilities could not be listed", &err);
    let envelope = CapabilitiesToolOutput {
        ok: false,
        catalog: None,
        error: Some(err),
        renderer: RendererIdentity::current(),
    };
    with_structured(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        &envelope,
    )
}

fn error_text(lead: &str, err: &RenderError) -> String {
    let mut text = format!("{lead} ({}): {}", code_name(err.code), err.message);
    if let Some(input) = &err.input {
        text.push_str(&format!("\nInput error: {}", input.code));
        if let Some(token) = &input.token {
            text.push_str(&format!(" ({token})"));
        }
    }
    if let Some(output) = &err.program_output {
        text.push_str("\nProgram output before the error:\n");
        text.push_str(output);
    }
    text
}

fn validation_text(report: &ValidationReport) -> String {
    let mut text = if report.valid {
        format!(
            "Valid: the program ran without error and produced {} page(s).",
            report.page_count
        )
    } else {
        "Not valid.".to_string()
    };
    if let Some(err) = &report.syntax.error {
        text.push_str(&format!("\nSyntax: {err}"));
    }
    if let Some(diag) = &report.execution_error {
        text.push_str(&format!("\nPostScript error: {}", diag.report.trim_end()));
    }
    for lint in &report.lint {
        text.push_str(&format!("\nLint [{}]: {}", lint.check, lint.message));
    }
    if !report.program_output.is_empty() {
        text.push_str("\nProgram output:\n");
        text.push_str(&report.program_output);
    }
    text
}
