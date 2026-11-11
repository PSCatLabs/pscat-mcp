//! PSCat MCP server: exposes the PSCat PostScript interpreter to AI agents
//! over the Model Context Protocol.
//!
//! Layers, from the outside in:
//!
//! * [`http`] / [`mcp::serve_stdio`] — transports.
//! * [`mcp`] — tool definitions; no rendering logic.
//! * [`render`] — transport-independent service: validation, limits,
//!   concurrency, structured errors.
//! * [`backend`] — runs a job; [`backend::SubprocessBackend`] starts one
//!   `render-worker` process per job and kills it at the deadline.
//! * [`worker`] — the only code that executes PostScript (inside the child).
//! * [`pscat_extras`] — the one module that calls `#[doc(hidden)]` PSCat APIs.

#![warn(missing_docs)]

pub mod auth;
pub mod backend;
pub mod build_info;
pub mod config;
pub mod http;
pub mod mcp;
pub mod pscat_extras;
pub mod render;
pub mod telemetry;
pub mod worker;

use std::sync::Arc;

/// Build the production service: subprocess backend over `limits`.
pub fn build_service(
    limits: config::LimitsConfig,
    worker: &config::WorkerConfig,
) -> anyhow::Result<Arc<render::RenderService>> {
    limits.check().map_err(anyhow::Error::msg)?;
    let exe = worker.exe()?;
    let backend = Arc::new(backend::SubprocessBackend::new(exe, limits.clone()));
    Ok(Arc::new(render::RenderService::new(backend, limits)))
}
