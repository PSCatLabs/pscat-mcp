//! The transport-independent rendering service.
//!
//! [`RenderService`] owns request validation, the concurrency bound and the
//! backend. MCP tools (and a future REST API) call it; nothing here knows
//! about MCP or HTTP.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::artifacts::{ArtifactStore, LinksBackend, MemoryArtifactStore};
use crate::backend::{BackendError, RenderBackend};
use crate::config::{
    ArtifactConfig, JobsConfig, LimitsConfig, MAX_DPI, MIN_DPI, PSCAT_MAX_SIDE_PX,
};
use crate::generate::{GenerateJob, PlannedPage};
use crate::jobs::{JobState, JobStatus, JobStore, LOCAL_PRINCIPAL};
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
    /// Artifact links (`delivery` `url`) could not be made: the upload to,
    /// or the signing for, the link store failed. Retryable; nothing is
    /// returned by link.
    StorageUnavailable,
}

impl ErrorCode {
    /// The wire name, as `structuredContent.error.code` carries it. The log
    /// fields `code` use the same strings, so a log line and a response
    /// match.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::SourceTooLarge => "source_too_large",
            ErrorCode::UnsupportedFormat => "unsupported_format",
            ErrorCode::InvalidDimensions => "invalid_dimensions",
            ErrorCode::TooManyPixels => "too_many_pixels",
            ErrorCode::PostscriptError => "postscript_error",
            ErrorCode::LimitExceeded => "limit_exceeded",
            ErrorCode::Timeout => "timeout",
            ErrorCode::TooManyPages => "too_many_pages",
            ErrorCode::OutputTooLarge => "output_too_large",
            ErrorCode::ServerBusy => "server_busy",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::WorkerFailed => "worker_failed",
            ErrorCode::UnknownJob => "unknown_job",
            ErrorCode::JobNotFinished => "job_not_finished",
            ErrorCode::ResultsBudgetExceeded => "results_budget_exceeded",
            ErrorCode::StorageUnavailable => "storage_unavailable",
        }
    }
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
    /// The per-principal cap on render slots
    /// ([`LimitsConfig::renders_per_principal`]).
    per_principal: Arc<PrincipalSlots>,
    /// The art catalog, fetched from a worker on first use. A failed fetch
    /// leaves it empty, so the next call tries again.
    catalog: tokio::sync::OnceCell<Arc<ArtCatalog>>,
    /// Whether the operator configured a font-pack directory for the worker
    /// ([`RenderService::with_font_packs_configured`]). Only reported.
    font_packs_configured: bool,
    /// Async jobs (off unless [`RenderService::with_jobs`] turned them on).
    /// Its ledger also holds the memory artifact store's bytes.
    jobs: Arc<JobStore>,
    /// Artifact delivery settings ([`RenderService::with_artifacts`]).
    artifact_config: ArtifactConfig,
    /// Where links point, when `artifact_config` turns them on.
    artifacts: Option<Arc<dyn ArtifactStore>>,
    /// Counters for health and tests.
    pub counters: Arc<Counters>,
}

impl RenderService {
    /// A service running jobs on `backend` under `limits`.
    pub fn new(backend: Arc<dyn RenderBackend>, limits: LimitsConfig) -> Self {
        let slots = Arc::new(Semaphore::new(limits.concurrency()));
        let per_principal = Arc::new(PrincipalSlots::new(limits.renders_per_principal()));
        RenderService {
            per_principal,
            jobs: Arc::new(JobStore::new(JobsConfig::default(), &limits)),
            artifact_config: ArtifactConfig::default(),
            artifacts: None,
            backend,
            limits,
            slots,
            catalog: tokio::sync::OnceCell::new(),
            font_packs_configured: false,
            counters: Arc::new(Counters::default()),
        }
    }

    /// The same service, told whether its backend gives workers a
    /// font-pack directory. It changes nothing about how jobs run; it makes
    /// `get_renderer_info` report the packs the workers can see.
    pub fn with_font_packs_configured(mut self, configured: bool) -> Self {
        self.font_packs_configured = configured;
        self
    }

    /// Whether the operator configured a font-pack directory.
    pub fn font_packs_configured(&self) -> bool {
        self.font_packs_configured
    }

    /// The same service serving async jobs with `jobs` (on only when
    /// `jobs.enabled`).
    pub fn with_jobs(mut self, jobs: JobsConfig) -> Self {
        self.jobs = Arc::new(JobStore::new(jobs, &self.limits));
        self.rebuild_artifacts()
    }

    /// The same service with artifact delivery as `config` says: links
    /// (`url`, `both`, `auto`) only with a backend. The memory backend
    /// shares the async-job results budget, so it is built over the job
    /// store whichever of this and [`Self::with_jobs`] comes last.
    pub fn with_artifacts(mut self, config: ArtifactConfig) -> Self {
        self.artifact_config = config;
        self.rebuild_artifacts()
    }

    /// The same service with `store` as its artifact store (tests: a store
    /// with its own clock). `config`'s backend setting is not consulted.
    pub fn with_artifact_store(
        mut self,
        config: ArtifactConfig,
        store: Arc<dyn ArtifactStore>,
    ) -> Self {
        self.artifact_config = config;
        self.artifacts = Some(store);
        self
    }

    fn rebuild_artifacts(mut self) -> Self {
        let c = &self.artifact_config;
        match c.artifact_links {
            LinksBackend::Memory => {
                self.artifacts = Some(Arc::new(MemoryArtifactStore::new(
                    self.jobs.clone(),
                    c.ttl(),
                    c.artifact_quota_bytes,
                )))
            }
            LinksBackend::Off => self.artifacts = None,
            // Built by `build_service_with` (its TLS client can fail) and
            // installed with `with_artifact_store`; it holds nothing of the
            // job store, so a later `with_jobs` keeps it.
            LinksBackend::Gcs => {}
        }
        self
    }

    /// The artifact store links point to, if any.
    pub fn artifacts(&self) -> Option<&Arc<dyn ArtifactStore>> {
        self.artifacts.as_ref()
    }

    /// Artifact delivery settings.
    pub fn artifact_config(&self) -> &ArtifactConfig {
        &self.artifact_config
    }

    /// The job store's ledger, shared with the memory artifact store.
    pub fn job_store(&self) -> &Arc<JobStore> {
        &self.jobs
    }

    /// The async job store.
    pub fn jobs(&self) -> &JobStore {
        &self.jobs
    }

    /// The limits in force.
    pub fn limits(&self) -> &LimitsConfig {
        &self.limits
    }

    /// Render a program, with no per-principal cap (one local user).
    pub async fn render(&self, req: RenderRequest) -> Result<RenderOutput, RenderError> {
        self.render_as(None, req).await
    }

    /// Render a program for `owner`, whose renders share the per-principal
    /// cap on render slots (`None`, or stdio's one local user
    /// [`LOCAL_PRINCIPAL`]: no cap).
    pub async fn render_as(
        &self,
        owner: Option<&str>,
        req: RenderRequest,
    ) -> Result<RenderOutput, RenderError> {
        let validated = self.check(&req, JobMode::Render)?;
        self.render_job(owner, validated.job).await
    }

    /// Generate a program from non-PostScript input and render it, with no
    /// per-principal cap. The input is checked here ([`validate_generate`]);
    /// the generator and the program run in the worker, like a render.
    pub async fn generate(&self, req: GenerateRequest) -> Result<RenderOutput, RenderError> {
        self.generate_as(None, req).await
    }

    /// [`Self::generate`] for `owner` (see [`Self::render_as`]).
    pub async fn generate_as(
        &self,
        owner: Option<&str>,
        req: GenerateRequest,
    ) -> Result<RenderOutput, RenderError> {
        let validated = self.rejected(validate_generate(&req, &self.limits))?;
        self.render_job(owner, validated.job).await
    }

    async fn render_job(
        &self,
        owner: Option<&str>,
        job: WorkerJob,
    ) -> Result<RenderOutput, RenderError> {
        let started = Instant::now();
        // A review sheet's deadline covers all of its cells.
        let deadline = if job.sheet.is_some() {
            self.limits.sheet_timeout()
        } else {
            self.limits.render_timeout()
        };
        let permit = self.slot(owner).await?;
        self.render_in_slot(permit, job, deadline, started).await
    }

    async fn render_in_slot(
        &self,
        permit: Slot,
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

    /// Check `req` as an async job: under the job limits (the job deadline
    /// as the render timeout). Returns the job and the tool it runs.
    fn validate_job(
        &self,
        req: &JobRequest,
    ) -> Result<(ValidatedRequest, &'static str), RenderError> {
        if !self.jobs.enabled() {
            return Err(RenderError::new(
                ErrorCode::InvalidRequest,
                "async jobs are not enabled on this server",
            ));
        }
        let limits = self.jobs.config().job_limits(&self.limits);
        let (checked, tool) = match req {
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
        Ok((validated, tool))
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
        let (validated, tool) = self.validate_job(&req)?;
        let (id, cancel) = self.jobs.insert(owner, tool)?;
        self.counters.jobs_started.fetch_add(1, Ordering::Relaxed);
        tracing::info!(tool, "async job accepted");
        // The status as accepted (`queued`), before the job can move on.
        let status = self.jobs.status(owner, &id);
        let guard = self.finish_on_drop(&id);
        let service = Arc::clone(self);
        let job = validated.job;
        let deadline = self.jobs.config().job_timeout();
        let work = {
            let service = Arc::clone(self);
            let id = id.clone();
            let owner = owner.to_string();
            async move {
                // A running-job permit first, then the owner's share of the
                // render slots, then a render slot shared with every
                // synchronous call; a queued job waits as long as it takes.
                let _running = service
                    .jobs
                    .running_permit()
                    .await
                    .ok_or_else(shutting_down)?;
                let principal = match service.per_principal.cap {
                    Some(_) if owner != LOCAL_PRINCIPAL => {
                        Some(service.per_principal.acquire(&owner).await)
                    }
                    _ => None,
                };
                let global = service
                    .slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| shutting_down())?;
                let permit = Slot {
                    _global: global,
                    _principal: principal,
                };
                service.jobs.mark_running(&id);
                service
                    .render_in_slot(permit, job, deadline, Instant::now())
                    .await
            }
        };
        // In the span of the request that started it, so the job's lines
        // carry its request id (and, for `oauth`, its `sub`).
        tokio::spawn(async move { service.drive_job(guard, cancel, work).await }.in_current_span());
        status
    }

    /// Finishes job `id` as `worker_failed` if it is dropped armed: its
    /// task panicked, or was dropped (the runtime shutting down) before it
    /// reached [`Self::drive_job`]'s `finish`, or before it first ran. The
    /// job then leaves the caps and keeps only its error against the
    /// budget, and `unfinished` counts it no more.
    fn finish_on_drop(self: &Arc<Self>, id: &str) -> FinishOnDrop {
        FinishOnDrop {
            service: Arc::clone(self),
            id: id.to_string(),
            armed: true,
        }
    }

    /// Run a job's `work` to its end, or until `cancel` fires, and record
    /// the outcome. Dropping `work` drops the backend's future, which kills
    /// the worker.
    async fn drive_job(
        self: Arc<Self>,
        mut guard: FinishOnDrop,
        cancel: CancellationToken,
        work: impl Future<Output = Result<RenderOutput, RenderError>>,
    ) {
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
        self.jobs.finish(&guard.id, outcome);
    }

    /// A Tasks client's `tools/call` of a task-eligible tool, for `owner`
    /// (see [`crate::jobs`]). It runs synchronously first, in a render slot
    /// taken the synchronous way (the queue timeout applies), and becomes a
    /// task only if it is still running after the task threshold
    /// (`--task-after-ms`):
    ///
    /// * finished before the threshold: its result, and the job store is
    ///   never touched (no cap, no reservation, no running-job permit);
    /// * past the threshold: the same render, worker and slot carry on as a
    ///   job, which takes a running-job permit, counts against the caps and
    ///   reserves its largest result only from then on;
    /// * past the threshold but refused as a job (no running-job permit
    ///   free, a cap, the budget): it carries on as a synchronous call and
    ///   is stopped at the sync deadline (`timeout`).
    ///
    /// The worker starts under the job limits, since a worker's limits are
    /// fixed when it starts; until the call becomes a task, the sync
    /// deadline is enforced here. `ct` is the request's cancellation: it
    /// kills the worker until the call has become a task, and after that
    /// only `tasks/cancel` (the job's own token) does. A threshold of zero
    /// makes the call a task at once ([`Self::start_job`]).
    pub async fn call_or_task(
        self: &Arc<Self>,
        owner: &str,
        req: JobRequest,
        ct: &CancellationToken,
    ) -> TaskCall {
        let after = self.jobs.config().task_after();
        if after.is_zero() {
            return match self.start_job(owner, req) {
                Ok(status) => TaskCall::Task(status),
                Err(err) => TaskCall::Done(Err(err)),
            };
        }
        let (validated, tool) = match self.validate_job(&req) {
            Ok(v) => v,
            Err(err) => return TaskCall::Done(Err(err)),
        };
        let permit = tokio::select! {
            biased;
            () = ct.cancelled() => return TaskCall::Done(Err(cancelled())),
            p = self.slot(Some(owner)) => match p {
                Ok(p) => p,
                Err(err) => return TaskCall::Done(Err(err)),
            },
        };
        // The threshold and the sync deadline count from here, as a sync
        // call's deadline does (time queued is not counted).
        let started = Instant::now();
        let job_deadline = self.jobs.config().job_timeout();
        let service = Arc::clone(self);
        let job = validated.job;
        let mut render: RenderFuture = Box::pin(async move {
            service
                .render_in_slot(permit, job, job_deadline, started)
                .await
        });
        tokio::select! {
            biased;
            r = &mut render => return TaskCall::Done(r),
            () = ct.cancelled() => return TaskCall::Done(Err(cancelled())),
            () = tokio::time::sleep(after) => {}
        }
        let render = match self.promote(owner, tool, started, render) {
            Ok(status) => return TaskCall::Task(status),
            Err((render, why)) => {
                tracing::info!(
                    tool,
                    code = ?why.code,
                    "call not promoted to a task; it carries on under the sync deadline"
                );
                render
            }
        };
        let sync = self.limits.render_timeout();
        let left = sync.saturating_sub(started.elapsed());
        TaskCall::Done(tokio::select! {
            biased;
            r = render => r,
            () = ct.cancelled() => Err(cancelled()),
            () = tokio::time::sleep(left) => {
                // Dropping `render` here kills the worker.
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(tool, "render killed at the deadline");
                Err(RenderError::new(
                    ErrorCode::Timeout,
                    format!("the render ran past {} ms and was stopped", sync.as_millis()),
                ))
            }
        })
    }

    /// Make the running `render` a job of `owner`'s: a running-job permit
    /// (never waited for: the call holds a render slot, and sync calls must
    /// always have one), then the caps and the reservation. On success the
    /// render moves into a background task, with the same worker and slot.
    /// On refusal the render comes back, still running.
    fn promote(
        self: &Arc<Self>,
        owner: &str,
        tool: &'static str,
        started: Instant,
        render: RenderFuture,
    ) -> Result<JobStatus, (RenderFuture, RenderError)> {
        let Some(running) = self.jobs.try_running_permit() else {
            return Err((
                render,
                RenderError::new(ErrorCode::ServerBusy, "every running-job permit is taken"),
            ));
        };
        let (id, cancel) = match self.jobs.insert_since(owner, tool, started) {
            Ok(v) => v,
            Err(err) => return Err((render, err)),
        };
        self.jobs.mark_running(&id);
        let status = match self.jobs.status(owner, &id) {
            Ok(status) => status,
            Err(err) => {
                self.jobs.finish(&id, None);
                return Err((render, err));
            }
        };
        self.counters.jobs_started.fetch_add(1, Ordering::Relaxed);
        tracing::info!(tool, "call promoted to a task");
        let guard = self.finish_on_drop(&id);
        let service = Arc::clone(self);
        tokio::spawn(
            async move {
                let work = async move {
                    let _running = running;
                    render.await
                };
                service.drive_job(guard, cancel, work).await
            }
            .in_current_span(),
        );
        Ok(status)
    }

    /// Cancel `owner`'s job `id` and wait (briefly) for it to stop; returns
    /// its status. A finished job is left as it is.
    pub async fn cancel_job(&self, owner: &str, id: &str) -> Result<JobStatus, RenderError> {
        let before = self.jobs.status(owner, id)?;
        self.jobs.request_cancel(owner, id)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = match self.jobs.status(owner, id) {
                Ok(status) => status,
                // A cancelled job is dropped early when room is short, so
                // one that stopped and was dropped between two polls is
                // still a cancel that worked (a job already finished when
                // asked keeps the state it had).
                Err(e) if e.code == ErrorCode::UnknownJob => {
                    let state = if before.state.is_finished() {
                        before.state
                    } else {
                        JobState::Cancelled
                    };
                    return Ok(JobStatus {
                        state,
                        cancel_requested: false,
                        ..before
                    });
                }
                Err(e) => return Err(e),
            };
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

    /// Scan and execute a program, returning diagnostics and no image; no
    /// per-principal cap.
    pub async fn validate(&self, source: String) -> Result<ValidationReport, RenderError> {
        self.validate_as(None, source).await
    }

    /// [`Self::validate`] for `owner` (see [`Self::render_as`]).
    pub async fn validate_as(
        &self,
        owner: Option<&str>,
        source: String,
    ) -> Result<ValidationReport, RenderError> {
        let req = RenderRequest {
            source,
            ..RenderRequest::default()
        };
        let validated = self.check(&req, JobMode::Validate)?;
        let result = self
            .run(owner, validated.job, self.limits.render_timeout())
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
        let result = self.run(None, job, self.limits.render_timeout()).await?;
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
            tracing::info!(code = e.code.as_str(), "request rejected before execution");
        })
    }

    /// Run `job` on the backend in a render slot of `owner`'s, killing it
    /// after `deadline`.
    async fn run(
        &self,
        owner: Option<&str>,
        job: WorkerJob,
        deadline: Duration,
    ) -> Result<WorkerResult, RenderError> {
        let permit = self.slot(owner).await?;
        self.run_in_slot(permit, job, deadline).await
    }

    /// A render slot for `owner`, waiting at most the queue timeout in all:
    /// first for the owner's share of the slots (when the per-principal cap
    /// is on and there is an owner), then for a slot. Waiting for the share
    /// holds no slot, so another principal's call can take a free one.
    async fn slot(&self, owner: Option<&str>) -> Result<Slot, RenderError> {
        let waited = Instant::now();
        let until = tokio::time::Instant::from_std(waited + self.limits.queue_timeout());
        // The one local user of stdio is not limited.
        let owner = owner.filter(|o| *o != LOCAL_PRINCIPAL);
        let principal = match (owner, self.per_principal.cap) {
            (Some(owner), Some(cap)) => Some(
                tokio::time::timeout_at(until, self.per_principal.acquire(owner))
                    .await
                    .map_err(|_| {
                        // Only fixed fields: the wire code, the cap and the
                        // time waited, never who it was.
                        tracing::warn!(
                            code = ErrorCode::ServerBusy.as_str(),
                            limit = PER_PRINCIPAL_CODE,
                            renders_per_principal = cap,
                            queue_wait_ms = waited.elapsed().as_millis() as u64,
                            "request refused: this client already holds its share of the render \
                             slots"
                        );
                        let mut err = RenderError::new(
                            ErrorCode::ServerBusy,
                            format!(
                                "this client already has {cap} render(s) running, the most one \
                                 client may run at once on this server; try again when one \
                                 finishes"
                            ),
                        );
                        err.input = Some(Box::new(InputDiagnostic {
                            code: PER_PRINCIPAL_CODE.into(),
                            token: None,
                            limit_bytes: None,
                            reached_bytes: None,
                        }));
                        err
                    })?,
            ),
            _ => None,
        };
        let global = tokio::time::timeout_at(until, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| {
                // The signal that the service needs more capacity. Only fixed
                // fields: the wire code and the time waited.
                tracing::warn!(
                    code = ErrorCode::ServerBusy.as_str(),
                    queue_wait_ms = waited.elapsed().as_millis() as u64,
                    "request refused: no render slot became free in time"
                );
                RenderError::new(
                    ErrorCode::ServerBusy,
                    "all render slots are busy; try again shortly",
                )
            })?
            .map_err(|_| shutting_down())?;
        Ok(Slot {
            _global: global,
            _principal: principal,
        })
    }

    /// Run `job` in the slot `permit` holds, killing it after `deadline`.
    async fn run_in_slot(
        &self,
        permit: Slot,
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
                // `code` and `limit` are the wire's `error.code` and
                // `error.postscript.limit`, picked from a fixed vocabulary:
                // nothing the program wrote (an error's name or command, a
                // message) reaches the log.
                let (code, limit) = match r.failure.as_ref() {
                    Some(f) => {
                        let (c, l) = failure_log_fields(f, r.ps_error.as_ref());
                        (Some(c.as_str()), l)
                    }
                    None => (None, None),
                };
                tracing::info!(
                    source_bytes = source_len,
                    steps = r.steps_used,
                    elapsed_ms = r.elapsed_ms,
                    pages = r.page_count,
                    failed = r.failure.is_some(),
                    code,
                    limit,
                    "render finished"
                );
                Ok(r)
            }
            Err(BackendError::Timeout) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    source_bytes = source_len,
                    code = ErrorCode::Timeout.as_str(),
                    "render killed at the deadline"
                );
                Err(RenderError::new(
                    ErrorCode::Timeout,
                    format!(
                        "{what} ran past {} ms and was stopped",
                        deadline.as_millis()
                    ),
                ))
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    code = ErrorCode::WorkerFailed.as_str(),
                    "render worker failed"
                );
                Err(RenderError::new(ErrorCode::WorkerFailed, e.to_string()))
            }
        }
    }
}

/// `error.input.code` (and the log's `limit`) of a call refused by the
/// per-principal cap.
const PER_PRINCIPAL_CODE: &str = "renders_per_principal";

/// A held render slot: the global permit and, under the per-principal cap,
/// the owner's share. Dropping it (the render ended, was cancelled or was
/// killed) frees both.
pub(crate) struct Slot {
    _global: OwnedSemaphorePermit,
    _principal: Option<PrincipalLease>,
}

/// The per-principal cap on render slots: one semaphore of `cap` permits
/// per principal id that holds or waits for a slot. An entry is removed as
/// soon as nobody holds or waits for it, so the map stays as small as the
/// set of principals rendering right now, however many sign in over time.
pub(crate) struct PrincipalSlots {
    cap: Option<usize>,
    held: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl PrincipalSlots {
    fn new(cap: Option<usize>) -> Self {
        PrincipalSlots {
            cap,
            held: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Semaphore>>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One of `owner`'s permits, waiting as long as it takes (callers bound
    /// the wait). Only called with a cap.
    async fn acquire(self: &Arc<Self>, owner: &str) -> PrincipalLease {
        let cap = self.cap.unwrap_or(1);
        let sem = self
            .lock()
            .entry(owner.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(cap)))
            .clone();
        // Made before the wait, so a wait given up (the queue timeout, a
        // cancelled call) still removes an entry nobody uses.
        let mut lease = PrincipalLease {
            slots: Arc::clone(self),
            owner: owner.to_string(),
            sem: Some(sem.clone()),
            permit: None,
        };
        // The semaphore is never closed.
        lease.permit = sem.acquire_owned().await.ok();
        lease
    }

    /// Principals holding or waiting for a slot (tests).
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

/// A principal's share of a render slot, held or waited for.
struct PrincipalLease {
    slots: Arc<PrincipalSlots>,
    owner: String,
    sem: Option<Arc<Semaphore>>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for PrincipalLease {
    fn drop(&mut self) {
        // Release first, so the count below sees only other users.
        drop(self.permit.take());
        drop(self.sem.take());
        let mut held = self.slots.lock();
        // Every holder and waiter keeps a clone; only the map's is left
        // when nobody uses the entry. Clones are taken under the lock, so
        // the count cannot grow while it is checked.
        if held
            .get(&self.owner)
            .is_some_and(|sem| Arc::strong_count(sem) == 1)
        {
            held.remove(&self.owner);
        }
    }
}

/// A render running in its slot, movable into a job's task.
type RenderFuture = Pin<Box<dyn Future<Output = Result<RenderOutput, RenderError>> + Send>>;

/// What [`RenderService::call_or_task`] gives.
#[derive(Debug)]
pub enum TaskCall {
    /// The call's result: it finished before the task threshold, could not
    /// become a task, or was refused or cancelled.
    Done(Result<RenderOutput, RenderError>),
    /// The call became a task (a job); its status as it became one.
    Task(JobStatus),
}

/// The error for a request the client cancelled.
pub(crate) fn cancelled() -> RenderError {
    RenderError::new(ErrorCode::Cancelled, "the request was cancelled")
}

/// See [`RenderService::finish_on_drop`].
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

/// The error code and, for a limit hit, the limit (`steps`, `time`,
/// `memory`) that [`failure_to_error`] gives a failure, for the log. The
/// limit is one of a fixed set (`other` for a name outside it), never the
/// worker's own string.
fn failure_log_fields(
    failure: &WorkerFailure,
    ps_error: Option<&PsDiagnostic>,
) -> (ErrorCode, Option<&'static str>) {
    match failure {
        WorkerFailure::PostScript => match ps_error.and_then(|d| d.limit.as_deref()) {
            Some("steps") => (ErrorCode::LimitExceeded, Some("steps")),
            Some("time") => (ErrorCode::LimitExceeded, Some("time")),
            Some("memory") => (ErrorCode::LimitExceeded, Some("memory")),
            Some(_) => (ErrorCode::LimitExceeded, Some("other")),
            None => (ErrorCode::PostscriptError, None),
        },
        WorkerFailure::Generate(f) => (f.code.into(), None),
        WorkerFailure::TooManyPages { .. } => (ErrorCode::TooManyPages, None),
        WorkerFailure::OutputTooLarge { .. } => (ErrorCode::OutputTooLarge, None),
        WorkerFailure::Encode { .. } | WorkerFailure::BadJob { .. } => {
            (ErrorCode::WorkerFailed, None)
        }
    }
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
    fn error_code_as_str_is_the_wire_name() {
        for code in [
            ErrorCode::InvalidRequest,
            ErrorCode::SourceTooLarge,
            ErrorCode::UnsupportedFormat,
            ErrorCode::InvalidDimensions,
            ErrorCode::TooManyPixels,
            ErrorCode::PostscriptError,
            ErrorCode::LimitExceeded,
            ErrorCode::Timeout,
            ErrorCode::TooManyPages,
            ErrorCode::OutputTooLarge,
            ErrorCode::ServerBusy,
            ErrorCode::Cancelled,
            ErrorCode::WorkerFailed,
            ErrorCode::UnknownJob,
            ErrorCode::JobNotFinished,
            ErrorCode::ResultsBudgetExceeded,
        ] {
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::Value::String(code.as_str().into())
            );
        }
    }

    #[test]
    fn failure_log_fields_agree_with_the_wire_error() {
        use crate::generate::{GenerateErrorCode, GenerateFailure};
        let ps = |name: &str, limit: Option<&str>| PsDiagnostic {
            name: name.into(),
            limit: limit.map(Into::into),
            report: "a program-controlled report".into(),
        };
        let cases: Vec<(WorkerFailure, Option<PsDiagnostic>, Option<&str>)> = vec![
            (WorkerFailure::PostScript, Some(ps("undefined", None)), None),
            (WorkerFailure::PostScript, None, None),
            (
                WorkerFailure::PostScript,
                Some(ps("limitexceeded", Some("memory"))),
                Some("memory"),
            ),
            (
                WorkerFailure::PostScript,
                Some(ps("limitexceeded", Some("steps"))),
                Some("steps"),
            ),
            (
                WorkerFailure::PostScript,
                Some(ps("limitexceeded", Some("time"))),
                Some("time"),
            ),
            (
                WorkerFailure::PostScript,
                Some(ps("limitexceeded", Some("a program-controlled name"))),
                Some("other"),
            ),
            (WorkerFailure::TooManyPages { pages: 3, max: 2 }, None, None),
            (
                WorkerFailure::OutputTooLarge { bytes: 3, max: 2 },
                None,
                None,
            ),
            (
                WorkerFailure::Encode {
                    message: "x".into(),
                },
                None,
                None,
            ),
            (
                WorkerFailure::BadJob {
                    message: "x".into(),
                },
                None,
                None,
            ),
            (
                WorkerFailure::Generate(GenerateFailure {
                    code: GenerateErrorCode::InvalidRequest,
                    detail: "config".into(),
                    token: Some("a token".into()),
                    message: "m".into(),
                }),
                None,
                None,
            ),
        ];
        for (failure, ps_error, limit) in cases {
            let (code, got_limit) = failure_log_fields(&failure, ps_error.as_ref());
            assert_eq!(got_limit, limit, "{failure:?}");
            let mut result = WorkerResult::fail(failure.clone());
            result.ps_error = ps_error;
            let err = failure_to_error(failure, result, OutputFormat::Png, None);
            assert_eq!(code, err.code);
            // The limit the log names is the one the wire names (or the
            // fixed `other` for a name outside the set).
            if code == ErrorCode::LimitExceeded {
                let wire = err.postscript.as_ref().and_then(|d| d.limit.as_deref());
                assert!(got_limit == wire || got_limit == Some("other"), "{wire:?}");
            }
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

    /// A backend whose job, named by its source, runs until the test opens
    /// that source's gate (or drops the call).
    #[derive(Default)]
    struct Gated {
        gates: Mutex<HashMap<String, Arc<Semaphore>>>,
        started: AtomicU64,
    }

    impl Gated {
        fn gate(&self, name: &str) -> Arc<Semaphore> {
            self.gates
                .lock()
                .unwrap()
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(0)))
                .clone()
        }
    }

    impl RenderBackend for Gated {
        fn execute(
            &self,
            job: WorkerJob,
            _deadline: std::time::Duration,
        ) -> crate::backend::BackendFuture<'_> {
            let gate = self.gate(&job.source);
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _open = gate.acquire().await;
                Ok(WorkerResult::empty())
            })
        }
    }

    async fn wait_for(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_principal_cannot_take_every_render_slot() {
        let backend = Arc::new(Gated::default());
        let limits = LimitsConfig {
            max_concurrent_renders: Some(2),
            queue_timeout_ms: 200,
            ..LimitsConfig::default()
        };
        assert_eq!(limits.renders_per_principal(), Some(1));
        let svc = Arc::new(RenderService::new(backend.clone(), limits));
        let started = || backend.started.load(Ordering::SeqCst);
        let spawn = |owner: &'static str, source: &'static str| {
            let svc = svc.clone();
            tokio::spawn(async move { svc.render_as(Some(owner), req(source)).await })
        };

        // A holds its one share; its second call waits out the queue
        // timeout without taking the free slot, and is refused as busy.
        let a1 = spawn("token#0", "a1");
        wait_for("a1 to start", || started() == 1).await;
        let err = svc.render_as(Some("token#0"), req("a2")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ServerBusy, "{err}");
        assert_eq!(
            err.input.as_ref().map(|i| i.code.as_str()),
            Some(PER_PRINCIPAL_CODE)
        );
        assert!(err.message.contains("1 render(s) running"), "{err}");
        assert_eq!(started(), 1);
        // Another principal still gets the free slot.
        let b1 = spawn("oauth:bbbb", "b1");
        wait_for("b1 to start", || started() == 2).await;
        assert_eq!(svc.per_principal.tracked(), 2);

        // A's render ends: A gets a slot again.
        backend.gate("a1").add_permits(1);
        a1.await.unwrap().unwrap_err(); // no artifacts: worker_failed
        let a3 = spawn("token#0", "a3");
        wait_for("a3 to start", || started() == 3).await;
        // A's render is cancelled (the call dropped): A gets a slot again.
        a3.abort();
        assert!(a3.await.unwrap_err().is_cancelled());
        let a4 = spawn("token#0", "a4");
        wait_for("a4 to start", || started() == 4).await;

        // Every share freed, the map is empty again.
        a4.abort();
        b1.abort();
        let _ = (a4.await, b1.await);
        assert_eq!(svc.per_principal.tracked(), 0);
        assert_eq!(svc.slots.available_permits(), 2);

        // stdio's one local user, and a call with no owner, are not
        // limited: both slots at once.
        let l1 = spawn(LOCAL_PRINCIPAL, "l1");
        let svc2 = svc.clone();
        let l2 = tokio::spawn(async move { svc2.render(req("l2")).await });
        wait_for("both local renders to start", || started() == 6).await;
        assert_eq!(svc.per_principal.tracked(), 0);
        l1.abort();
        l2.abort();
        let _ = (l1.await, l2.await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_job_takes_its_share_and_gives_it_back() {
        let backend = Arc::new(Gated::default());
        let limits = LimitsConfig {
            max_concurrent_renders: Some(4),
            queue_timeout_ms: 200,
            ..LimitsConfig::default()
        };
        let jobs = JobsConfig {
            max_running_jobs: Some(3),
            ..JobsConfig::default()
        }
        .enabled();
        let svc = Arc::new(RenderService::new(backend.clone(), limits).with_jobs(jobs));
        let started = || backend.started.load(Ordering::SeqCst);
        // Two jobs fill p's share of 2; a third waits, holding no slot.
        let mut ids = Vec::new();
        for src in ["j1", "j2", "j3"] {
            let r = JobRequest::Render(req(src));
            ids.push(svc.start_job("p", r).unwrap().job_id);
        }
        wait_for("two jobs to start", || started() == 2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(started(), 2);
        assert_eq!(svc.slots.available_permits(), 2);
        // A sync call of p's is refused while its share is held.
        let err = svc.render_as(Some("p"), req("s1")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ServerBusy);
        // Cancelling a running job lets the waiting one start.
        let first_running = ids
            .iter()
            .find(|id| svc.jobs().status("p", id).unwrap().state == JobState::Running)
            .unwrap()
            .clone();
        svc.cancel_job("p", &first_running).await.unwrap();
        wait_for("the third job to start", || started() == 3).await;
        for id in &ids {
            let _ = svc.cancel_job("p", id).await;
        }
        wait_for("every share to be freed", || {
            svc.per_principal.tracked() == 0
        })
        .await;
        assert_eq!(svc.slots.available_permits(), 4);
    }
}
