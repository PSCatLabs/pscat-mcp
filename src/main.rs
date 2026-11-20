//! `pscat-mcp-server`: `serve-http`, `serve-stdio`, and the internal
//! `render-worker` subcommand.

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use pscat_mcp::capture::CaptureConfig;
use pscat_mcp::config::{
    ArtifactConfig, AuthConfig, HttpConfig, JobsConfig, LimitsConfig, WorkerConfig,
};
use pscat_mcp::telemetry::{self, LogFormat};

// PSCat's memory limit needs its counting allocator. Each render runs in its
// own worker process, so the count covers exactly one program.
#[global_allocator]
static ALLOC: pscat::TrackingAllocator = pscat::TrackingAllocator;

/// Largest job the worker reads from stdin (JSON-escaped source + framing).
const MAX_WORKER_JOB_BYTES: usize = 64 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "pscat-mcp-server",
    version,
    about = "MCP server for the PSCat PostScript renderer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Parsed once at startup; boxing the settings structs would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Serve MCP over Streamable HTTP (remote use).
    ServeHttp {
        #[command(flatten)]
        http: HttpConfig,
        /// Boxed: the OAuth settings make it large.
        #[command(flatten)]
        auth: Box<AuthConfig>,
        #[command(flatten)]
        limits: LimitsConfig,
        #[command(flatten)]
        worker: WorkerConfig,
        #[command(flatten)]
        jobs: JobsConfig,
        #[command(flatten)]
        artifacts: ArtifactConfig,
        #[command(flatten)]
        capture: CaptureConfig,
        /// Log format.
        #[arg(long, env = "PSCAT_MCP_LOG_FORMAT", value_enum, default_value_t = LogFormat::Json)]
        log_format: LogFormat,
    },
    /// Serve MCP over stdin/stdout (local agents such as Claude Code).
    ServeStdio {
        #[command(flatten)]
        limits: LimitsConfig,
        #[command(flatten)]
        worker: WorkerConfig,
        #[command(flatten)]
        jobs: JobsConfig,
        #[command(flatten)]
        artifacts: ArtifactConfig,
        /// Log format (logs go to stderr).
        #[arg(long, env = "PSCAT_MCP_LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
        log_format: LogFormat,
    },
    /// Internal: run one render job read from stdin. Started by the server.
    #[command(hide = true)]
    RenderWorker {
        /// Ignored; lets tests find a specific worker process.
        #[arg(long)]
        tag: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        // No async runtime in the worker: one thread, one program.
        Command::RenderWorker { .. } => {
            let code = pscat_mcp::worker::run_stdio(MAX_WORKER_JOB_BYTES);
            std::process::ExitCode::from(code as u8)
        }
        Command::ServeHttp {
            http,
            auth,
            limits,
            worker,
            jobs,
            artifacts,
            capture,
            log_format,
        } => run(log_format, async move {
            // In-memory jobs need one long-lived instance: opt-in over HTTP.
            let jobs = JobsConfig {
                enabled: http.async_jobs,
                ..jobs
            };
            let service = pscat_mcp::build_service_with(limits, jobs, artifacts, &worker)?;
            let capture = match &capture.capture_file {
                Some(path) => {
                    pscat_mcp::http::check_capture_bind(&http)?;
                    let file = pscat_mcp::capture::Capture::open(path)
                        .with_context(|| format!("opening the capture file {}", path.display()))?;
                    tracing::warn!(
                        path = %path.display(),
                        "CAPTURE MODE: request heads (no tokens) and bodies are written to this file; \
                         for local debugging only"
                    );
                    Some(std::sync::Arc::new(file))
                }
                None => None,
            };
            pscat_mcp::http::serve_with_capture(http, &auth, service, capture).await
        }),
        Command::ServeStdio {
            limits,
            worker,
            jobs,
            artifacts,
            log_format,
        } => run(log_format, async move {
            // Short links are served by `serve-http`'s `/a/` route only.
            if artifacts
                .artifact_public_origin
                .as_deref()
                .is_some_and(|o| o.trim() != pscat_mcp::config::DIRECT_LINKS)
            {
                anyhow::bail!(
                    "refusing to start: PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN makes short links, which \
                     serve-http answers at /a/...; serve-stdio serves no HTTP. Unset it"
                );
            }
            // One long-lived process and one local user: jobs are always on.
            let service =
                pscat_mcp::build_service_with(limits, jobs.enabled(), artifacts, &worker)?;
            pscat_mcp::mcp::serve_stdio(service).await
        }),
    }
}

fn run(
    log_format: LogFormat,
    fut: impl std::future::Future<Output = anyhow::Result<()>>,
) -> std::process::ExitCode {
    telemetry::init(log_format);
    // Before any worker exists: a worker runs as the same user, and a
    // dumpable server's memory would be open to it through /proc.
    if let Err(e) = pscat_mcp::sandbox::harden_server() {
        tracing::error!(error = %e, "cannot make the server non-dumpable");
        eprintln!("pscat-mcp-server: {e}");
        return std::process::ExitCode::FAILURE;
    }
    // Once, at startup: the positive record that the flag is in force (the
    // line above is the only other outcome). `get_renderer_info` reports it.
    if pscat_mcp::sandbox::server_dumpable() {
        tracing::info!(
            server_dumpable = true,
            "sandbox: the server process is dumpable (no non-dumpable flag on this system)"
        );
    } else {
        tracing::info!(
            server_dumpable = false,
            "sandbox: the server process is non-dumpable"
        );
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("pscat-mcp-server: cannot start the runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(fut) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "fatal");
            eprintln!("pscat-mcp-server: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
