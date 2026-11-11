//! The transport-independent rendering service.
//!
//! [`RenderService`] owns request validation, the concurrency bound and the
//! backend. MCP tools (and a future REST API) call it; nothing here knows
//! about MCP or HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::backend::{BackendError, RenderBackend};
use crate::config::{JobsConfig, LimitsConfig, MAX_DPI, MIN_DPI, PSCAT_MAX_SIDE_PX};
use crate::generate::{GenerateJob, PlannedPage};
use crate::jobs::{JobStatus, JobStore};
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
    /// Screen the raster like a mono laser printer (PNG only).
    pub halftone: bool,
}

/// A request for a generated program (see [`crate::generate`]), as any
/// transport receives it.
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    /// What to generate.
    pub job: GenerateJob,
    /// Requested format name; `None` means PNG.
    pub format: Option<String>,
}

/// What an async job runs: a render, or a generated program.
#[derive(Debug, Clone)]
pub enum JobRequest {
    /// `render_postscript`.
    Render(RenderRequest),
    /// `chalkboard_math` or `handwrite` (not `review_sheet`, whose plan
    /// shares out the sheet deadline and so cannot take a job's).
    Generate(GenerateRequest),
}

/// A request that passed validation. Only these reach the backend.
#[derive(Debug, Clone)]
pub struct ValidatedRequest {
    job: WorkerJob,
    /// Device pixels of the starting page.
    pub page_px: (u32, u32),
}

/// Stable error codes. Clients may match on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// No such async job for this client: never started by it, or expired.
    UnknownJob,
    /// The async job is still queued or running; it has no result yet.
    JobNotFinished,
    /// The async job results held in memory would pass the server's budget.
    ResultsBudgetExceeded,
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
    /// What was wrong with a generator tool's input (`chalkboard_math`'s
    /// LaTeX or board config), when the generator named it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Box<InputDiagnostic>>,
}

/// The input or budget a failure names: the input error a generator
/// reported, or the limit that refused a request.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct InputDiagnostic {
    /// The stable code: for `chalkboard_math`, `unsupported_command`,
    /// `unsupported_character`, `unsupported_environment`,
    /// `missing_argument`, `config`, `empty`, ...; for a size limit,
    /// `output_bytes`; for the async results budget,
    /// `results_per_principal_bytes` or `results_budget_bytes`.
    pub code: String,
    /// The command, character or environment the error names, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// For a size limit (`output_bytes`, the results budget), the limit in
    /// bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    /// For a size limit, the size reached in bytes (for SVG, all pages
    /// together; for the results budget, what the job would have held).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reached_bytes: Option<u64>,
}

impl RenderError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        RenderError {
            code,
            message: message.into(),
            postscript: None,
            program_output: None,
            input: None,
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
    /// What a review sheet holds, for `review_sheet`.
    pub sheet: Option<crate::sheet::SheetReport>,
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

/// PSCat's catalog of the art capabilities a program can use, as returned
/// by `describe_art_capabilities`. Mirrors `pscat --capabilities`, minus the
/// host filesystem paths.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtCatalog {
    /// PSCat version that produced the catalog.
    pub pscat_version: String,
    /// Fingerprint of which capabilities are available. A change here or in
    /// `pscat_version` means the set changed.
    pub catalog_signature: String,
    /// Installed font packs.
    pub font_packs: FontPacks,
    /// Every capability, sorted by name then kind.
    pub capabilities: Vec<ArtCapability>,
}

/// One capability: a font, Type 3 face, palette, page template, procedure,
/// native operator or config variable.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtCapability {
    /// Name as a program uses it.
    pub name: String,
    /// One of `font`, `type3_face`, `palette`, `template`, `procedure`,
    /// `operator`, `dial`.
    pub kind: String,
    /// What it is or does.
    pub description: String,
    /// Named keys of the options dict, for dict-driven procedures and
    /// templates; empty for positional ones.
    pub parameters: Vec<ArtParameter>,
    /// Where it is defined (`lib/artkit.ps`, `systemdict`, `fonts/`, ...).
    pub source: String,
    /// The `run` sequence that brings it into scope before `example`; empty
    /// when nothing needs loading.
    pub load: String,
    /// A usage example or the stack effect.
    pub example: String,
    /// Where it resolves (`builtin`, `catalog ...`, or a library file).
    pub availability: String,
}

/// One named key of a capability's options dict.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtParameter {
    /// Key name.
    pub name: String,
    /// Meaning and range.
    pub description: String,
    /// Default value as PostScript text, when it has one.
    pub default: Option<String>,
}

/// Font packs known to the linked PSCat build.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FontPacks {
    /// The font-pack release this build pins, if any.
    pub pinned_index: Option<PinnedIndex>,
    /// Installed packs. Their directories are not reported.
    pub installed: Vec<InstalledFontPack>,
    /// Font directories chosen explicitly, which take precedence over
    /// packs. Their locations are not reported.
    pub overrides: Vec<FontOverride>,
    /// Names of the packs that can be installed.
    pub available: Vec<String>,
}

/// The pinned font-pack release.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PinnedIndex {
    /// Release tag.
    pub tag: String,
    /// SHA-256 of the pack index.
    pub sha256: String,
}

/// An installed font pack.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InstalledFontPack {
    /// Pack name.
    pub name: String,
    /// Release tag it was installed from.
    pub tag: String,
    /// Whether it is the pinned release or user-selected.
    pub status: String,
    /// SHA-256 of the index it came from.
    pub index_sha256: Option<String>,
    /// SHA-256 of the pack archive.
    pub pack_sha256: String,
}

/// A font directory chosen explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FontOverride {
    /// Always `user-selected`.
    pub status: String,
}

/// Counters exposed for health and tests. Low-cardinality by design.
#[derive(Debug, Default)]
pub struct Counters {
    /// Requests rejected before execution.
    pub rejected: AtomicU64,
    /// Jobs handed to the backend.
    pub executed: AtomicU64,
    /// Jobs that ran out of time: killed at the external deadline, or a
    /// review sheet that stopped itself at its own deadline in the worker
    /// (`timeout`, `error.input.code` `deadline`). Both are a `timeout` to
    /// the client.
    pub timed_out: AtomicU64,
    /// Async jobs accepted.
    pub jobs_started: AtomicU64,
    /// Async jobs cancelled before they finished (a running one's worker
    /// killed), by `cancel_render`, `tasks/cancel` or shutdown.
    pub jobs_cancelled: AtomicU64,
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
    let format = parse_format(req.format.as_deref())?;
    if req.halftone && format != OutputFormat::Png {
        return Err(RenderError::new(
            ErrorCode::InvalidRequest,
            format!(
                "halftone applies to png output only; format {} keeps its vector art",
                format.name()
            ),
        ));
    }
    let dpi = req.dpi.unwrap_or(72.0);
    let page_points = match (req.width, req.height) {
        (None, None) => None,
        (w, h) => Some((
            w.unwrap_or(DEFAULT_PAGE_POINTS.0),
            h.unwrap_or(DEFAULT_PAGE_POINTS.1),
        )),
    };
    let page_px = check_page(page_points.unwrap_or(DEFAULT_PAGE_POINTS), dpi, limits)?;
    Ok(ValidatedRequest {
        page_px,
        job: WorkerJob {
            mode,
            source: req.source.clone(),
            format,
            page_points,
            dpi,
            halftone: req.halftone,
            limits: interp_limits(limits),
            generate: None,
            decided_height: None,
            sheet: None,
        },
    })
}

/// Check a generate request against the limits without generating or
/// executing anything: the generator's own checks ([`GenerateJob::plan`]),
/// then the format and the planned page, like a render.
pub fn validate_generate(
    req: &GenerateRequest,
    limits: &LimitsConfig,
) -> Result<ValidatedRequest, RenderError> {
    let plan = req.job.plan(limits)?;
    let format = parse_format(req.format.as_deref())?;
    // A fixed page is checked whole. When the generator decides the height,
    // the width is checked as a page one pixel tall (the least a page can
    // be) and the worker checks the whole page before it allocates it. A
    // sheet's plan has checked the sheet's own budgets.
    let (page_px, page_points, decided_height, sheet) = match plan.page {
        PlannedPage::Fixed { page_points } => (
            check_page(page_points, plan.dpi, limits)?,
            Some(page_points),
            None,
            None,
        ),
        PlannedPage::FixedWidth(budget) => {
            let px = check_page((budget.width_points, 1), plan.dpi, limits)?;
            (px, None, Some(budget), None)
        }
        PlannedPage::Sheet { bound_px, budget } => {
            if format != OutputFormat::Png {
                return Err(RenderError::new(
                    ErrorCode::UnsupportedFormat,
                    "a review sheet is a PNG",
                ));
            }
            (bound_px, None, None, Some(budget))
        }
    };
    Ok(ValidatedRequest {
        page_px,
        job: WorkerJob {
            mode: JobMode::Generate,
            source: String::new(),
            format,
            page_points,
            decided_height,
            dpi: plan.dpi,
            halftone: false,
            limits: interp_limits(limits),
            generate: Some(req.job.clone()),
            sheet,
        },
    })
}

fn parse_format(name: Option<&str>) -> Result<OutputFormat, RenderError> {
    match name {
        None | Some("") => Ok(OutputFormat::Png),
        Some(name) => OutputFormat::parse(name).ok_or_else(|| {
            RenderError::new(
                ErrorCode::UnsupportedFormat,
                format!("format {name:?} is not supported; PSCat renders png, svg or pdf"),
            )
        }),
    }
}

/// Check a page of `(wp, hp)` points at `dpi` against the dpi range, PSCat's
/// side limit and the pixel limit; returns its device pixels.
fn check_page(
    (wp, hp): (u32, u32),
    dpi: f64,
    limits: &LimitsConfig,
) -> Result<(u32, u32), RenderError> {
    if !dpi.is_finite() || !(MIN_DPI..=MAX_DPI).contains(&dpi) {
        return Err(RenderError::new(
            ErrorCode::InvalidDimensions,
            format!("dpi must be between {MIN_DPI} and {MAX_DPI}, got {dpi}"),
        ));
    }
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
    Ok((wx, hx))
}

fn interp_limits(limits: &LimitsConfig) -> InterpLimits {
    InterpLimits {
        max_steps: limits.steps(),
        max_time_ms: limits.interp_time_limit().map(|d| d.as_millis() as u64),
        max_memory_bytes: limits.memory(),
        max_page_pixels: limits.max_pixels,
        max_pages: limits.max_pages,
        max_output_bytes: limits.max_output_bytes,
        max_program_output_bytes: limits.max_program_output_bytes,
    }
}

/// The rendering service shared by every transport.
pub struct RenderService {
    backend: Arc<dyn RenderBackend>,
    limits: LimitsConfig,
    slots: Arc<Semaphore>,
    /// The art catalog, fetched from a worker on first use. A failed fetch
    /// leaves it empty, so the next call tries again.
    catalog: tokio::sync::OnceCell<Arc<ArtCatalog>>,
    /// Async jobs (off unless [`RenderService::with_jobs`] turned them on).
    jobs: JobStore,
    /// Counters for health and tests.
    pub counters: Arc<Counters>,
}

impl RenderService {
    /// A service running jobs on `backend` under `limits`.
    pub fn new(backend: Arc<dyn RenderBackend>, limits: LimitsConfig) -> Self {
        let slots = Arc::new(Semaphore::new(limits.concurrency()));
        RenderService {
            jobs: JobStore::new(JobsConfig::default(), &limits),
            backend,
            limits,
            slots,
            catalog: tokio::sync::OnceCell::new(),
            counters: Arc::new(Counters::default()),
        }
    }

    /// The same service serving async jobs with `jobs` (on only when
    /// `jobs.enabled`).
    pub fn with_jobs(mut self, jobs: JobsConfig) -> Self {
        self.jobs = JobStore::new(jobs, &self.limits);
        self
    }

    /// The async job store.
    pub fn jobs(&self) -> &JobStore {
        &self.jobs
    }

    /// The limits in force.
    pub fn limits(&self) -> &LimitsConfig {
        &self.limits
    }

    /// Render a program.
    pub async fn render(&self, req: RenderRequest) -> Result<RenderOutput, RenderError> {
        let validated = self.check(&req, JobMode::Render)?;
        self.render_job(validated.job).await
    }

    /// Generate a program from non-PostScript input and render it. The
    /// input is checked here ([`validate_generate`]); the generator and the
    /// program run in the worker, like a render.
    pub async fn generate(&self, req: GenerateRequest) -> Result<RenderOutput, RenderError> {
        let validated = self.rejected(validate_generate(&req, &self.limits))?;
        self.render_job(validated.job).await
    }

    async fn render_job(&self, job: WorkerJob) -> Result<RenderOutput, RenderError> {
        let started = Instant::now();
        // A review sheet's deadline covers all of its cells.
        let deadline = if job.sheet.is_some() {
            self.limits.sheet_timeout()
        } else {
            self.limits.render_timeout()
        };
        let permit = self.slot().await?;
        self.render_in_slot(permit, job, deadline, started).await
    }

    async fn render_in_slot(
        &self,
        permit: OwnedSemaphorePermit,
        job: WorkerJob,
        deadline: Duration,
        started: Instant,
    ) -> Result<RenderOutput, RenderError> {
        let format = job.format;
        let tool = job.generate.as_ref().map(|g| g.kind());
        let result = self.run_in_slot(permit, job, deadline).await?;
        let wall_ms = started.elapsed().as_millis() as u64;
        if let Some(WorkerFailure::Generate(f)) = &result.failure
            && f.code == crate::generate::GenerateErrorCode::Timeout
        {
            self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(wall_ms, "a review sheet ran out of its time in the worker");
        }
        if let Some(failure) = result.failure.clone() {
            return Err(failure_to_error(failure, result, format, tool));
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
            sheet: result.sheet,
        })
    }

    /// Accept an async job for `owner` (see [`crate::jobs`]): check the
    /// request under the job limits (the job deadline as the render
    /// timeout) and the job caps, then run it in the background. Returns
    /// its status, `queued`.
    pub fn start_job(
        self: &Arc<Self>,
        owner: &str,
        req: JobRequest,
    ) -> Result<JobStatus, RenderError> {
        if !self.jobs.enabled() {
            return Err(RenderError::new(
                ErrorCode::InvalidRequest,
                "async jobs are not enabled on this server",
            ));
        }
        let limits = self.jobs.config().job_limits(&self.limits);
        let (checked, tool) = match &req {
            JobRequest::Render(r) => (
                validate_request(r, JobMode::Render, &limits),
                "render_postscript",
            ),
            JobRequest::Generate(g) => (validate_generate(g, &limits), g.job.kind()),
        };
        let validated = self.rejected(checked)?;
        if validated.job.sheet.is_some() {
            return Err(RenderError::new(
                ErrorCode::InvalidRequest,
                "review_sheet does not run as an async job",
            ));
        }
        let (id, cancel) = self.jobs.insert(owner, tool)?;
        self.counters.jobs_started.fetch_add(1, Ordering::Relaxed);
        tracing::info!(tool, "async job accepted");
        // The status as accepted (`queued`), before the job can move on.
        let status = self.jobs.status(owner, &id);
        let service = Arc::clone(self);
        tokio::spawn(async move { service.run_async_job(id, validated.job, cancel).await });
        status
    }

    async fn run_async_job(
        self: Arc<Self>,
        id: String,
        job: WorkerJob,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        // Finishes the job as `worker_failed` if this task never reaches the
        // `finish` below: a panic, or the task dropped (the runtime shutting
        // down). The job then leaves the caps and keeps only its error
        // against the budget, and `unfinished` counts it no more.
        let mut guard = FinishOnDrop {
            service: Arc::clone(&self),
            id: id.clone(),
            armed: true,
        };
        let deadline = self.jobs.config().job_timeout();
        let work = async {
            // A running-job permit first, then a render slot shared with
            // every synchronous call; a queued job waits as long as it takes.
            let _running = self.jobs.running_permit().await.ok_or_else(shutting_down)?;
            let permit = self
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| shutting_down())?;
            self.jobs.mark_running(&id);
            self.render_in_slot(permit, job, deadline, Instant::now())
                .await
        };
        // Dropping `work` drops the backend's future, which kills the worker.
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            r = work => Some(r),
        };
        if outcome.is_none() {
            self.counters.jobs_cancelled.fetch_add(1, Ordering::Relaxed);
            tracing::info!("async job cancelled");
        }
        guard.armed = false;
        self.jobs.finish(&id, outcome);
    }

    /// Cancel `owner`'s job `id` and wait (briefly) for it to stop; returns
    /// its status. A finished job is left as it is.
    pub async fn cancel_job(&self, owner: &str, id: &str) -> Result<JobStatus, RenderError> {
        self.jobs.request_cancel(owner, id)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = self.jobs.status(owner, id)?;
            if status.state.is_finished() || Instant::now() >= deadline {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Shut the job store down: refuse new jobs and kill every running one,
    /// then wait (at most `wait`) for them to stop. Their results are lost.
    pub async fn shutdown_jobs(&self, wait: Duration) {
        if !self.jobs.enabled() {
            return;
        }
        self.jobs.close();
        let deadline = Instant::now() + wait;
        while self.jobs.unfinished() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tracing::info!(
            left = self.jobs.unfinished(),
            "async jobs stopped for shutdown"
        );
    }

    /// Scan and execute a program, returning diagnostics and no image.
    pub async fn validate(&self, source: String) -> Result<ValidationReport, RenderError> {
        let req = RenderRequest {
            source,
            ..RenderRequest::default()
        };
        let validated = self.check(&req, JobMode::Validate)?;
        let result = self
            .run(validated.job, self.limits.render_timeout())
            .await?;
        match &result.failure {
            None | Some(WorkerFailure::PostScript) => {}
            Some(other) => {
                return Err(failure_to_error(
                    other.clone(),
                    result,
                    OutputFormat::Png,
                    None,
                ));
            }
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

    /// PSCat's art catalog, read in a worker process so it reflects the
    /// environment renders run in (fonts, font packs), not the server's.
    ///
    /// The first successful call runs the worker (no PostScript; it takes a
    /// render slot and the render deadline) and the result is kept for the
    /// life of the service: the worker's environment is fixed, so the
    /// catalog is too. Later calls take no slot. Concurrent first calls share
    /// one fetch; a failed fetch is not kept.
    pub async fn capabilities(&self) -> Result<Arc<ArtCatalog>, RenderError> {
        self.catalog
            .get_or_try_init(|| async { self.fetch_catalog().await.map(Arc::new) })
            .await
            .cloned()
    }

    async fn fetch_catalog(&self) -> Result<ArtCatalog, RenderError> {
        let job = WorkerJob {
            mode: JobMode::Capabilities,
            source: String::new(),
            format: OutputFormat::Png,
            page_points: None,
            dpi: 72.0,
            halftone: false,
            limits: interp_limits(&self.limits),
            generate: None,
            decided_height: None,
            sheet: None,
        };
        let result = self.run(job, self.limits.render_timeout()).await?;
        if let Some(failure) = result.failure.clone() {
            return Err(failure_to_error(failure, result, OutputFormat::Png, None));
        }
        let value = result.capabilities.ok_or_else(|| {
            RenderError::new(
                ErrorCode::WorkerFailed,
                "the worker returned no capability catalog",
            )
        })?;
        serde_json::from_value(value).map_err(|e| {
            RenderError::new(
                ErrorCode::WorkerFailed,
                format!("the capability catalog has an unexpected shape: {e}"),
            )
        })
    }

    fn check(&self, req: &RenderRequest, mode: JobMode) -> Result<ValidatedRequest, RenderError> {
        self.rejected(validate_request(req, mode, &self.limits))
    }

    /// Count and log a request rejected before execution.
    fn rejected(
        &self,
        checked: Result<ValidatedRequest, RenderError>,
    ) -> Result<ValidatedRequest, RenderError> {
        checked.inspect_err(|e| {
            self.counters.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::info!(code = ?e.code, "request rejected before execution");
        })
    }

    /// Run `job` on the backend in a render slot, killing it after
    /// `deadline`.
    async fn run(&self, job: WorkerJob, deadline: Duration) -> Result<WorkerResult, RenderError> {
        let permit = self.slot().await?;
        self.run_in_slot(permit, job, deadline).await
    }

    /// A render slot, waiting at most the queue timeout.
    async fn slot(&self) -> Result<OwnedSemaphorePermit, RenderError> {
        tokio::time::timeout(
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
        .map_err(|_| shutting_down())
    }

    /// Run `job` in the slot `permit` holds, killing it after `deadline`.
    async fn run_in_slot(
        &self,
        permit: OwnedSemaphorePermit,
        job: WorkerJob,
        deadline: Duration,
    ) -> Result<WorkerResult, RenderError> {
        self.counters.executed.fetch_add(1, Ordering::Relaxed);
        let (source_len, generator) = match &job.generate {
            Some(g) => (g.input_len(), g.kind()),
            None => (job.source.len(), ""),
        };
        // One line per job handed to a worker; the server-layer evals count
        // these to assert how many renders a scenario executed.
        tracing::info!(source_bytes = source_len, generator, "render started");
        let what = if job.sheet.is_some() {
            "the sheet"
        } else {
            "the render"
        };
        let outcome = self.backend.execute(job, deadline).await;
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
                        "{what} ran past {} ms and was stopped",
                        deadline.as_millis()
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

/// See [`RenderService::run_async_job`].
struct FinishOnDrop {
    service: Arc<RenderService>,
    id: String,
    armed: bool,
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        if self.armed {
            tracing::error!("async job stopped before it finished; recorded as worker_failed");
            self.service.jobs.finish(
                &self.id,
                Some(Err(RenderError::new(
                    ErrorCode::WorkerFailed,
                    "the job stopped before it finished (an internal error, or the server \
                     shutting down)",
                ))),
            );
        }
    }
}

fn shutting_down() -> RenderError {
    RenderError::new(ErrorCode::ServerBusy, "the server is shutting down")
}

/// The error for a worker failure; `format` is the requested format, which
/// decides the advice for output that is too large, and `tool` the
/// generator tool (`chalkboard_math`, ...) that made the program, if any.
fn failure_to_error(
    failure: WorkerFailure,
    result: WorkerResult,
    format: OutputFormat,
    tool: Option<&str>,
) -> RenderError {
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
                input: None,
            }
        }
        WorkerFailure::Generate(f) => RenderError {
            code: f.code.into(),
            message: f.message,
            postscript: None,
            program_output,
            input: Some(Box::new(InputDiagnostic {
                code: f.detail,
                token: f.token,
                limit_bytes: None,
                reached_bytes: None,
            })),
        },
        WorkerFailure::TooManyPages { pages, max } => RenderError::new(
            ErrorCode::TooManyPages,
            format!("the program produced {pages} pages; at most {max} are returned"),
        ),
        WorkerFailure::OutputTooLarge { bytes, max } => {
            let advice = match format {
                OutputFormat::Png => "lower the page size or dpi, or use svg for vector artwork",
                OutputFormat::Svg if tool == Some("chalkboard_math") => {
                    "a chalkboard's SVG keeps every chalk grain as a vector mark and runs to \
                     megabytes, so png is the practical format (about 0.3 MB; pdf about 2.6 MB), \
                     or use a smaller board"
                }
                OutputFormat::Svg => {
                    "SVG draws text as glyph outlines and a gradient as many shapes, so it grows \
                     quickly: use png or pdf, lower the page size, or simplify the program"
                }
                OutputFormat::Pdf => "lower the page size or the amount of artwork, or use png",
            };
            let mut e = RenderError::new(
                ErrorCode::OutputTooLarge,
                format!(
                    "the rendered output is over {max} bytes (reached {bytes}; \
                     PSCAT_MCP_MAX_OUTPUT_BYTES); {advice}. No artifact storage is configured, \
                     so output is only returned inline"
                ),
            );
            e.input = Some(Box::new(InputDiagnostic {
                code: "output_bytes".into(),
                token: None,
                limit_bytes: Some(max as u64),
                reached_bytes: Some(bytes as u64),
            }));
            e
        }
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
    use crate::backend::RenderBackend;

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
    fn halftone_is_png_only() {
        let limits = LimitsConfig::default();
        for format in ["svg", "pdf"] {
            let err = validate_request(
                &RenderRequest {
                    format: Some(format.into()),
                    halftone: true,
                    ..req("showpage")
                },
                JobMode::Render,
                &limits,
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRequest, "{format}");
        }
        for format in [None, Some("png".to_string())] {
            let v = validate_request(
                &RenderRequest {
                    format,
                    halftone: true,
                    ..req("showpage")
                },
                JobMode::Render,
                &limits,
            )
            .unwrap();
            assert!(v.job.halftone);
        }
        let plain = validate_request(&req("showpage"), JobMode::Render, &limits).unwrap();
        assert!(!plain.job.halftone);
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

    /// A backend that answers every job with a fixed outcome and counts the
    /// jobs it was given.
    struct Fixed {
        outcome: fn() -> Result<WorkerResult, BackendError>,
        calls: Arc<AtomicU64>,
    }

    impl RenderBackend for Fixed {
        fn execute(
            &self,
            _job: WorkerJob,
            _deadline: std::time::Duration,
        ) -> crate::backend::BackendFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = (self.outcome)();
            Box::pin(async move { outcome })
        }
    }

    fn service_with(
        outcome: fn() -> Result<WorkerResult, BackendError>,
    ) -> (RenderService, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let backend = Fixed {
            outcome,
            calls: calls.clone(),
        };
        (
            RenderService::new(Arc::new(backend), LimitsConfig::default()),
            calls,
        )
    }

    async fn capabilities_with(
        outcome: fn() -> Result<WorkerResult, BackendError>,
    ) -> Result<Arc<ArtCatalog>, RenderError> {
        service_with(outcome).0.capabilities().await
    }

    fn catalog_result() -> Result<WorkerResult, BackendError> {
        Ok(WorkerResult {
            capabilities: Some(serde_json::json!({
                "pscat_version": "0.0.0",
                "catalog_signature": "0000000000000000",
                "font_packs": {
                    "pinned_index": null, "installed": [], "overrides": [], "available": []
                },
                "capabilities": []
            })),
            ..WorkerResult::empty()
        })
    }

    /// A backend whose job never ends, or panics.
    struct Stuck {
        panic: bool,
    }

    impl RenderBackend for Stuck {
        fn execute(
            &self,
            _job: WorkerJob,
            _deadline: std::time::Duration,
        ) -> crate::backend::BackendFuture<'_> {
            let panic = self.panic;
            Box::pin(async move {
                if panic {
                    panic!("backend panic (test)");
                }
                std::future::pending().await
            })
        }
    }

    fn job_service(panic: bool) -> Arc<RenderService> {
        Arc::new(
            RenderService::new(Arc::new(Stuck { panic }), LimitsConfig::default())
                .with_jobs(JobsConfig::default().enabled()),
        )
    }

    fn job_req() -> JobRequest {
        JobRequest::Render(req("showpage"))
    }

    /// After a job's task died, the store holds it as `worker_failed`, with
    /// only its small error charged and no cap slot taken.
    fn assert_failed_cleanly(svc: &RenderService, id: &str) {
        let status = svc.jobs().status("p", id).unwrap();
        assert_eq!(status.state, crate::jobs::JobState::Failed);
        assert_eq!(
            status.job_error.as_ref().map(|e| e.code),
            Some(ErrorCode::WorkerFailed)
        );
        assert_eq!(svc.jobs().unfinished(), 0);
        assert!(svc.jobs().charged_bytes() < 8192, "reservation released");
        // Its slot in the caps is free: the per-client cap of 4 allows 4 more.
        for _ in 0..4 {
            svc.jobs().insert("p", "render_postscript").unwrap();
        }
    }

    #[test]
    fn a_job_dropped_with_the_runtime_is_finished() {
        let svc = job_service(false);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let id = rt.block_on(async {
            let status = svc.start_job("p", job_req()).unwrap();
            // Let the job reach its worker, which never returns.
            for _ in 0..100 {
                if svc.jobs().status("p", &status.job_id).unwrap().state
                    == crate::jobs::JobState::Running
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            status.job_id
        });
        assert_eq!(
            svc.jobs().status("p", &id).unwrap().state,
            crate::jobs::JobState::Running
        );
        // Shutting the runtime down drops the job's task mid-render.
        drop(rt);
        assert_failed_cleanly(&svc, &id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_job_whose_task_panics_is_finished() {
        let svc = job_service(true);
        let id = svc.start_job("p", job_req()).unwrap().job_id;
        for _ in 0..200 {
            if svc.jobs().unfinished() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_failed_cleanly(&svc, &id);
    }

    #[tokio::test]
    async fn the_catalog_is_fetched_once_and_failures_are_not_kept() {
        let (service, calls) = service_with(catalog_result);
        let first = service.capabilities().await.unwrap();
        let second = service.capabilities().await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call hit the worker"
        );
        assert_eq!(service.counters.executed.load(Ordering::SeqCst), 1);

        let (service, calls) = service_with(|| Err(BackendError::Timeout));
        assert!(service.capabilities().await.is_err());
        assert!(service.capabilities().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a failure was cached");
    }

    #[tokio::test]
    async fn capabilities_failures_use_the_error_codes() {
        let err = capabilities_with(|| Err(BackendError::Timeout))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);

        let err = capabilities_with(|| Err(BackendError::Crashed("signal 9".into())))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::WorkerFailed);

        let err = capabilities_with(|| Ok(WorkerResult::empty()))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::WorkerFailed);
        assert!(err.message.contains("no capability catalog"), "{err}");

        let err = capabilities_with(|| {
            Ok(WorkerResult {
                capabilities: Some(serde_json::json!({"capabilities": 3})),
                ..WorkerResult::empty()
            })
        })
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::WorkerFailed);
        assert!(err.message.contains("unexpected shape"), "{err}");

        let err = capabilities_with(|| {
            Ok(WorkerResult::fail(WorkerFailure::BadJob {
                message: "nope".into(),
            }))
        })
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::WorkerFailed);
    }
}
