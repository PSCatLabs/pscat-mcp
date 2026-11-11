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
//! * [`generate`] — tools whose input is not PostScript (LaTeX for
//!   `chalkboard_math`, text for `handwrite`; the latter in [`handwrite`]):
//!   checked in the server, turned into a program in the worker.
//! * [`jobs`] — the async job store behind `start_render` and the Tasks
//!   extension, owned by the render service.
//! * [`pscat_extras`] — the one module that calls `#[doc(hidden)]` PSCat APIs.

#![warn(missing_docs)]

pub mod auth;
pub mod backend;
pub mod build_info;
pub mod capture;
pub mod catalog;
pub mod config;
pub mod generate;
pub mod handwrite;
pub mod http;
pub mod jobs;
pub mod mcp;
pub mod pscat_extras;
pub mod render;
pub mod sheet;
pub mod telemetry;
pub mod worker;

use std::sync::Arc;

/// Build the production service: subprocess backend over `limits`, with
/// async jobs off.
pub fn build_service(
    limits: config::LimitsConfig,
    worker: &config::WorkerConfig,
) -> anyhow::Result<Arc<render::RenderService>> {
    build_service_with_jobs(limits, config::JobsConfig::default(), worker)
}

/// [`build_service`] with async jobs as `jobs` says (on when
/// `jobs.enabled`).
pub fn build_service_with_jobs(
    limits: config::LimitsConfig,
    jobs: config::JobsConfig,
    worker: &config::WorkerConfig,
) -> anyhow::Result<Arc<render::RenderService>> {
    limits.check().map_err(anyhow::Error::msg)?;
    jobs.check(&limits).map_err(anyhow::Error::msg)?;
    let exe = worker.exe()?;
    let backend = Arc::new(backend::SubprocessBackend::new(exe, limits.clone()));
    Ok(Arc::new(
        render::RenderService::new(backend, limits).with_jobs(jobs),
    ))
}
