//! The transport-independent rendering service.
//!
//! [`RenderService`] owns request validation, the concurrency bound and the
//! backend. MCP tools (and a future REST API) call it; nothing here knows
//! about MCP or HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::Semaphore;

use crate::backend::{BackendError, RenderBackend};
use crate::config::{LimitsConfig, MAX_DPI, MIN_DPI, PSCAT_MAX_SIDE_PX};
use crate::worker::{
    Artifact, InterpLimits, JobMode, LintNote, OutputFormat, PsDiagnostic, SyntaxScan,
    WorkerFailure, WorkerJob, WorkerResult,
};

/// PSCat's default page, used when no size is given.
pub const DEFAULT_PAGE_POINTS: (u32, u32) = (612, 792);

/// A render or validation request as any transport receives it.
#[derive(Debug, Clone, Default)]
pub struct RenderRequest {
    /// PostScript program text.
    pub source: String,
    /// Requested format name; `None` means PNG.
    pub format: Option<String>,
    /// Page width in points.
    pub width: Option<u32>,
    /// Page height in points.
    pub height: Option<u32>,
    /// Resolution; `None` means 72 dpi (one pixel per point).
    pub dpi: Option<f64>,
}

/// A request that passed validation. Only these reach the backend.
#[derive(Debug, Clone)]
pub struct ValidatedRequest {
    job: WorkerJob,
    /// Device pixels of the starting page.
    pub page_px: (u32, u32),
}

/// Stable error codes. Clients may match on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Missing or malformed argument.
    InvalidRequest,
    /// Source over the configured byte limit.
    SourceTooLarge,
    /// Format PSCat cannot produce.
    UnsupportedFormat,
    /// Page size or dpi out of range.
    InvalidDimensions,
    /// Page over the pixel limit.
    TooManyPixels,
    /// The program raised a PostScript error.
    PostscriptError,
    /// PSCat stopped the program at a step, time or memory limit.
    LimitExceeded,
    /// The render passed the wall-clock deadline and was killed.
    Timeout,
    /// The program produced more pages than allowed.
    TooManyPages,
    /// The rendered output is larger than can be returned inline.
    OutputTooLarge,
    /// No render slot became free in time.
    ServerBusy,
    /// The request was cancelled by the client.
    Cancelled,
    /// The worker process failed (crash, OS limit, bad output).
    WorkerFailed,
}

/// A structured failure. `message` is written for the person or agent that
/// made the request.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RenderError {
    /// Stable code.
    pub code: ErrorCode,
    /// Human-readable explanation.
    pub message: String,
    /// The PostScript error, for `postscript_error` and `limit_exceeded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postscript: Option<Box<PsDiagnostic>>,
    /// Program stdout before the failure, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_output: Option<String>,
}

impl RenderError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        RenderError {
            code,
            message: message.into(),
            postscript: None,
            program_output: None,
        }
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RenderError {}

/// A successful render.
#[derive(Debug, Clone)]
pub struct RenderOutput {
    /// Format produced.
    pub format: OutputFormat,
    /// One artifact per page (PNG, SVG) or one document (PDF).
    pub artifacts: Vec<Artifact>,
    /// Pages the program produced.
    pub page_count: usize,
    /// Program stdout, possibly truncated.
    pub program_output: String,
    /// Whether `program_output` was cut.
    pub program_output_truncated: bool,
    /// Interpreter steps.
    pub steps_used: u64,
    /// Time inside the interpreter.
    pub elapsed_ms: u64,
    /// Time including process start and encoding.
    pub wall_ms: u64,
}

/// Diagnostics from `validate`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ValidationReport {
    /// True when the scan passed and the program ran without error.
    pub valid: bool,
    /// Tokenizer pass that executes nothing.
    pub syntax: SyntaxReport,
    /// The error the program raised when executed, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_error: Option<PsDiagnostic>,
    /// PSCat lint findings (heuristics: blank page, stack leak, gsave
    /// imbalance). Advisory only.
    pub lint: Vec<LintReport>,
    /// Pages the program produced.
    pub page_count: usize,
    /// Program stdout, possibly truncated.
    pub program_output: String,
    /// Interpreter steps.
    pub steps_used: u64,
}

/// Serializable copy of [`SyntaxScan`].
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SyntaxReport {
    /// No tokenizer error and balanced braces.
    pub ok: bool,
    /// Tokens read.
    pub tokens: usize,
    /// Problem found, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Serializable copy of [`LintNote`].
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LintReport {
    /// Check slug.
    pub check: String,
    /// Explanation.
    pub message: String,
}

impl From<SyntaxScan> for SyntaxReport {
    fn from(s: SyntaxScan) -> Self {
        SyntaxReport {
            ok: s.ok,
            tokens: s.tokens,
            error: s.error,
        }
    }
}

impl From<LintNote> for LintReport {
    fn from(l: LintNote) -> Self {
        LintReport {
            check: l.check,
            message: l.message,
        }
    }
}

/// Counters exposed for health and tests. Low-cardinality by design.
#[derive(Debug, Default)]
pub struct Counters {
    /// Requests rejected before execution.
    pub rejected: AtomicU64,
    /// Jobs handed to the backend.
    pub executed: AtomicU64,
    /// Jobs that hit the external deadline.
    pub timed_out: AtomicU64,
}

/// Device pixels for `points` at `dpi`, the way PSCat rounds them.
pub fn device_px(points: u32, dpi: f64) -> u32 {
    (((points as f32) * (dpi / 72.0) as f32).round() as u32).max(1)
}

/// Check a request against the limits without executing anything.
pub fn validate_request(
    req: &RenderRequest,
    mode: JobMode,
    limits: &LimitsConfig,
) -> Result<ValidatedRequest, RenderError> {
    if req.source.trim().is_empty() {
        return Err(RenderError::new(
            ErrorCode::InvalidRequest,
            "source is empty",
        ));
    }
    if req.source.len() > limits.max_source_bytes {
        return Err(RenderError::new(
            ErrorCode::SourceTooLarge,
            format!(
                "source is {} bytes; the limit is {} bytes",
                req.source.len(),
                limits.max_source_bytes
            ),
        ));
    }
    let format = match req.format.as_deref() {
        None | Some("") => OutputFormat::Png,
        Some(name) => OutputFormat::parse(name).ok_or_else(|| {
            RenderError::new(
                ErrorCode::UnsupportedFormat,
                format!("format {name:?} is not supported; PSCat renders png, svg or pdf"),
            )
        })?,
    };
    let dpi = req.dpi.unwrap_or(72.0);
    if !dpi.is_finite() || !(MIN_DPI..=MAX_DPI).contains(&dpi) {
        return Err(RenderError::new(
            ErrorCode::InvalidDimensions,
            format!("dpi must be between {MIN_DPI} and {MAX_DPI}, got {dpi}"),
        ));
    }
    let page_points = match (req.width, req.height) {
        (None, None) => None,
        (w, h) => Some((
            w.unwrap_or(DEFAULT_PAGE_POINTS.0),
            h.unwrap_or(DEFAULT_PAGE_POINTS.1),
        )),
    };
    let (wp, hp) = page_points.unwrap_or(DEFAULT_PAGE_POINTS);
    if wp == 0 || hp == 0 {
        return Err(RenderError::new(
            ErrorCode::InvalidDimensions,
            "width and height must be at least 1 point",
        ));
    }
    let (wx, hx) = (device_px(wp, dpi), device_px(hp, dpi));
    if wx > PSCAT_MAX_SIDE_PX || hx > PSCAT_MAX_SIDE_PX {
        return Err(RenderError::new(
            ErrorCode::InvalidDimensions,
            format!(
                "page is {wx}x{hx} pixels at {dpi} dpi; PSCat allows at most {PSCAT_MAX_SIDE_PX} pixels a side"
            ),
        ));
    }
    let pixels = u64::from(wx) * u64::from(hx);
    if pixels > limits.max_pixels {
        return Err(RenderError::new(
            ErrorCode::TooManyPixels,
            format!(
                "page is {wx}x{hx} = {pixels} pixels; the limit is {} pixels (lower the size or dpi)",
                limits.max_pixels
            ),
        ));
    }
    Ok(ValidatedRequest {
        page_px: (wx, hx),
        job: WorkerJob {
            mode,
            source: req.source.clone(),
            format,
            page_points,
            dpi,
            limits: InterpLimits {
                max_steps: limits.steps(),
                max_time_ms: limits.interp_time_limit().map(|d| d.as_millis() as u64),
                max_memory_bytes: limits.memory(),
                max_page_pixels: limits.max_pixels,
                max_pages: limits.max_pages,
                max_output_bytes: limits.max_output_bytes,
                max_program_output_bytes: limits.max_program_output_bytes,
            },
        },
    })
}

/// The rendering service shared by every transport.
pub struct RenderService {
    backend: Arc<dyn RenderBackend>,
    limits: LimitsConfig,
    slots: Arc<Semaphore>,
    /// Counters for health and tests.
    pub counters: Arc<Counters>,
}

impl RenderService {
    /// A service running jobs on `backend` under `limits`.
    pub fn new(backend: Arc<dyn RenderBackend>, limits: LimitsConfig) -> Self {
        let slots = Arc::new(Semaphore::new(limits.concurrency()));
        RenderService {
            backend,
            limits,
            slots,
            counters: Arc::new(Counters::default()),
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> &LimitsConfig {
        &self.limits
    }

    /// Render a program.
    pub async fn render(&self, req: RenderRequest) -> Result<RenderOutput, RenderError> {
        let validated = self.check(&req, JobMode::Render)?;
        let format = validated.job.format;
        let started = Instant::now();
        let result = self.run(validated).await?;
        let wall_ms = started.elapsed().as_millis() as u64;
        if let Some(failure) = result.failure.clone() {
            return Err(failure_to_error(failure, result));
        }
        if result.artifacts.is_empty() {
            return Err(RenderError::new(
                ErrorCode::WorkerFailed,
                "the renderer returned no output",
            ));
        }
        Ok(RenderOutput {
            format,
            artifacts: result.artifacts,
            page_count: result.page_count,
            program_output: result.program_output,
            program_output_truncated: result.program_output_truncated,
            steps_used: result.steps_used,
            elapsed_ms: result.elapsed_ms,
            wall_ms,
        })
    }

    /// Scan and execute a program, returning diagnostics and no image.
    pub async fn validate(&self, source: String) -> Result<ValidationReport, RenderError> {
        let req = RenderRequest {
            source,
            ..RenderRequest::default()
        };
        let validated = self.check(&req, JobMode::Validate)?;
        let result = self.run(validated).await?;
        match &result.failure {
            None | Some(WorkerFailure::PostScript) => {}
            Some(other) => return Err(failure_to_error(other.clone(), result)),
        }
        let syntax: SyntaxReport = result.syntax.map(Into::into).unwrap_or(SyntaxReport {
            ok: false,
            tokens: 0,
            error: Some("the worker returned no syntax scan".into()),
        });
        Ok(ValidationReport {
            valid: syntax.ok && result.ps_error.is_none(),
            syntax,
            execution_error: result.ps_error,
            lint: result.lint.into_iter().map(Into::into).collect(),
            page_count: result.page_count,
            program_output: result.program_output,
            steps_used: result.steps_used,
        })
    }

    fn check(&self, req: &RenderRequest, mode: JobMode) -> Result<ValidatedRequest, RenderError> {
        validate_request(req, mode, &self.limits).inspect_err(|e| {
            self.counters.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::info!(code = ?e.code, "request rejected before execution");
        })
    }

    async fn run(&self, validated: ValidatedRequest) -> Result<WorkerResult, RenderError> {
        let permit = tokio::time::timeout(
            self.limits.queue_timeout(),
            self.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| {
            RenderError::new(
                ErrorCode::ServerBusy,
                "all render slots are busy; try again shortly",
            )
        })?
        .map_err(|_| RenderError::new(ErrorCode::ServerBusy, "the server is shutting down"))?;
        self.counters.executed.fetch_add(1, Ordering::Relaxed);
        let source_len = validated.job.source.len();
        let outcome = self
            .backend
            .execute(validated.job, self.limits.render_timeout())
            .await;
        drop(permit);
        match outcome {
            Ok(r) => {
                tracing::info!(
                    source_bytes = source_len,
                    steps = r.steps_used,
                    elapsed_ms = r.elapsed_ms,
                    pages = r.page_count,
                    failed = r.failure.is_some(),
                    "render finished"
                );
                Ok(r)
            }
            Err(BackendError::Timeout) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(source_bytes = source_len, "render killed at the deadline");
                Err(RenderError::new(
                    ErrorCode::Timeout,
                    format!(
                        "the render ran past {} ms and was stopped",
                        self.limits.render_timeout_ms
                    ),
                ))
            }
            Err(e) => {
                tracing::error!(error = %e, "render worker failed");
                Err(RenderError::new(ErrorCode::WorkerFailed, e.to_string()))
            }
        }
    }
}

fn failure_to_error(failure: WorkerFailure, result: WorkerResult) -> RenderError {
    let program_output = Some(result.program_output).filter(|s| !s.is_empty());
    match failure {
        WorkerFailure::PostScript => {
            let diag = result.ps_error.unwrap_or(PsDiagnostic {
                name: "unknown".into(),
                limit: None,
                report: String::new(),
            });
            let (code, message) = match &diag.limit {
                Some(limit) => (
                    ErrorCode::LimitExceeded,
                    format!("the program was stopped at the {limit} limit"),
                ),
                None => (
                    ErrorCode::PostscriptError,
                    format!("PostScript error: {}", diag.report),
                ),
            };
            RenderError {
                code,
                message,
                postscript: Some(Box::new(diag)),
                program_output,
            }
        }
        WorkerFailure::TooManyPages { pages, max } => RenderError::new(
            ErrorCode::TooManyPages,
            format!("the program produced {pages} pages; at most {max} are returned"),
        ),
        WorkerFailure::OutputTooLarge { bytes, max } => RenderError::new(
            ErrorCode::OutputTooLarge,
            format!(
                "the rendered output is over {max} bytes (reached {bytes}); lower the page size or dpi, \
                 or use svg for vector artwork. No artifact storage is configured, so output is only returned inline"
            ),
        ),
        WorkerFailure::Encode { message } => RenderError::new(
            ErrorCode::WorkerFailed,
            format!("encoding failed: {message}"),
        ),
        WorkerFailure::BadJob { message } => RenderError::new(ErrorCode::WorkerFailed, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(src: &str) -> RenderRequest {
        RenderRequest {
            source: src.into(),
            ..RenderRequest::default()
        }
    }

    #[test]
    fn rejects_before_execution() {
        let limits = LimitsConfig {
            max_source_bytes: 10,
            ..LimitsConfig::default()
        };
        let code = |r: RenderRequest| {
            validate_request(&r, JobMode::Render, &limits)
                .unwrap_err()
                .code
        };
        assert_eq!(code(req("")), ErrorCode::InvalidRequest);
        assert_eq!(code(req("01234567890")), ErrorCode::SourceTooLarge);
        assert_eq!(
            code(RenderRequest {
                format: Some("jpeg".into()),
                ..req("showpage")
            }),
            ErrorCode::UnsupportedFormat
        );
        assert_eq!(
            code(RenderRequest {
                width: Some(0),
                ..req("showpage")
            }),
            ErrorCode::InvalidDimensions
        );
        assert_eq!(
            code(RenderRequest {
                width: Some(9000),
                height: Some(10),
                ..req("showpage")
            }),
            ErrorCode::InvalidDimensions
        );
        assert_eq!(
            code(RenderRequest {
                dpi: Some(5000.0),
                ..req("showpage")
            }),
            ErrorCode::InvalidDimensions
        );
        assert_eq!(
            code(RenderRequest {
                dpi: Some(f64::NAN),
                ..req("showpage")
            }),
            ErrorCode::InvalidDimensions
        );
        assert_eq!(
            code(RenderRequest {
                width: Some(5000),
                height: Some(5000),
                ..req("showpage")
            }),
            ErrorCode::TooManyPixels
        );
    }

    #[test]
    fn accepts_and_sizes_pages() {
        let limits = LimitsConfig::default();
        let v = validate_request(
            &RenderRequest {
                width: Some(200),
                height: Some(100),
                dpi: Some(144.0),
                format: Some("SVG".into()),
                ..req("showpage")
            },
            JobMode::Render,
            &limits,
        )
        .unwrap();
        assert_eq!(v.page_px, (400, 200));
        assert_eq!(v.job.format, OutputFormat::Svg);
        let d = validate_request(&req("showpage"), JobMode::Render, &limits).unwrap();
        assert_eq!(d.page_px, (612, 792));
        assert!(d.job.page_points.is_none());
    }
}
