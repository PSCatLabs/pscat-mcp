//! Logging setup. Logs always go to stderr: in stdio mode stdout carries
//! the MCP protocol.

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum LogFormat {
    /// One JSON object per line (Cloud Logging reads `severity`-free JSON fine).
    #[default]
    Json,
    /// Human-readable lines.
    Pretty,
}

/// Install the global subscriber. `RUST_LOG` overrides the default `info`.
pub fn init(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    let result = match format {
        LogFormat::Json => registry
            .with(
                fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_writer(std::io::stderr),
            )
            .try_init(),
        LogFormat::Pretty => registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .try_init(),
    };
    let _ = result;
}
