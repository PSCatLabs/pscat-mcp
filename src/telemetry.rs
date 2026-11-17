//! Logging setup. Logs always go to stderr: in stdio mode stdout carries
//! the MCP protocol.

use tracing_subscriber::{
    EnvFilter, Registry,
    filter::{FilterExt as _, filter_fn},
    fmt,
    prelude::*,
};

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum LogFormat {
    /// One JSON object per line (Cloud Logging reads `severity`-free JSON fine).
    #[default]
    Json,
    /// Human-readable lines.
    Pretty,
}

/// The filter used when `RUST_LOG` is unset: INFO. (rmcp is held to WARN by
/// [`rmcp_clamp`] whatever the filter says.)
pub const DEFAULT_FILTER: &str = "info";

/// Whether an event or span from `meta` may be logged: everything from the
/// `rmcp` crate (the `rmcp` target and every `rmcp::...` module) is held to
/// WARN and ERROR, whatever `RUST_LOG` says.
///
/// rmcp logs whole JSON-RPC messages below WARN, and a message carries
/// what the logging contract keeps out of the logs: artifact links
/// (`pscat://render/<id>/<n>` today, a signed URL later: a bearer
/// capability for its lifetime), tool arguments, notification text, the
/// client's `initialize` parameters. As of rmcp 3.9 the lines that do it:
///
/// * `rmcp::service`: "received request" (DEBUG, the request), "response
///   message" (DEBUG, the result), "received notification" (INFO),
///   "new event" (TRACE, every event), "Service initialized as server"
///   (INFO, the `initialize` parameters), "cancelled" (INFO, a client's
///   reason text) and "received id-less peer error" (DEBUG);
/// * `rmcp::transport::async_rw`: "Failed to parse message" (DEBUG, the raw
///   line) and a TRACE line per message;
/// * `rmcp::transport::streamable_http_server::tower`: a TRACE line per
///   message (`?message`);
/// * `rmcp::transport::auth` (client side OAuth, unused by the server):
///   DEBUG lines with token results.
///
/// A clamp on those targets would miss the next one a new rmcp version adds,
/// so the clamp is the whole crate. Its WARN and ERROR lines (a rejected
/// Host or Origin, a failed send) still get through; it has no other lines
/// the server needs, and our own `pscat_mcp` targets are not touched.
pub fn rmcp_clamp(meta: &tracing::Metadata<'_>) -> bool {
    let target = meta.target();
    let is_rmcp = target == "rmcp"
        || target
            .strip_prefix("rmcp")
            .is_some_and(|rest| rest.starts_with("::"));
    !is_rmcp || *meta.level() <= tracing::Level::WARN
}

/// The fmt layer in `format` writing to `writer`, behind `env` and
/// [`rmcp_clamp`]. Both are filters of this layer, so no `RUST_LOG` can lift
/// the clamp.
fn layer<W>(
    format: LogFormat,
    writer: W,
    env: EnvFilter,
) -> Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync + 'static>
where
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = env.and(filter_fn(rmcp_clamp));
    match format {
        LogFormat::Json => fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(writer)
            .with_filter(filter)
            .boxed(),
        LogFormat::Pretty => fmt::layer().with_writer(writer).with_filter(filter).boxed(),
    }
}

/// Install the global subscriber. `RUST_LOG`, when set, replaces
/// [`DEFAULT_FILTER`] completely, except that rmcp stays at WARN
/// ([`rmcp_clamp`]).
pub fn init(format: LogFormat) {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let _ = tracing_subscriber::registry()
        .with(layer(format, std::io::stderr, env))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::Level;

    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Emit one event per (target, level) through the real layer with
    /// `rust_log` and return the log text.
    fn logged(format: LogFormat, rust_log: &str) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buf);
        let subscriber = tracing_subscriber::registry().with(layer(
            format,
            move || Buf(Arc::clone(&writer)),
            EnvFilter::new(rust_log),
        ));
        tracing::subscriber::with_default(subscriber, || {
            macro_rules! emit {
                ($target:literal) => {
                    tracing::event!(target: $target, Level::TRACE, "t-{}", $target);
                    tracing::event!(target: $target, Level::DEBUG, "d-{}", $target);
                    tracing::event!(target: $target, Level::INFO, "i-{}", $target);
                    tracing::event!(target: $target, Level::WARN, "w-{}", $target);
                    tracing::event!(target: $target, Level::ERROR, "e-{}", $target);
                };
            }
            emit!("rmcp");
            emit!("rmcp::service");
            emit!("rmcp::transport::streamable_http_server::tower");
            emit!("pscat_mcp::mcp");
            emit!("rmcp_other");
        });
        let out = buf.lock().unwrap().clone();
        String::from_utf8(out).unwrap()
    }

    /// Nothing below WARN from `targets` is in `out`; their WARN and ERROR
    /// lines are.
    fn assert_clamped(out: &str, targets: &[&str]) {
        // Whole messages: "t-rmcp" is a prefix of "t-rmcp::service".
        let has = |m: String| out.contains(&format!("\"{m}\""));
        for target in targets {
            for lower in ["t", "d", "i"] {
                assert!(!has(format!("{lower}-{target}")), "{lower}-{target}: {out}");
            }
            for upper in ["w", "e"] {
                assert!(has(format!("{upper}-{target}")), "{upper}-{target}: {out}");
            }
        }
    }

    #[test]
    fn rmcp_is_held_to_warn_whatever_rust_log_says() {
        const TOWER: &str = "rmcp::transport::streamable_http_server::tower";
        let cases: [(&str, &[&str]); 5] = [
            ("trace", &["rmcp", "rmcp::service", TOWER]),
            (
                "rmcp=trace,pscat_mcp=trace",
                &["rmcp", "rmcp::service", TOWER],
            ),
            ("rmcp::service=trace", &["rmcp::service"]),
            (
                "info,rmcp::transport::streamable_http_server::tower=trace",
                &[TOWER],
            ),
            ("debug,rmcp=debug", &["rmcp", "rmcp::service", TOWER]),
        ];
        for (rust_log, targets) in cases {
            let out = logged(LogFormat::Json, rust_log);
            assert_clamped(&out, targets);
            // Our own targets are not touched, and a lookalike crate name is
            // not rmcp.
            if rust_log.contains("pscat_mcp=trace") || rust_log == "trace" {
                for level in ["t", "d", "i", "w", "e"] {
                    assert!(
                        out.contains(&format!("\"{level}-pscat_mcp::mcp\"")),
                        "{level} of pscat_mcp lost under {rust_log}: {out}"
                    );
                }
            }
            if rust_log == "trace" {
                assert!(out.contains("\"t-rmcp_other\""), "{out}");
            }
        }
    }

    #[test]
    fn the_pretty_format_is_clamped_too() {
        let out = logged(LogFormat::Pretty, "trace");
        assert!(!out.contains("d-rmcp::service"), "{out}");
        assert!(out.contains("w-rmcp::service"), "{out}");
        assert!(out.contains("d-pscat_mcp::mcp"), "{out}");
    }

    #[test]
    fn the_default_filter_is_info_for_our_own_targets() {
        let out = logged(LogFormat::Json, DEFAULT_FILTER);
        assert!(out.contains("\"i-pscat_mcp::mcp\""), "{out}");
        assert!(!out.contains("\"d-pscat_mcp::mcp\""), "{out}");
        assert!(!out.contains("\"i-rmcp::service\""), "{out}");
        assert!(out.contains("\"w-rmcp::service\""), "{out}");
    }
}
