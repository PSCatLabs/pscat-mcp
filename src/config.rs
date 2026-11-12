//! Configuration: resource limits, the worker, and the HTTP server.
//!
//! Every setting has a command-line flag and an environment variable
//! (`PSCAT_MCP_*`, plus Cloud Run's `PORT`), so the container is configured
//! entirely through its environment.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;

/// The default size up to which SVG is returned as text, in bytes
/// (40 KiB). A larger SVG is returned as a blob resource instead, up to the
/// output limit.
///
/// The server cannot see a client's limit, so this is chosen from what the
/// clients do with each form (measured and documented in
/// `docs/LOCAL_TESTING.md`, section 11, issues #64 and #63):
///
/// * Claude Code 2.1.322 shows a text result inline only up to about
///   50,000 characters and 25,000 tokens (a 40.5 KB SVG went inline at
///   about 21,400 tokens; 47.5 KB needed `MAX_MCP_OUTPUT_TOKENS` raised).
///   Above that it saves the whole result as a JSON array in a `.txt` file.
///   A blob, SVG included, is saved as a real file (`.svg`, byte for byte)
///   at every size measured, 40 KB to 1 MB. So up to 40 KiB the model reads
///   the markup inline, and above it the user gets a usable `.svg` file
///   instead of an escaped JSON file.
/// * Claude's connector docs give about 150,000 characters for a tool
///   result on claude.ai and Desktop (docs only); what they do with a blob
///   is not measured. Raise this for such a client to keep more SVG as
///   text.
/// * Codex passes a result's `structuredContent` to the model instead of
///   its content blocks, so it sees neither form (source reading).
///
/// The earlier default, 128 KiB, was a cap: over it the call failed.
pub const DEFAULT_SVG_BLOB_THRESHOLD_BYTES: usize = 40 * 1024;

/// The environment variable the old name of the SVG threshold was read from.
/// Kept as a deprecated alias (see [`LimitsConfig::deprecated_max_svg_bytes`]).
pub const DEPRECATED_MAX_SVG_BYTES_ENV: &str = "PSCAT_MCP_MAX_SVG_BYTES";
/// Hard per-side ceiling PSCat applies to every page.
pub const PSCAT_MAX_SIDE_PX: u32 = 8000;
/// The most pixels a review sheet can have: 8000 a side.
pub const MAX_SHEET_PIXELS: u64 = 8000 * 8000;
/// Bounds accepted for the `dpi` argument (the same as upstream pscat-mcp).
pub const MIN_DPI: f64 = 9.0;
/// Upper bound for `dpi`.
pub const MAX_DPI: f64 = 1200.0;

/// Resource limits for one render. Defaults are sized for a 1–2 GiB Cloud
/// Run instance running a couple of renders at once.
#[derive(Debug, Clone, Args)]
pub struct LimitsConfig {
    /// Largest PostScript source accepted, in bytes. Checked before execution.
    #[arg(long, env = "PSCAT_MCP_MAX_SOURCE_BYTES", default_value_t = 256 * 1024)]
    pub max_source_bytes: usize,

    /// Largest LaTeX formula `chalkboard_math` accepts, in bytes. Checked
    /// before execution. About forty rows of a derivation; a board that full
    /// is already crowded.
    #[arg(long, env = "PSCAT_MCP_MAX_LATEX_BYTES", default_value_t = 4096)]
    pub max_latex_bytes: usize,

    /// Largest note text `handwrite` accepts, in bytes. Checked before
    /// execution. The PSCat plugin's is 1 MiB; 64 KiB is far more drawable text than a
    /// note within the pixel limits holds, and keeps the HTTP body limit at
    /// the source limit's. The
    /// note's size is bounded by the pixel limits whatever the text.
    #[arg(long, env = "PSCAT_MCP_MAX_HANDWRITE_BYTES", default_value_t = 64 * 1024)]
    pub max_handwrite_bytes: usize,

    /// Longest value of one MCP prompt argument (`prompts/get` of the guide
    /// prompts), in bytes; a longer one is `invalid_params`. Prompt
    /// arguments are only copied into the prompt's text, never run.
    #[arg(
        long,
        env = "PSCAT_MCP_MAX_PROMPT_ARGUMENT_BYTES",
        default_value_t = 8192
    )]
    pub max_prompt_argument_bytes: usize,

    /// Wall-clock deadline for one render, in milliseconds. The worker
    /// process is killed (SIGKILL) when it passes; PSCat's own time limit is
    /// set a little below it so most runaways stop cleanly first.
    #[arg(long, env = "PSCAT_MCP_RENDER_TIMEOUT_MS", default_value_t = 20_000)]
    pub render_timeout_ms: u64,

    /// PSCat's in-interpreter time limit, in milliseconds. Defaults to the
    /// render timeout minus a margin. 0 disables it (the kill still applies).
    #[arg(long, env = "PSCAT_MCP_INTERP_TIME_LIMIT_MS")]
    pub interp_time_limit_ms: Option<u64>,

    /// PSCat's step limit (objects executed). 0 disables it.
    #[arg(long, env = "PSCAT_MCP_MAX_STEPS", default_value_t = 5_000_000_000)]
    pub max_steps: u64,

    /// PSCat's heap-growth limit for one program, in bytes. 0 disables it.
    #[arg(long, env = "PSCAT_MCP_MAX_MEMORY_BYTES", default_value_t = 512 * 1024 * 1024)]
    pub max_memory_bytes: usize,

    /// OS address-space limit (RLIMIT_AS) for the worker process on Linux, in
    /// bytes. Defaults to the memory limit plus 1 GiB of headroom. 0 disables.
    #[arg(long, env = "PSCAT_MCP_WORKER_ADDRESS_SPACE_BYTES")]
    pub worker_address_space_bytes: Option<u64>,

    /// Most device pixels on one page (width x height), checked before
    /// execution for the requested page and by PSCat for `setpagedevice`.
    #[arg(long, env = "PSCAT_MCP_MAX_PIXELS", default_value_t = 16_000_000)]
    pub max_pixels: u64,

    /// Most pages one render may return.
    #[arg(long, env = "PSCAT_MCP_MAX_PAGES", default_value_t = 10)]
    pub max_pages: usize,

    /// Most bytes of rendered output (all pages, before base64) returned
    /// inline in one tool result.
    #[arg(long, env = "PSCAT_MCP_MAX_OUTPUT_BYTES", default_value_t = 8 * 1024 * 1024)]
    pub max_output_bytes: usize,

    /// Most bytes of SVG (all pages together) returned as embedded text
    /// resources, which a client can show the model as markup. A larger
    /// SVG is returned as blob resources (base64), which clients save as
    /// files, up to the output limit. 0 returns every SVG as a blob.
    /// Defaults to 40 KiB, or the output limit if that is smaller; it may
    /// not exceed the output limit.
    #[arg(long, env = "PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES")]
    pub svg_blob_threshold_bytes: Option<usize>,

    /// Deprecated alias of `--svg-blob-threshold-bytes`, kept for existing
    /// deployments. Setting both is refused at startup.
    #[arg(long = "max-svg-bytes", env = "PSCAT_MCP_MAX_SVG_BYTES", hide = true)]
    pub deprecated_max_svg_bytes: Option<usize>,

    /// Most bytes of program stdout (`print`, `=`) returned.
    #[arg(long, env = "PSCAT_MCP_MAX_PROGRAM_OUTPUT_BYTES", default_value_t = 16 * 1024)]
    pub max_program_output_bytes: usize,

    /// Renders running at once. Defaults to the CPU count, at most 4.
    #[arg(long, env = "PSCAT_MCP_MAX_CONCURRENT_RENDERS")]
    pub max_concurrent_renders: Option<usize>,

    /// How long a render waits for a free slot before failing with
    /// `server_busy`, in milliseconds.
    #[arg(long, env = "PSCAT_MCP_QUEUE_TIMEOUT_MS", default_value_t = 10_000)]
    pub queue_timeout_ms: u64,

    /// Most cells (dpi entries over all rows) one `review_sheet` may have.
    /// Checked before execution. The PSCat plugin's limit.
    #[arg(long, env = "PSCAT_MCP_MAX_SHEET_CELLS", default_value_t = 32)]
    pub max_sheet_cells: usize,

    /// Most bytes of PostScript in one `review_sheet`, all entries together.
    /// Checked before execution. Defaults to the source limit, as the
    /// plugin's does; each entry is also held to the source limit.
    #[arg(long, env = "PSCAT_MCP_MAX_SHEET_SOURCE_BYTES")]
    pub max_sheet_source_bytes: Option<usize>,

    /// Most device pixels the cells of one `review_sheet` render, all cells
    /// together (before any zoom). Checked before execution. Defaults to
    /// the page pixel limit, as the plugin's does.
    #[arg(long, env = "PSCAT_MCP_MAX_SHEET_CELL_PIXELS")]
    pub max_sheet_cell_pixels: Option<u64>,

    /// Most device pixels of the composed `review_sheet` image (width x
    /// height; at most 8000 a side, so at most 64,000,000). Checked before
    /// execution. Defaults to the page pixel limit, as the plugin's does
    /// (capped at 64,000,000). The worker holds the sheet (4 bytes a pixel)
    /// beside every cell, and up to three copies of it while it encodes the
    /// PNG; none of that is in a cell's memory limit (see
    /// docs/CLOUD_RUN.md, "Sizing").
    #[arg(long, env = "PSCAT_MCP_MAX_SHEET_PIXELS")]
    pub max_sheet_pixels: Option<u64>,

    /// Wall-clock deadline for one whole `review_sheet`, in milliseconds:
    /// the worker that renders every cell is killed when it passes. Each
    /// cell also has the render's own step and time limits. Defaults to the
    /// render timeout (the plugin fits a sheet in one call's timeout).
    #[arg(long, env = "PSCAT_MCP_MAX_SHEET_TIMEOUT_MS")]
    pub max_sheet_timeout_ms: Option<u64>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            max_source_bytes: 256 * 1024,
            max_latex_bytes: 4096,
            max_handwrite_bytes: 64 * 1024,
            max_prompt_argument_bytes: 8192,
            render_timeout_ms: 20_000,
            interp_time_limit_ms: None,
            max_steps: 5_000_000_000,
            max_memory_bytes: 512 * 1024 * 1024,
            worker_address_space_bytes: None,
            max_pixels: 16_000_000,
            max_pages: 10,
            max_output_bytes: 8 * 1024 * 1024,
            svg_blob_threshold_bytes: None,
            deprecated_max_svg_bytes: None,
            max_program_output_bytes: 16 * 1024,
            max_concurrent_renders: None,
            queue_timeout_ms: 10_000,
            max_sheet_cells: 32,
            max_sheet_source_bytes: None,
            max_sheet_cell_pixels: None,
            max_sheet_pixels: None,
            max_sheet_timeout_ms: None,
        }
    }
}

impl LimitsConfig {
    /// The external kill deadline.
    pub fn render_timeout(&self) -> Duration {
        Duration::from_millis(self.render_timeout_ms)
    }

    /// PSCat's own time limit: explicit, or the timeout minus a margin
    /// (10%, at least 250 ms) so the interpreter usually stops first.
    pub fn interp_time_limit(&self) -> Option<Duration> {
        match self.interp_time_limit_ms {
            Some(0) => None,
            Some(ms) => Some(Duration::from_millis(ms)),
            None => Some(below_with_margin(self.render_timeout_ms)),
        }
    }

    /// Most bytes of SVG (all pages together) returned as text: the
    /// configured limit (or its deprecated alias), or
    /// [`DEFAULT_SVG_BLOB_THRESHOLD_BYTES`] held to the output limit.
    pub fn svg_blob_threshold_bytes(&self) -> usize {
        self.svg_blob_threshold_bytes
            .or(self.deprecated_max_svg_bytes)
            .unwrap_or_else(|| DEFAULT_SVG_BLOB_THRESHOLD_BYTES.min(self.max_output_bytes))
    }

    /// The one-line deprecation warning, when only the deprecated alias is
    /// set. `None` when the new name is set or neither is.
    pub fn deprecation_warning(&self) -> Option<String> {
        match (self.svg_blob_threshold_bytes, self.deprecated_max_svg_bytes) {
            (None, Some(_)) => Some(format!(
                "{DEPRECATED_MAX_SVG_BYTES_ENV} (--max-svg-bytes) is deprecated; \
                 use PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES (--svg-blob-threshold-bytes) \
                 instead. The value is used as before."
            )),
            _ => None,
        }
    }

    /// Most bytes of PostScript in one sheet.
    pub fn sheet_source_bytes(&self) -> usize {
        self.max_sheet_source_bytes.unwrap_or(self.max_source_bytes)
    }

    /// Most device pixels the cells of one sheet render together.
    pub fn sheet_cell_pixels(&self) -> u64 {
        self.max_sheet_cell_pixels.unwrap_or(self.max_pixels)
    }

    /// Most device pixels of a composed sheet.
    pub fn sheet_pixels(&self) -> u64 {
        self.max_sheet_pixels
            .unwrap_or(self.max_pixels.min(MAX_SHEET_PIXELS))
    }

    /// The deadline of a whole sheet, in milliseconds.
    pub fn sheet_timeout_ms(&self) -> u64 {
        self.max_sheet_timeout_ms.unwrap_or(self.render_timeout_ms)
    }

    /// The external kill deadline of a whole sheet.
    pub fn sheet_timeout(&self) -> Duration {
        Duration::from_millis(self.sheet_timeout_ms())
    }

    /// The time the worker shares out between a sheet's cells: the sheet
    /// deadline minus the same margin the render's interpreter limit
    /// leaves, so composing and encoding the sheet fit before the kill.
    pub fn sheet_time_budget(&self) -> Duration {
        below_with_margin(self.sheet_timeout_ms())
    }

    /// The larger of the input limits that bound one tool call's text: the
    /// source, the `handwrite` text and a sheet's sources together. The
    /// HTTP body limit follows it.
    pub fn max_text_bytes(&self) -> usize {
        self.max_source_bytes
            .max(self.max_handwrite_bytes)
            .max(self.sheet_source_bytes())
    }

    /// PSCat's step limit, or `None` when disabled.
    pub fn steps(&self) -> Option<u64> {
        (self.max_steps > 0).then_some(self.max_steps)
    }

    /// PSCat's memory limit, or `None` when disabled.
    pub fn memory(&self) -> Option<usize> {
        (self.max_memory_bytes > 0).then_some(self.max_memory_bytes)
    }

    /// RLIMIT_AS for the worker, or `None` when disabled.
    pub fn address_space(&self) -> Option<u64> {
        match self.worker_address_space_bytes {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(self.max_memory_bytes as u64 + (1 << 30)),
        }
    }

    /// Renders running at once.
    pub fn concurrency(&self) -> usize {
        self.max_concurrent_renders
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .min(4)
            })
    }

    /// Queue wait before `server_busy`.
    pub fn queue_timeout(&self) -> Duration {
        Duration::from_millis(self.queue_timeout_ms)
    }

    /// Reject settings that cannot work.
    pub fn check(&self) -> Result<(), String> {
        if self.max_source_bytes == 0 {
            return Err("max-source-bytes must be positive".into());
        }
        if self.max_latex_bytes == 0 {
            return Err("max-latex-bytes must be positive".into());
        }
        if self.max_handwrite_bytes == 0 {
            return Err("max-handwrite-bytes must be positive".into());
        }
        if self.max_prompt_argument_bytes == 0 {
            return Err("max-prompt-argument-bytes must be positive".into());
        }
        if self.render_timeout_ms == 0 {
            return Err("render-timeout-ms must be positive".into());
        }
        if let Some(t) = self.interp_time_limit()
            && t >= self.render_timeout()
        {
            return Err("interp-time-limit-ms must be below render-timeout-ms".into());
        }
        if self.max_pixels == 0 || self.max_pages == 0 || self.max_output_bytes == 0 {
            return Err("max-pixels, max-pages and max-output-bytes must be positive".into());
        }
        if self.svg_blob_threshold_bytes.is_some() && self.deprecated_max_svg_bytes.is_some() {
            return Err(
                "set only one of PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES (--svg-blob-threshold-bytes) \
                 and its deprecated alias PSCAT_MCP_MAX_SVG_BYTES (--max-svg-bytes), not both"
                    .into(),
            );
        }
        if self.svg_blob_threshold_bytes() > self.max_output_bytes {
            return Err(format!(
                "svg-blob-threshold-bytes ({}) may not exceed max-output-bytes ({})",
                self.svg_blob_threshold_bytes(),
                self.max_output_bytes
            ));
        }
        if self.max_sheet_cells == 0
            || self.sheet_source_bytes() == 0
            || self.sheet_cell_pixels() == 0
            || self.sheet_pixels() == 0
            || self.sheet_timeout_ms() == 0
        {
            return Err(
                "max-sheet-cells, max-sheet-source-bytes, max-sheet-cell-pixels, \
                        max-sheet-pixels and max-sheet-timeout-ms must be positive"
                    .into(),
            );
        }
        if self.sheet_pixels() > MAX_SHEET_PIXELS {
            return Err(format!(
                "max-sheet-pixels is at most {MAX_SHEET_PIXELS} (8000 pixels a side)"
            ));
        }
        Ok(())
    }
}

/// Async render jobs: `start_render` and its companions, and the MCP Tasks
/// extension (see `src/jobs.rs`). The store is in memory, in the server
/// process: on for stdio, opt-in for HTTP (`--async-jobs`), where it only
/// works on a single, always-running instance.
#[derive(Debug, Clone, Args)]
pub struct JobsConfig {
    /// Whether async jobs are served. Set by the transport, not a flag:
    /// always on for stdio, `--async-jobs` for HTTP.
    #[arg(skip)]
    pub enabled: bool,

    /// Wall-clock deadline of one async job, in milliseconds, counted from
    /// when its worker starts (time queued is not counted). The worker is
    /// killed when it passes; PSCat's own time limit sits a little below it.
    /// 10 minutes: thirty times the sync deadline, enough for a dense
    /// 1200-dpi page, while one job holds one render slot and up to the
    /// memory limit. The PSCat plugin's is an hour; raise it to match.
    #[arg(long, env = "PSCAT_MCP_JOB_TIMEOUT_MS", default_value_t = 600_000)]
    pub job_timeout_ms: u64,

    /// Async jobs running at once; more wait queued. They share the render
    /// slots with every other call, so this stays below
    /// `--max-concurrent-renders` and synchronous calls always have a slot.
    /// Defaults to half the render slots, at least 1. With a single render
    /// slot it must be 1, and a running job then makes synchronous calls
    /// wait for it (and fail with `server_busy` after the queue timeout).
    #[arg(long, env = "PSCAT_MCP_MAX_RUNNING_JOBS")]
    pub max_running_jobs: Option<usize>,

    /// PSCat's step limit for an async job. Unset: 25,000,000,000 (PSCat's
    /// own default for untrusted programs, the plugin's), or
    /// `--max-steps` if that is larger; 0 when `--max-steps` is 0. The sync
    /// step limit would stop a job long before its deadline.
    #[arg(long, env = "PSCAT_MCP_JOB_MAX_STEPS")]
    pub job_max_steps: Option<u64>,

    /// Unfinished (queued or running) jobs one principal (one bearer token,
    /// or the local user over stdio) may have.
    #[arg(long, env = "PSCAT_MCP_MAX_JOBS_PER_PRINCIPAL", default_value_t = 4)]
    pub max_jobs_per_principal: usize,

    /// Unfinished jobs on the whole server. The PSCat plugin keeps 16.
    #[arg(long, env = "PSCAT_MCP_MAX_JOBS", default_value_t = 16)]
    pub max_jobs: usize,

    /// How long a finished job and its result are kept, in milliseconds,
    /// counted from when it finished. The PSCat plugin's is an hour.
    #[arg(long, env = "PSCAT_MCP_JOB_RESULT_TTL_MS", default_value_t = 3_600_000)]
    pub job_result_ttl_ms: u64,

    /// Bytes of job results held in memory, all jobs together. A job
    /// reserves its largest possible result when it starts and keeps what
    /// it really produced once it finishes; a job that does not fit is
    /// refused with `results_budget_exceeded`. Must hold at least one
    /// largest result (`--max-output-bytes` as base64, plus stdout).
    #[arg(long, env = "PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES", default_value_t = 128 * 1024 * 1024)]
    pub job_results_budget_bytes: usize,

    /// Bytes of the results budget one principal (one bearer token, or the
    /// local user over stdio) may have reserved and held, so that one
    /// principal cannot fill the budget and get every other principal's new
    /// jobs refused. A job over the share is refused with
    /// `results_budget_exceeded` and `error.input.code`
    /// `results_per_principal_bytes`. Unset: the budget less one largest
    /// result, so another principal can always start a job, or the whole
    /// budget when that holds fewer than two largest results. Must hold at
    /// least one largest result and be at most the budget; equal to the
    /// budget it turns the share off.
    #[arg(long, env = "PSCAT_MCP_JOB_RESULTS_PER_PRINCIPAL_BYTES")]
    pub job_results_per_principal_bytes: Option<usize>,

    /// Serve the MCP Tasks extension (SEP-2663, `io.modelcontextprotocol/tasks`)
    /// on top of the async jobs. A `tools/call` of `render_postscript`,
    /// `chalkboard_math` or `handwrite` from a client that declares the
    /// extension runs synchronously at first; only one still running after
    /// `--task-after-ms` becomes a task (a job, with the job deadline). A
    /// client that does not declare it gets the synchronous result. Off by
    /// default: no client we have tested sends the capability on
    /// `tools/call`. Needs async jobs.
    #[arg(long, env = "PSCAT_MCP_TASKS", default_value_t = false)]
    pub tasks: bool,

    /// With `--tasks`: how long, in milliseconds, a Tasks client's call runs
    /// before it becomes a task, counted from when its worker starts. A call
    /// that finishes sooner returns its result directly and never touches
    /// the job caps or the results budget. A call that cannot become a task
    /// then (the job caps, the budget, or every running-job permit taken)
    /// carries on synchronously under the sync deadline. Must be below
    /// `--render-timeout-ms`. 0 makes every such call a task at once. Above
    /// 0, such a call's worker starts under the job limits, so they must be
    /// no tighter than the sync ones (`--job-timeout-ms` at least
    /// `--render-timeout-ms`, the job step limit at least `--max-steps`).
    #[arg(long, env = "PSCAT_MCP_TASK_AFTER_MS", default_value_t = DEFAULT_TASK_AFTER_MS)]
    pub task_after_ms: u64,
}

/// The default of `--task-after-ms`: well above what a typical render takes,
/// well below the 20 s sync deadline and the clients' tool-call timeouts.
pub const DEFAULT_TASK_AFTER_MS: u64 = 2_000;

impl Default for JobsConfig {
    fn default() -> Self {
        JobsConfig {
            enabled: false,
            job_timeout_ms: 600_000,
            job_max_steps: None,
            max_running_jobs: None,
            max_jobs_per_principal: 4,
            max_jobs: 16,
            job_result_ttl_ms: 3_600_000,
            job_results_budget_bytes: 128 * 1024 * 1024,
            job_results_per_principal_bytes: None,
            tasks: false,
            task_after_ms: DEFAULT_TASK_AFTER_MS,
        }
    }
}

impl JobsConfig {
    /// The same settings, with async jobs on.
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    /// The deadline of one job.
    pub fn job_timeout(&self) -> Duration {
        Duration::from_millis(self.job_timeout_ms)
    }

    /// How long a Tasks client's call runs before it becomes a task; zero
    /// for at once.
    pub fn task_after(&self) -> Duration {
        Duration::from_millis(self.task_after_ms)
    }

    /// How long a finished job is kept.
    pub fn result_ttl(&self) -> Duration {
        Duration::from_millis(self.job_result_ttl_ms)
    }

    /// Jobs running at once, given `limits`' render slots.
    pub fn running_jobs(&self, limits: &LimitsConfig) -> usize {
        self.max_running_jobs
            .filter(|n| *n > 0)
            .unwrap_or_else(|| (limits.concurrency() / 2).max(1))
    }

    /// The largest result one job can hold: its artifacts as base64, its
    /// stdout, and [`JOB_MIN_CHARGE_BYTES`] for the rest.
    pub fn max_result_bytes(limits: &LimitsConfig) -> usize {
        (limits.max_output_bytes / 3 + 1)
            .saturating_mul(4)
            .saturating_add(limits.max_program_output_bytes)
            .saturating_add(JOB_MIN_CHARGE_BYTES)
    }

    /// The bytes of the results budget one principal may have reserved and
    /// held: the setting, or by default the budget less one largest result
    /// (about 117 of the 128 MiB), so that another principal can always
    /// start a job. A budget under two largest results cannot protect a
    /// second principal whatever the share, so then the share is the whole
    /// budget.
    pub fn principal_share_bytes(&self, limits: &LimitsConfig) -> usize {
        let budget = self.job_results_budget_bytes;
        self.job_results_per_principal_bytes.unwrap_or_else(|| {
            let largest = Self::max_result_bytes(limits);
            if budget >= largest.saturating_mul(2) {
                budget - largest
            } else {
                budget
            }
        })
    }

    /// The limits an async job runs under: the render's, with the job
    /// deadline as the render timeout (PSCat's own time limit follows it,
    /// unless it is turned off) and the job step limit.
    pub fn job_limits(&self, limits: &LimitsConfig) -> LimitsConfig {
        LimitsConfig {
            render_timeout_ms: self.job_timeout_ms,
            max_steps: match (limits.max_steps, self.job_max_steps) {
                (_, Some(n)) => n,
                (0, None) => 0,
                (n, None) => n.max(DEFAULT_JOB_MAX_STEPS),
            },
            interp_time_limit_ms: match limits.interp_time_limit_ms {
                Some(0) => Some(0),
                _ => None,
            },
            ..limits.clone()
        }
    }

    /// Reject settings that cannot work with `limits`.
    pub fn check(&self, limits: &LimitsConfig) -> Result<(), String> {
        if self.tasks && !self.enabled {
            return Err("--tasks needs async jobs (--async-jobs over HTTP)".into());
        }
        // Settings for jobs that are off cannot stop the server starting.
        if !self.enabled {
            return Ok(());
        }
        if self.job_timeout_ms == 0
            || self.max_jobs_per_principal == 0
            || self.max_jobs == 0
            || self.job_result_ttl_ms == 0
        {
            return Err(
                "job-timeout-ms, max-jobs-per-principal, max-jobs and job-result-ttl-ms must be \
                 positive"
                    .into(),
            );
        }
        if self.tasks && self.task_after_ms > 0 && self.task_after_ms >= limits.render_timeout_ms {
            return Err(format!(
                "task-after-ms ({}) must be below render-timeout-ms ({}): a call would reach \
                 the sync deadline before it could become a task (0 makes every call a task at \
                 once)",
                self.task_after_ms, limits.render_timeout_ms
            ));
        }
        // A Tasks call's worker starts under the job limits, before anyone
        // knows whether it will be quick: they must be no tighter than a
        // synchronous call's, or a quick call would be held to less.
        if self.tasks && self.task_after_ms > 0 {
            if self.job_timeout_ms < limits.render_timeout_ms {
                return Err(format!(
                    "with --tasks, job-timeout-ms ({}) must be at least render-timeout-ms ({}): a \
                     Tasks client's call starts under the job limits",
                    self.job_timeout_ms, limits.render_timeout_ms
                ));
            }
            let (sync, job) = (limits.max_steps, self.job_limits(limits).max_steps);
            if job != 0 && (sync == 0 || job < sync) {
                return Err(format!(
                    "with --tasks, the job step limit ({job}) must be at least max-steps ({sync}, \
                     0 for none): a Tasks client's call starts under the job limits"
                ));
            }
        }
        let slots = limits.concurrency();
        let running = self.running_jobs(limits);
        if slots > 1 && running >= slots {
            return Err(format!(
                "max-running-jobs ({running}) must be below max-concurrent-renders ({slots}), so \
                 synchronous calls always have a render slot"
            ));
        }
        if slots == 1 && running > 1 {
            return Err("with one render slot, max-running-jobs must be 1".into());
        }
        let largest = Self::max_result_bytes(limits);
        if self.job_results_budget_bytes < largest {
            return Err(format!(
                "job-results-budget-bytes ({}) must hold at least one largest result ({largest} \
                 bytes: max-output-bytes as base64, plus stdout)",
                self.job_results_budget_bytes
            ));
        }
        if let Some(share) = self.job_results_per_principal_bytes {
            if share < largest {
                return Err(format!(
                    "job-results-per-principal-bytes ({share}) must hold at least one largest \
                     result ({largest} bytes), or a principal could never start a job"
                ));
            }
            if share > self.job_results_budget_bytes {
                return Err(format!(
                    "job-results-per-principal-bytes ({share}) must not exceed \
                     job-results-budget-bytes ({}); set it equal to turn the share off",
                    self.job_results_budget_bytes
                ));
            }
        }
        Ok(())
    }
}

/// The default step limit of an async job: PSCat's for untrusted programs
/// (`pscat::limits::UNTRUSTED_MAX_STEPS`), the PSCat plugin's.
pub const DEFAULT_JOB_MAX_STEPS: u64 = 25_000_000_000;

/// The least a finished job is charged against the results budget, for its
/// status, message and bookkeeping.
pub const JOB_MIN_CHARGE_BYTES: usize = 4096;

/// `ms` less a margin of 10%, at least 250 ms, and never below 1 ms.
fn below_with_margin(ms: u64) -> Duration {
    let margin = (ms / 10).max(250);
    Duration::from_millis(ms.saturating_sub(margin).max(1))
}

/// How the render worker is started.
#[derive(Debug, Clone, Args, Default)]
pub struct WorkerConfig {
    /// Executable started for each render with the `render-worker`
    /// subcommand. Defaults to this server's own executable.
    #[arg(long, env = "PSCAT_MCP_WORKER_EXE")]
    pub worker_exe: Option<PathBuf>,

    /// A PSCat data root the worker reads font packs from: packs live in
    /// `DIR/fonts/packs/<pack>/`, the layout `PSCAT_HOME=DIR pscat fonts
    /// install` writes. Passed to the worker, and only the worker, as
    /// `PSCAT_HOME`; the worker's other environment stays empty. Read-only
    /// by construction: programs still cannot read or write files there.
    /// Unset (the default): only PSCat's compiled-in fonts.
    #[arg(long, env = "PSCAT_MCP_FONT_PACK_DIR")]
    pub font_pack_dir: Option<PathBuf>,
}

impl WorkerConfig {
    /// The executable to start.
    pub fn exe(&self) -> std::io::Result<PathBuf> {
        match &self.worker_exe {
            Some(p) => Ok(p.clone()),
            None => std::env::current_exe(),
        }
    }

    /// The font-pack directory to pass to the worker: absolute and
    /// canonical (the worker runs in `/`, so a relative path would resolve
    /// there). `Ok(None)` when unset or empty. An error, naming the flag and
    /// the variable, when the path does not exist or is not a directory.
    pub fn font_pack_dir(&self) -> Result<Option<PathBuf>, String> {
        let Some(dir) = self
            .font_pack_dir
            .as_ref()
            .filter(|d| !d.as_os_str().is_empty())
        else {
            return Ok(None);
        };
        let canonical = std::fs::canonicalize(dir).map_err(|e| {
            format!(
                "--font-pack-dir / PSCAT_MCP_FONT_PACK_DIR {}: {e}",
                dir.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "--font-pack-dir / PSCAT_MCP_FONT_PACK_DIR {} is not a directory",
                dir.display()
            ));
        }
        Ok(Some(canonical))
    }
}

/// HTTP transport settings.
#[derive(Debug, Clone, Args)]
pub struct HttpConfig {
    /// Address to bind. Containers set 0.0.0.0.
    #[arg(long, env = "PSCAT_MCP_BIND", default_value = "127.0.0.1")]
    pub bind: IpAddr,

    /// Port to listen on (Cloud Run sets `PORT`).
    #[arg(long, env = "PORT", default_value_t = 8080)]
    pub port: u16,

    /// Bearer tokens accepted on `/mcp`, comma-separated. Prefer
    /// `--auth-token-file` so tokens stay out of process listings.
    #[arg(
        long,
        env = "PSCAT_MCP_AUTH_TOKENS",
        hide_env_values = true,
        value_delimiter = ','
    )]
    pub auth_tokens: Vec<String>,

    /// File holding bearer tokens, one per line (a mounted secret).
    #[arg(long, env = "PSCAT_MCP_AUTH_TOKEN_FILE")]
    pub auth_token_file: Option<PathBuf>,

    /// Serve `/mcp` without authentication. Refused unless the bind address
    /// is loopback: for local development only.
    #[arg(long, env = "PSCAT_MCP_ALLOW_UNAUTHENTICATED", default_value_t = false)]
    pub allow_unauthenticated: bool,

    /// Host header values accepted (DNS-rebinding protection), comma
    /// separated, e.g. `pscat-mcp-abc.a.run.app`. `*` accepts any host —
    /// only for deployments behind a proxy that already checks it.
    #[arg(
        long,
        env = "PSCAT_MCP_ALLOWED_HOSTS",
        value_delimiter = ',',
        default_value = "localhost,127.0.0.1,::1"
    )]
    pub allowed_hosts: Vec<String>,

    /// Browser origins accepted, comma separated. Empty: requests carrying
    /// an `Origin` header are rejected (MCP clients do not send one).
    #[arg(long, env = "PSCAT_MCP_ALLOWED_ORIGINS", value_delimiter = ',')]
    pub allowed_origins: Vec<String>,

    /// Seconds to wait for in-flight requests on SIGTERM before exiting.
    #[arg(long, env = "PSCAT_MCP_SHUTDOWN_GRACE_SECS", default_value_t = 8)]
    pub shutdown_grace_secs: u64,

    /// Serve async render jobs (`start_render`, `render_status`,
    /// `render_result`, `cancel_render`, and the Tasks extension with
    /// `--tasks`). Jobs and results live in this process's memory, so this
    /// only works on a single instance whose CPU stays allocated between
    /// requests: not on an autoscaled or request-billed Cloud Run service.
    #[arg(long, env = "PSCAT_MCP_ASYNC_JOBS", default_value_t = false)]
    pub async_jobs: bool,
}

impl HttpConfig {
    /// Socket address to bind.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }

    /// Tokens from `--auth-tokens` and `--auth-token-file`, trimmed, empty
    /// lines and `#` comments skipped.
    pub fn load_tokens(&self) -> std::io::Result<Vec<String>> {
        let mut tokens: Vec<String> = self
            .auth_tokens
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if let Some(path) = &self.auth_token_file {
            let text = std::fs::read_to_string(path)?;
            tokens.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_string),
            );
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interp_time_limit_sits_below_the_kill() {
        let l = LimitsConfig::default();
        assert_eq!(l.interp_time_limit(), Some(Duration::from_millis(18_000)));
        assert!(l.check().is_ok());
        let short = LimitsConfig {
            render_timeout_ms: 1000,
            ..LimitsConfig::default()
        };
        assert_eq!(short.interp_time_limit(), Some(Duration::from_millis(750)));
        let off = LimitsConfig {
            interp_time_limit_ms: Some(0),
            ..LimitsConfig::default()
        };
        assert_eq!(off.interp_time_limit(), None);
        let bad = LimitsConfig {
            interp_time_limit_ms: Some(30_000),
            ..LimitsConfig::default()
        };
        assert!(bad.check().is_err());
    }

    #[test]
    fn the_svg_text_limit_defaults_below_the_output_cap_and_never_exceeds_it() {
        let l = LimitsConfig::default();
        assert_eq!(l.svg_blob_threshold_bytes(), 40 * 1024);
        assert!(l.check().is_ok());
        // A small output cap pulls the default down with it.
        let small = LimitsConfig {
            max_output_bytes: 6000,
            ..LimitsConfig::default()
        };
        assert_eq!(small.svg_blob_threshold_bytes(), 6000);
        assert!(small.check().is_ok());
        // An explicit cap above the output cap is refused, not clamped:
        // the operator asked for something the server cannot do.
        let over = LimitsConfig {
            svg_blob_threshold_bytes: Some(l.max_output_bytes + 1),
            ..LimitsConfig::default()
        };
        let e = over.check().unwrap_err();
        assert!(
            e.contains("svg-blob-threshold-bytes") && e.contains("max-output-bytes"),
            "{e}"
        );
        let at = LimitsConfig {
            svg_blob_threshold_bytes: Some(l.max_output_bytes),
            ..LimitsConfig::default()
        };
        assert!(at.check().is_ok());
        // Zero is allowed: every SVG is returned as a blob.
        let zero = LimitsConfig {
            svg_blob_threshold_bytes: Some(0),
            ..LimitsConfig::default()
        };
        assert!(zero.check().is_ok());
    }

    #[test]
    fn the_deprecated_svg_alias_works_alone_and_is_refused_beside_the_new_name() {
        let old = LimitsConfig {
            deprecated_max_svg_bytes: Some(20_000),
            ..LimitsConfig::default()
        };
        assert_eq!(old.svg_blob_threshold_bytes(), 20_000);
        assert!(old.check().is_ok());
        assert!(old.deprecation_warning().is_some());
        let new = LimitsConfig {
            svg_blob_threshold_bytes: Some(20_000),
            ..LimitsConfig::default()
        };
        assert_eq!(new.svg_blob_threshold_bytes(), 20_000);
        assert!(new.check().is_ok());
        assert!(new.deprecation_warning().is_none());
        assert!(LimitsConfig::default().deprecation_warning().is_none());
        let both = LimitsConfig {
            svg_blob_threshold_bytes: Some(20_000),
            deprecated_max_svg_bytes: Some(10_000),
            ..LimitsConfig::default()
        };
        let e = both.check().unwrap_err();
        assert!(
            e.contains("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES")
                && e.contains("PSCAT_MCP_MAX_SVG_BYTES")
                && e.contains("--svg-blob-threshold-bytes")
                && e.contains("--max-svg-bytes")
                && e.contains("not both"),
            "{e}"
        );
    }

    #[test]
    fn sheet_limits_default_to_the_render_limits() {
        let l = LimitsConfig::default();
        assert_eq!(l.max_sheet_cells, 32);
        assert_eq!(l.sheet_source_bytes(), l.max_source_bytes);
        assert_eq!(l.sheet_cell_pixels(), l.max_pixels);
        assert_eq!(l.sheet_pixels(), l.max_pixels);
        assert_eq!(l.sheet_timeout(), l.render_timeout());
        assert_eq!(l.sheet_time_budget(), Duration::from_millis(18_000));
        assert_eq!(l.max_text_bytes(), 256 * 1024);
        let big = LimitsConfig {
            max_sheet_source_bytes: Some(1 << 20),
            max_pixels: 1000,
            max_sheet_timeout_ms: Some(60_000),
            ..LimitsConfig::default()
        };
        assert_eq!(big.max_text_bytes(), 1 << 20);
        assert_eq!(big.sheet_pixels(), 1000);
        // A page limit past what a sheet can be does not make the sheet's
        // default invalid.
        let huge = LimitsConfig {
            max_pixels: 100_000_000,
            ..LimitsConfig::default()
        };
        assert_eq!(huge.sheet_pixels(), 64_000_000);
        assert!(huge.check().is_ok());
        assert_eq!(big.sheet_time_budget(), Duration::from_millis(54_000));
        for bad in [
            LimitsConfig {
                max_sheet_cells: 0,
                ..LimitsConfig::default()
            },
            LimitsConfig {
                max_sheet_pixels: Some(0),
                ..LimitsConfig::default()
            },
            LimitsConfig {
                max_sheet_timeout_ms: Some(0),
                ..LimitsConfig::default()
            },
            LimitsConfig {
                max_sheet_pixels: Some(64_000_001),
                ..LimitsConfig::default()
            },
        ] {
            assert!(bad.check().is_err());
        }
    }

    #[test]
    fn job_settings() {
        let l = LimitsConfig {
            max_concurrent_renders: Some(4),
            ..LimitsConfig::default()
        };
        let j = JobsConfig::default().enabled();
        assert!(j.check(&l).is_ok());
        assert_eq!(j.running_jobs(&l), 2);
        assert_eq!(DEFAULT_JOB_MAX_STEPS, pscat::limits::UNTRUSTED_MAX_STEPS);
        // The job limits: the job deadline, PSCat's time limit below it, the
        // job step limit; everything else as for a render.
        let jl = j.job_limits(&l);
        assert_eq!(jl.render_timeout(), Duration::from_secs(600));
        assert_eq!(jl.interp_time_limit(), Some(Duration::from_secs(540)));
        assert_eq!(jl.max_steps, DEFAULT_JOB_MAX_STEPS);
        assert_eq!(jl.max_pixels, l.max_pixels);
        assert!(jl.check().is_ok());
        let explicit = LimitsConfig {
            interp_time_limit_ms: Some(5_000),
            max_steps: 0,
            ..l.clone()
        };
        assert_eq!(
            j.job_limits(&explicit).interp_time_limit(),
            Some(Duration::from_secs(540))
        );
        assert_eq!(j.job_limits(&explicit).max_steps, 0);
        let off = LimitsConfig {
            interp_time_limit_ms: Some(0),
            ..l.clone()
        };
        assert_eq!(j.job_limits(&off).interp_time_limit(), None);
        let steps = JobsConfig {
            job_max_steps: Some(7),
            ..j.clone()
        };
        assert_eq!(steps.job_limits(&l).max_steps, 7);

        // The per-principal share: by default the budget less one largest
        // result (the whole budget when it holds fewer than two), or the
        // setting, which can run from one largest result up to the budget.
        let largest = JobsConfig::max_result_bytes(&l);
        assert_eq!(
            j.principal_share_bytes(&l),
            j.job_results_budget_bytes - largest
        );
        let tight = |budget| JobsConfig {
            job_results_budget_bytes: budget,
            ..j.clone()
        };
        assert_eq!(tight(2 * largest).principal_share_bytes(&l), largest);
        assert_eq!(
            tight(2 * largest - 1).principal_share_bytes(&l),
            2 * largest - 1
        );
        assert_eq!(tight(largest).principal_share_bytes(&l), largest);
        for share in [largest, j.job_results_budget_bytes] {
            let set = JobsConfig {
                job_results_per_principal_bytes: Some(share),
                ..j.clone()
            };
            assert_eq!(set.principal_share_bytes(&l), share);
            assert!(set.check(&l).is_ok(), "{share}");
        }
        // Off with jobs off, like every job setting.
        assert!(
            JobsConfig {
                job_results_per_principal_bytes: Some(0),
                ..JobsConfig::default()
            }
            .check(&l)
            .is_ok()
        );

        // Jobs stay below the render slots, and the budget holds one result.
        for bad in [
            JobsConfig {
                max_running_jobs: Some(4),
                ..j.clone()
            },
            JobsConfig {
                job_results_budget_bytes: JobsConfig::max_result_bytes(&l) - 1,
                ..j.clone()
            },
            JobsConfig {
                job_results_per_principal_bytes: Some(0),
                ..j.clone()
            },
            JobsConfig {
                job_results_per_principal_bytes: Some(JobsConfig::max_result_bytes(&l) - 1),
                ..j.clone()
            },
            JobsConfig {
                job_results_per_principal_bytes: Some(j.job_results_budget_bytes + 1),
                ..j.clone()
            },
            JobsConfig {
                max_jobs: 0,
                ..j.clone()
            },
            JobsConfig {
                tasks: true,
                ..JobsConfig::default()
            },
            // A call would reach the sync deadline before it could become
            // a task.
            JobsConfig {
                tasks: true,
                task_after_ms: l.render_timeout_ms,
                ..j.clone()
            },
            // A Tasks call starts under the job limits: no tighter than the
            // sync ones.
            JobsConfig {
                tasks: true,
                job_timeout_ms: l.render_timeout_ms - 1,
                ..j.clone()
            },
            JobsConfig {
                tasks: true,
                job_max_steps: Some(l.max_steps - 1),
                ..j.clone()
            },
        ] {
            assert!(bad.check(&l).is_err(), "{bad:?}");
        }
        // The task threshold: below the sync deadline, or 0 (a task at
        // once); without --tasks it is not checked.
        for (tasks, after) in [
            (true, DEFAULT_TASK_AFTER_MS),
            (true, l.render_timeout_ms - 1),
            (true, 0),
            (false, l.render_timeout_ms),
        ] {
            let set = JobsConfig {
                tasks,
                task_after_ms: after,
                ..j.clone()
            };
            assert!(set.check(&l).is_ok(), "{set:?}");
        }
        assert!(DEFAULT_TASK_AFTER_MS < LimitsConfig::default().render_timeout_ms);
        // Job limits equal to the sync ones are fine, and so is an unlimited
        // job step count; a limited one under unlimited sync steps is not.
        let unlimited_steps = LimitsConfig {
            max_steps: 0,
            ..l.clone()
        };
        for (limits, job_max_steps, ok) in [
            (&l, Some(l.max_steps), true),
            (&l, Some(0), true),
            (&unlimited_steps, None, true),
            (&unlimited_steps, Some(7), false),
        ] {
            let set = JobsConfig {
                tasks: true,
                job_max_steps,
                job_timeout_ms: l.render_timeout_ms,
                ..j.clone()
            };
            assert_eq!(set.check(limits).is_ok(), ok, "{set:?}");
            // Without --tasks no Tasks call starts under them.
            let off = JobsConfig {
                tasks: false,
                ..set
            };
            assert!(off.check(limits).is_ok(), "{off:?}");
        }
        // With jobs off (HTTP by default) none of their settings matter,
        // even with an output limit larger than the default budget.
        let big_output = LimitsConfig {
            max_output_bytes: 512 * 1024 * 1024,
            ..l.clone()
        };
        assert!(JobsConfig::default().check(&big_output).is_ok());
        assert!(j.check(&big_output).is_err());
        let one = LimitsConfig {
            max_concurrent_renders: Some(1),
            ..LimitsConfig::default()
        };
        assert_eq!(j.running_jobs(&one), 1);
        assert!(j.check(&one).is_ok());
        assert!(
            JobsConfig {
                max_running_jobs: Some(2),
                ..j.clone()
            }
            .check(&one)
            .is_err()
        );
    }

    #[test]
    fn concurrency_defaults_are_bounded() {
        let l = LimitsConfig::default();
        assert!((1..=4).contains(&l.concurrency()));
        let one = LimitsConfig {
            max_concurrent_renders: Some(1),
            ..LimitsConfig::default()
        };
        assert_eq!(one.concurrency(), 1);
    }

    #[test]
    fn the_font_pack_dir_is_checked_and_made_absolute() {
        let unset = WorkerConfig::default();
        assert_eq!(unset.font_pack_dir(), Ok(None));
        let empty = WorkerConfig {
            font_pack_dir: Some(PathBuf::new()),
            ..WorkerConfig::default()
        };
        assert_eq!(empty.font_pack_dir(), Ok(None));

        let dir = std::env::temp_dir();
        let ok = WorkerConfig {
            font_pack_dir: Some(dir.clone()),
            ..WorkerConfig::default()
        };
        let got = ok.font_pack_dir().unwrap().unwrap();
        assert!(got.is_absolute());
        assert_eq!(got, std::fs::canonicalize(&dir).unwrap());

        // A relative path is made absolute: the worker runs in `/`.
        let relative = WorkerConfig {
            font_pack_dir: Some(PathBuf::from(".")),
            ..WorkerConfig::default()
        };
        assert!(relative.font_pack_dir().unwrap().unwrap().is_absolute());

        let missing = WorkerConfig {
            font_pack_dir: Some(dir.join("pscat-mcp-no-such-dir")),
            ..WorkerConfig::default()
        };
        let err = missing.font_pack_dir().unwrap_err();
        assert!(err.contains("PSCAT_MCP_FONT_PACK_DIR"), "{err}");
        assert!(err.contains("--font-pack-dir"), "{err}");

        let file = WorkerConfig {
            font_pack_dir: Some(PathBuf::from(file!())),
            ..WorkerConfig::default()
        };
        // `file!()` is relative to the workspace, which is the test's cwd.
        let err = file.font_pack_dir().unwrap_err();
        assert!(err.contains("is not a directory"), "{err}");
    }
}
