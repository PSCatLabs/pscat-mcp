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
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            max_source_bytes: 256 * 1024,
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
            None => {
                let margin = (self.render_timeout_ms / 10).max(250);
                Some(Duration::from_millis(
                    self.render_timeout_ms.saturating_sub(margin).max(1),
                ))
            }
        }
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
        self.max_concurrent_renders.filter(|n| *n > 0).unwrap_or_else(|| {
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
        Ok(())
    }
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
    #[arg(long, env = "PSCAT_MCP_AUTH_TOKENS", hide_env_values = true, value_delimiter = ',')]
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
