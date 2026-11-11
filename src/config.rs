//! Configuration: resource limits, the worker, and the HTTP server.
//!
//! Every setting has a command-line flag and an environment variable
//! (`PSCAT_MCP_*`, plus Cloud Run's `PORT`), so the container is configured
//! entirely through its environment.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;

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
            render_timeout_ms: 20_000,
            interp_time_limit_ms: None,
            max_steps: 5_000_000_000,
            max_memory_bytes: 512 * 1024 * 1024,
            worker_address_space_bytes: None,
            max_pixels: 16_000_000,
            max_pages: 10,
            max_output_bytes: 8 * 1024 * 1024,
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
}

impl WorkerConfig {
    /// The executable to start.
    pub fn exe(&self) -> std::io::Result<PathBuf> {
        match &self.worker_exe {
            Some(p) => Ok(p.clone()),
            None => std::env::current_exe(),
        }
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
    fn concurrency_defaults_are_bounded() {
        let l = LimitsConfig::default();
        assert!((1..=4).contains(&l.concurrency()));
        let one = LimitsConfig {
            max_concurrent_renders: Some(1),
            ..LimitsConfig::default()
        };
        assert_eq!(one.concurrency(), 1);
    }
}
