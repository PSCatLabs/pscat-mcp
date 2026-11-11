//! The render worker: the only code that executes PostScript.
//!
//! It runs in a short-lived child process (`pscat-mcp-server render-worker`),
//! one job per process. The parent writes one [`WorkerJob`] as JSON on stdin
//! and reads one [`WorkerResult`] as JSON from stdout. Running each program in
//! its own process is what makes the limits enforceable:
//!
//! * PSCat's memory limit measures heap growth of the *whole process*
//!   (through `pscat::TrackingAllocator`), which is only meaningful with one
//!   program per process.
//! * PSCat checks its limits between interpreter steps, so a single long
//!   operator runs to completion; the parent's kill is the backstop.
//! * The parent can apply OS limits (`setrlimit`) to the child alone.
//!
//! Inside the worker the interpreter is configured for untrusted input:
//! reads are confined to PSCat's compiled-in `lib/` (no disk paths at all),
//! no write roots are set (all disk writes fail with `invalidfileaccess`),
//! program stdout is captured in memory rather than written to the real
//! stdout (which carries the result), and interpreter stderr is silenced.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Output format the worker can produce. These are the three formats PSCat's
/// library supports (`Page::to_png`, `Interp::svg_pages`,
/// `Interp::pdf_document`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// One PNG per page.
    Png,
    /// One SVG document per page.
    Svg,
    /// One PDF document holding every page.
    Pdf,
}

impl OutputFormat {
    /// Every supported format.
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Png, OutputFormat::Svg, OutputFormat::Pdf];

    /// Lower-case name used on the wire.
    pub fn name(self) -> &'static str {
        match self {
            OutputFormat::Png => "png",
            OutputFormat::Svg => "svg",
            OutputFormat::Pdf => "pdf",
        }
    }

    /// The MIME type of one artifact in this format.
    pub fn mime_type(self) -> &'static str {
        match self {
            OutputFormat::Png => "image/png",
            OutputFormat::Svg => "image/svg+xml",
            OutputFormat::Pdf => "application/pdf",
        }
    }

    /// Parse a user-supplied name.
    pub fn parse(name: &str) -> Option<Self> {
        OutputFormat::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// What the worker is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobMode {
    /// Run the program and return its pages.
    Render,
    /// Scan the program, run it, and return diagnostics only.
    Validate,
    /// Run no program: return PSCat's art catalog (`capabilities` in the
    /// result). Every other job field is ignored.
    Capabilities,
}

/// Interpreter-level limits applied inside the worker by PSCat itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InterpLimits {
    /// `Limits::max_steps`; `None` disables it.
    pub max_steps: Option<u64>,
    /// `Limits::max_time` in milliseconds; `None` disables it.
    pub max_time_ms: Option<u64>,
    /// `Limits::max_memory` in bytes; `None` disables it.
    pub max_memory_bytes: Option<usize>,
    /// `Interp::set_max_page_pixels`: caps pages made by `setpagedevice`.
    pub max_page_pixels: u64,
    /// Most pages returned; a program producing more fails.
    pub max_pages: usize,
    /// Most bytes of raw artifact data (all pages together).
    pub max_output_bytes: usize,
    /// Most bytes of captured program stdout returned (the rest is cut).
    pub max_program_output_bytes: usize,
}

/// One job for the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerJob {
    /// Render or validate.
    pub mode: JobMode,
    /// PostScript program text.
    pub source: String,
    /// Output format (ignored for validation).
    pub format: OutputFormat,
    /// Explicit page size in points; `None` uses PSCat's default page (US
    /// Letter), which the program may change with `setpagedevice`.
    pub page_points: Option<(u32, u32)>,
    /// Resolution in dots per inch (72 = one pixel per point).
    pub dpi: f64,
    /// Screen each PNG page like a mono laser printer (PNG only; the
    /// request layer refuses it for other formats).
    #[serde(default)]
    pub halftone: bool,
    /// Interpreter limits.
    pub limits: InterpLimits,
}

/// One rendered artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// 1-based page number; `None` for a document holding every page (PDF).
    pub page: Option<usize>,
    /// Width in device pixels.
    pub width_px: u32,
    /// Height in device pixels.
    pub height_px: u32,
    /// Base64 of the artifact bytes.
    pub data_base64: String,
    /// Length of the decoded bytes.
    pub byte_len: usize,
}

/// A PostScript error raised by the program.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PsDiagnostic {
    /// PLRM error name (`undefined`, `syntaxerror`, ...), or `limitexceeded`.
    pub name: String,
    /// Which interpreter limit was passed (`steps`, `time`, `memory`).
    pub limit: Option<String>,
    /// PSCat's LaserWriter-style report, naming the command and line.
    pub report: String,
}

/// A heuristic finding from PSCat's lint checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LintNote {
    /// Stable check slug (`blank-page`, `stack-leak`, ...).
    pub check: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Result of a lexical scan that executes nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyntaxScan {
    /// No tokenizer error and balanced procedure braces.
    pub ok: bool,
    /// Tokens read before the scan stopped.
    pub tokens: usize,
    /// What went wrong, if anything.
    pub error: Option<String>,
}

/// Why the worker could not produce a normal result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkerFailure {
    /// The program raised a PostScript error (or passed an interpreter limit).
    PostScript,
    /// More pages than `max_pages`.
    TooManyPages {
        /// Pages produced.
        pages: usize,
        /// The limit.
        max: usize,
    },
    /// Artifact bytes over `max_output_bytes`.
    OutputTooLarge {
        /// Bytes reached.
        bytes: usize,
        /// The limit.
        max: usize,
    },
    /// Encoding a page failed.
    Encode {
        /// Encoder error.
        message: String,
    },
    /// The job itself was unusable.
    BadJob {
        /// What was wrong.
        message: String,
    },
}

/// The worker's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    /// `None` on success.
    pub failure: Option<WorkerFailure>,
    /// The PostScript error, when the program raised one.
    pub ps_error: Option<PsDiagnostic>,
    /// Rendered artifacts (render mode, success only).
    pub artifacts: Vec<Artifact>,
    /// Pages the program produced.
    pub page_count: usize,
    /// Lexical scan (validate mode only).
    pub syntax: Option<SyntaxScan>,
    /// Lint findings (validate mode only).
    pub lint: Vec<LintNote>,
    /// Captured program stdout (`print`, `=`, `==`), possibly truncated.
    pub program_output: String,
    /// Whether `program_output` was cut.
    pub program_output_truncated: bool,
    /// Interpreter steps counted by PSCat.
    pub steps_used: u64,
    /// Time spent executing the program, in milliseconds.
    pub elapsed_ms: u64,
    /// PSCat's art catalog (capabilities mode only).
    #[serde(default)]
    pub capabilities: Option<serde_json::Value>,
}

impl WorkerResult {
    pub(crate) fn empty() -> Self {
        WorkerResult {
            failure: None,
            ps_error: None,
            artifacts: Vec::new(),
            page_count: 0,
            syntax: None,
            lint: Vec::new(),
            program_output: String::new(),
            program_output_truncated: false,
            steps_used: 0,
            elapsed_ms: 0,
            capabilities: None,
        }
    }

    pub(crate) fn fail(failure: WorkerFailure) -> Self {
        WorkerResult {
            failure: Some(failure),
            ..WorkerResult::empty()
        }
    }
}

/// Entry point of the `render-worker` subcommand: read one job from stdin,
/// write one result to stdout. Returns the process exit code.
pub fn run_stdio(max_job_bytes: usize) -> i32 {
    let mut input = Vec::new();
    let read = std::io::stdin()
        .lock()
        .take(max_job_bytes as u64 + 1)
        .read_to_end(&mut input);
    let result = match read {
        Err(e) => WorkerResult::fail(WorkerFailure::BadJob {
            message: format!("reading the job failed: {e}"),
        }),
        Ok(_) if input.len() > max_job_bytes => WorkerResult::fail(WorkerFailure::BadJob {
            message: "job is larger than the worker accepts".into(),
        }),
        Ok(_) => match serde_json::from_slice::<WorkerJob>(&input) {
            Ok(job) => {
                drop(input);
                execute(&job)
            }
            Err(e) => WorkerResult::fail(WorkerFailure::BadJob {
                message: format!("job is not valid JSON: {e}"),
            }),
        },
    };
    let mut out = std::io::stdout().lock();
    let written = serde_json::to_writer(&mut out, &result).is_ok() && out.flush().is_ok();
    if written { 0 } else { 3 }
}

/// Scan `source` with PSCat's tokenizer without executing anything, and
/// check that procedure braces balance.
pub fn syntax_scan(source: &[u8]) -> SyntaxScan {
    crate::pscat_extras::syntax_scan(source)
}

fn configure(job: &WorkerJob) -> Result<pscat::Interp, String> {
    let scale = (job.dpi / 72.0) as f32;
    let mut interp = match job.page_points {
        Some((w, h)) => pscat::Interp::with_page_scaled(w, h, scale).ok_or_else(|| {
            format!(
                "page {w}x{h} points at {} dpi is not a usable size",
                job.dpi
            )
        })?,
        None if (job.dpi - 72.0).abs() < f64::EPSILON => pscat::Interp::new(),
        None => {
            let mut interp = pscat::Interp::with_page_scaled(612, 792, scale).ok_or_else(|| {
                format!("the default page at {} dpi is not a usable size", job.dpi)
            })?;
            // Like `Interp::new()`: nothing chose this size, so the program's
            // `setpagedevice` may replace it (bounded by max_page_pixels).
            crate::pscat_extras::allow_program_page_size(&mut interp);
            interp
        }
    };
    let l = &job.limits;
    let mut limits = pscat::Limits::none();
    if let Some(steps) = l.max_steps {
        limits = limits.with_max_steps(steps);
    }
    if let Some(ms) = l.max_time_ms {
        limits = limits.with_max_time(Duration::from_millis(ms));
    }
    if let Some(bytes) = l.max_memory_bytes {
        limits = limits.with_max_memory(bytes);
    }
    interp.set_limits(limits).map_err(|e| e.to_string())?;
    // An empty slice confines reads to the copy of lib/ compiled into PSCat.
    interp.set_read_roots::<&str>(&[])?;
    interp.set_write_roots::<&str>(&[])?;
    interp.set_max_write_bytes(Some(0));
    interp.set_max_page_pixels(Some(l.max_page_pixels));
    interp.capture_stdout();
    interp.quiet_stderr();
    if job.mode == JobMode::Render {
        match job.format {
            OutputFormat::Svg => interp.enable_svg(),
            OutputFormat::Pdf => interp.enable_pdf(),
            OutputFormat::Png => {}
        }
    }
    Ok(interp)
}

/// Run one job in this process. Public so unit tests can exercise the
/// interpreter configuration directly; production code reaches it only
/// through the `render-worker` subprocess.
pub fn execute(job: &WorkerJob) -> WorkerResult {
    let mut result = WorkerResult::empty();
    if job.mode == JobMode::Capabilities {
        let started = Instant::now();
        result.capabilities = Some(crate::pscat_extras::capabilities_json());
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        return result;
    }
    if job.mode == JobMode::Validate {
        result.syntax = Some(syntax_scan(job.source.as_bytes()));
    }
    let mut interp = match configure(job) {
        Ok(interp) => interp,
        Err(message) => return WorkerResult::fail(WorkerFailure::BadJob { message }),
    };

    let started = Instant::now();
    let run = interp.run_source(job.source.as_bytes());
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    result.steps_used = interp.steps_used();

    let stdout = interp.captured_stdout();
    let cap = job.limits.max_program_output_bytes;
    result.program_output_truncated = stdout.len() > cap;
    result.program_output = String::from_utf8_lossy(&stdout[..stdout.len().min(cap)]).into_owned();

    if let Err(err) = &run {
        result.ps_error = Some(PsDiagnostic {
            name: err.name().to_string(),
            limit: err.limit().map(|l| l.name().to_string()),
            report: interp.error_report(err),
        });
    }

    if job.mode == JobMode::Validate {
        // Limit errors stop the program midway; lint on a truncated run
        // would report noise, so only lint programs that ran to the end or
        // raised an ordinary PostScript error.
        let limited = run.as_ref().err().and_then(|e| e.limit()).is_some();
        if !limited {
            result.lint = interp
                .lint(true)
                .into_iter()
                .map(|f| LintNote {
                    check: f.check.to_string(),
                    message: f.message,
                })
                .collect();
        }
        result.page_count = interp.pages().len();
        if run.is_err() {
            result.failure = Some(WorkerFailure::PostScript);
        }
        return result;
    }

    if run.is_err() {
        // Never return a partial page as if it were the requested render.
        result.failure = Some(WorkerFailure::PostScript);
        return result;
    }

    match encode(&interp, job) {
        Ok((artifacts, pages)) => {
            result.artifacts = artifacts;
            result.page_count = pages;
        }
        Err(failure) => result.failure = Some(failure),
    }
    result
}

fn encode(
    interp: &pscat::Interp,
    job: &WorkerJob,
) -> Result<(Vec<Artifact>, usize), WorkerFailure> {
    let pages = interp.pages();
    let max_pages = job.limits.max_pages;
    if pages.len() > max_pages {
        return Err(WorkerFailure::TooManyPages {
            pages: pages.len(),
            max: max_pages,
        });
    }
    let max_bytes = job.limits.max_output_bytes;
    let mut total = 0usize;
    let mut check = |n: usize| -> Result<(), WorkerFailure> {
        total = total.saturating_add(n);
        if total > max_bytes {
            Err(WorkerFailure::OutputTooLarge {
                bytes: total,
                max: max_bytes,
            })
        } else {
            Ok(())
        }
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut artifacts = Vec::new();
    match job.format {
        OutputFormat::Png if job.halftone => {
            // Screened here, in the worker, one page at a time so only one
            // extra raster is alive at once.
            for (i, shot) in
                crate::pscat_extras::halftone_pngs(interp, (job.dpi / 72.0) as f32).enumerate()
            {
                let shot = shot.map_err(|e| WorkerFailure::Encode {
                    message: e.to_string(),
                })?;
                check(shot.png.len())?;
                artifacts.push(Artifact {
                    page: Some(i + 1),
                    width_px: shot.width,
                    height_px: shot.height,
                    byte_len: shot.png.len(),
                    data_base64: b64.encode(&shot.png),
                });
            }
        }
        OutputFormat::Png => {
            for (i, page) in pages.iter().enumerate() {
                let png = page.to_png().map_err(|e| WorkerFailure::Encode {
                    message: e.to_string(),
                })?;
                check(png.len())?;
                artifacts.push(Artifact {
                    page: Some(i + 1),
                    width_px: page.width(),
                    height_px: page.height(),
                    byte_len: png.len(),
                    data_base64: b64.encode(&png),
                });
            }
        }
        OutputFormat::Svg => {
            let svgs = interp.svg_pages().ok_or_else(|| WorkerFailure::Encode {
                message: "SVG recording was not enabled".into(),
            })?;
            for (i, svg) in svgs.iter().enumerate() {
                check(svg.len())?;
                let (w, h) = pages.get(i).map_or((0, 0), |p| (p.width(), p.height()));
                artifacts.push(Artifact {
                    page: Some(i + 1),
                    width_px: w,
                    height_px: h,
                    byte_len: svg.len(),
                    data_base64: b64.encode(svg.as_bytes()),
                });
            }
        }
        OutputFormat::Pdf => {
            let pdf = interp.pdf_document().ok_or_else(|| WorkerFailure::Encode {
                message: "PDF recording was not enabled".into(),
            })?;
            check(pdf.len())?;
            let (w, h) = pages.first().map_or((0, 0), |p| (p.width(), p.height()));
            artifacts.push(Artifact {
                page: None,
                width_px: w,
                height_px: h,
                byte_len: pdf.len(),
                data_base64: b64.encode(&pdf),
            });
        }
    }
    Ok((artifacts, pages.len()))
}
