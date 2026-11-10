//! MCP tool definitions (rmcp). Thin: every tool delegates to
//! [`RenderService`](crate::render::RenderService) and only translates
//! arguments in and results out.
//!
//! Tool result shapes:
//!
//! * `render_postscript` — on success, one content block per artifact (PNG
//!   as `image` content; SVG as an embedded text resource; PDF as an
//!   embedded blob resource) plus a short text summary. `structuredContent`
//!   is a [`RenderToolOutput`] envelope without the artifact bytes. On
//!   failure, `isError: true`, a text explanation, and the same envelope
//!   with `ok: false` and `error` set; never an image.
//! * `validate_postscript` — a [`ValidateToolOutput`] envelope (the
//!   [`ValidationReport`] fields at the top level) plus a text summary.
//! * `get_renderer_info` — a [`RendererInfo`] object.

use std::sync::Arc;

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::build_info::{RendererIdentity, SERVER_VERSION};
use crate::config::{MAX_DPI, MIN_DPI, PSCAT_MAX_SIDE_PX};
use crate::render::{
    DEFAULT_PAGE_POINTS, ErrorCode, RenderError, RenderOutput, RenderRequest, RenderService,
    ValidationReport,
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
    /// Size of the artifact in bytes (before base64).
    pub bytes: usize,
    /// MIME type of the artifact.
    pub mime_type: String,
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
    /// What a program can and cannot reach.
    pub capabilities: Vec<String>,
}

/// The MCP server handler shared by the stdio and HTTP transports.
#[derive(Clone)]
pub struct PscatMcp {
    service: Arc<RenderService>,
    tool_router: ToolRouter<Self>,
}

impl PscatMcp {
    /// A handler over `service`.
    pub fn new(service: Arc<RenderService>) -> Self {
        let mut tool_router = Self::tool_router();
        let limits = service.limits();
        if let Some(route) = tool_router.map.get_mut("render_postscript") {
            let base = route.attr.description.clone().unwrap_or_default();
            route.attr.description = Some(
                format!(
                    "{base} Limits on this server: source up to {} bytes; at most {} pixels per \
                     page and {PSCAT_MAX_SIDE_PX} pixels a side; at most {} pages; {} ms per render.",
                    limits.max_source_bytes,
                    limits.max_pixels,
                    limits.max_pages,
                    limits.render_timeout_ms
                )
                .into(),
            );
        }
        PscatMcp {
            service,
            tool_router,
        }
    }

    /// The underlying service.
    pub fn service(&self) -> &Arc<RenderService> {
        &self.service
    }

    fn renderer_info(&self) -> RendererInfo {
        let l = self.service.limits();
        let returned_as = |f: OutputFormat| match f {
            OutputFormat::Png => "one image content block per page",
            OutputFormat::Svg => "one embedded text resource per page",
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
                render_timeout_ms: l.render_timeout_ms,
                max_pixels: l.max_pixels,
                max_side_px: PSCAT_MAX_SIDE_PX,
                max_pages: l.max_pages,
                max_output_bytes: l.max_output_bytes,
                max_program_output_bytes: l.max_program_output_bytes,
                min_dpi: MIN_DPI,
                max_dpi: MAX_DPI,
                max_concurrent_renders: l.concurrency(),
                default_page_points: [DEFAULT_PAGE_POINTS.0, DEFAULT_PAGE_POINTS.1],
            },
            capabilities: vec![
                "File reads are confined to PSCat's built-in lib/ directory (for example \
                 `(lib/artkit.ps) run`); other paths fail with invalidfileaccess."
                    .into(),
                "Programs cannot write files.".into(),
                "Programs have no network access.".into(),
                "Each render runs in its own short-lived worker process with step, time and \
                 memory limits."
                    .into(),
            ],
        }
    }
}

/// All three tools only read their arguments and run the program in a
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
                       short text summary and the program's stdout. The program runs in a \
                       sandbox: reading files is refused except for PSCat's built-in lib/ \
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
        let req = RenderRequest {
            source: args.source,
            format: args.format,
            width: args.width,
            height: args.height,
            dpi: args.dpi,
        };
        let outcome = tokio::select! {
            r = self.service.render(req) => r,
            () = ct.cancelled() => Err(cancelled()),
        };
        match outcome {
            Ok(out) => render_success(out),
            Err(err) => render_failure(err),
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
        name = "get_renderer_info",
        title = "PSCat renderer info",
        description = "Reports the PSCat renderer version, this server's version, the supported \
                       output formats with their MIME types, the limits in force (source size, \
                       timeout, pixels, pages, output size, dpi range, concurrency) and what \
                       programs can access. Runs no PostScript.",
        annotations(
            title = "PSCat renderer info",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_renderer_info(&self) -> Json<RendererInfo> {
        Json(self.renderer_info())
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
        let tcc = ToolCallContext::new(self, request, context);
        Ok(match self.tool_router.call(tcc).await? {
            CallToolResponse::Complete(result) => {
                CallToolResponse::Complete(with_argument_error_envelope(&tool, result))
            }
            other => other,
        })
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("pscat-mcp", SERVER_VERSION))
            .with_instructions(
                "Renders PostScript programs to PNG, SVG or PDF with the PSCat interpreter, and \
                 validates programs without rendering. Programs run in a sandbox with no file \
                 writes, no network, and file reads limited to PSCat's built-in lib/.",
            )
    }
}

/// Serve MCP over stdin/stdout until the client disconnects.
pub async fn serve_stdio(service: Arc<RenderService>) -> anyhow::Result<()> {
    let running = PscatMcp::new(service)
        .serve(rmcp::transport::stdio())
        .await?;
    let reason = running.waiting().await?;
    tracing::info!(?reason, "stdio session ended");
    Ok(())
}

fn cancelled() -> RenderError {
    RenderError {
        code: ErrorCode::Cancelled,
        message: "the request was cancelled".into(),
        postscript: None,
        program_output: None,
    }
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

fn render_success(out: RenderOutput) -> CallToolResult {
    let mime = out.format.mime_type();
    let id = uuid::Uuid::new_v4();
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut blocks = Vec::with_capacity(out.artifacts.len() + 1);
    let mut summaries = Vec::with_capacity(out.artifacts.len());
    for a in &out.artifacts {
        summaries.push(ArtifactSummary {
            page: a.page,
            width_px: a.width_px,
            height_px: a.height_px,
            bytes: a.byte_len,
            mime_type: mime.to_string(),
        });
        let uri = render_uri(&id, a.page);
        let block = match out.format {
            OutputFormat::Png => ContentBlock::image(a.data_base64.clone(), mime),
            OutputFormat::Svg => match b64
                .decode(&a.data_base64)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
            {
                Some(svg) => {
                    ContentBlock::resource(ResourceContents::text(svg, uri).with_mime_type(mime))
                }
                None => ContentBlock::resource(
                    ResourceContents::blob(a.data_base64.clone(), uri).with_mime_type(mime),
                ),
            },
            OutputFormat::Pdf => ContentBlock::resource(
                ResourceContents::blob(a.data_base64.clone(), uri).with_mime_type(mime),
            ),
        };
        blocks.push(block);
    }
    let renderer = RendererIdentity::current();
    blocks.push(ContentBlock::text(success_text(&out, &renderer)));
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
        .map(|a| format!("{}x{}", a.width_px, a.height_px))
        .collect::<Vec<_>>()
        .join(", ");
    let pages = if out.page_count == 1 { "page" } else { "pages" };
    let mut text = format!(
        "Rendered {} {pages} as {} ({dims} px, {total} bytes) with PSCat {} in {} ms.",
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
    let text = error_text("Render failed", &err);
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
    let err = RenderError {
        code: ErrorCode::InvalidRequest,
        message: format!("invalid arguments: {detail}"),
        postscript: None,
        program_output: None,
    };
    match tool {
        "render_postscript" => render_failure(err),
        "validate_postscript" => validate_failure(err),
        _ => result,
    }
}

fn error_text(lead: &str, err: &RenderError) -> String {
    let mut text = format!("{lead} ({}): {}", code_name(err.code), err.message);
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
