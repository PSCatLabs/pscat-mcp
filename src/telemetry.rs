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

/// The filter used when `RUST_LOG` is unset: INFO, except that rmcp's
/// `rmcp::service` target is held to WARN. At INFO it logs "Service
/// initialized as server" with a Debug dump of the client's `initialize`
/// parameters (`clientInfo`, capabilities), and the text of a client's
/// notifications and cancellations: client-chosen strings that the logging
/// contract does not cover. Its other INFO lines (a stream ended, a task
/// cancelled, serve finished) carry nothing the request and render lines
/// do not.
pub const DEFAULT_FILTER: &str = "info,rmcp::service=warn";

/// Install the global subscriber. `RUST_LOG`, when set, replaces
/// [`DEFAULT_FILTER`] completely.
pub fn init(format: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
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
