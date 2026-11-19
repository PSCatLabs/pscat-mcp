//! PSCat MCP server: exposes the PSCat PostScript interpreter to AI agents
//! over the Model Context Protocol.
//!
//! Layers, from the outside in:
//!
//! * [`http`] / [`mcp::serve_stdio`] — transports.
//! * [`mcp`] — tool definitions; no rendering logic.
//! * [`guide`] — the PSCat plugin's skills as MCP resources and prompts.
//! * [`render`] — transport-independent service: validation, limits,
//!   concurrency, structured errors.
//! * [`backend`] — runs a job; [`backend::SubprocessBackend`] starts one
//!   `render-worker` process per job and kills it at the deadline.
//! * [`worker`] — the only code that executes PostScript (inside the child).
//! * [`sandbox`] — the worker's system-call filter (Linux).
//! * [`generate`] — tools whose input is not PostScript (LaTeX for
//!   `chalkboard_math`, text for `handwrite`; the latter in [`handwrite`]):
//!   checked in the server, turned into a program in the worker.
//! * [`jobs`] — the async job store behind `start_render` and the Tasks
//!   extension, owned by the render service.
//! * [`artifacts`] — artifact delivery: the `delivery` argument, the
//!   [`artifacts::ArtifactStore`] trait and its memory backend.
//! * [`gcs`] — the Cloud Storage artifact store: uploads and keyless V4
//!   signed URLs (server process only).
//! * [`short_link`] — short artifact links on this server (`/a/...`),
//!   redirecting to a signed URL minted when opened.
//! * [`widget`] — the opt-in MCP Apps image widget (`ui://` resource) for
//!   ChatGPT.
//! * [`pscat_extras`] — the one module that calls `#[doc(hidden)]` PSCat APIs.

#![warn(missing_docs)]

pub mod artifacts;
pub mod auth;
pub mod auth_none;
pub mod backend;
pub mod build_info;
pub mod capture;
pub mod catalog;
pub mod config;
pub mod gcs;
pub mod generate;
pub mod guide;
pub mod handwrite;
pub mod http;
pub mod jobs;
pub mod mcp;
pub mod oauth;
pub mod pscat_extras;
pub mod render;
pub mod sandbox;
pub mod sheet;
pub mod short_link;
pub mod telemetry;
pub mod widget;
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
    build_service_with(limits, jobs, config::ArtifactConfig::default(), worker)
}

/// [`build_service_with_jobs`] with artifact delivery as `artifacts` says.
pub fn build_service_with(
    limits: config::LimitsConfig,
    jobs: config::JobsConfig,
    artifacts: config::ArtifactConfig,
    worker: &config::WorkerConfig,
) -> anyhow::Result<Arc<render::RenderService>> {
    limits.check().map_err(anyhow::Error::msg)?;
    if let Some(warning) = limits.deprecation_warning() {
        tracing::warn!("{warning}");
    }
    jobs.check(&limits).map_err(anyhow::Error::msg)?;
    artifacts
        .check(&limits, &jobs)
        .map_err(anyhow::Error::msg)?;
    let exe = worker.exe()?;
    let font_pack_dir = worker.font_pack_dir().map_err(anyhow::Error::msg)?;
    let mut backend = backend::SubprocessBackend::new(exe, limits.clone());
    if let Some(dir) = &font_pack_dir {
        if !dir.join("fonts").is_dir() {
            tracing::warn!(
                "the font-pack directory has no fonts/ directory; PSCat reads packs from \
                 <dir>/fonts/packs/<pack>/, so it will find none"
            );
        }
        backend = backend.with_font_pack_dir(dir);
    }
    // The Cloud Storage store is built here, where its TLS client can fail
    // the start; the memory store is built by the service over its ledger.
    if artifacts.artifact_links != artifacts::LinksBackend::Gcs
        && artifacts
            .artifact_public_origin
            .as_deref()
            .is_some_and(|o| o.trim() != config::DIRECT_LINKS)
    {
        tracing::warn!(
            "PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN is set but PSCAT_MCP_ARTIFACT_LINKS is not gcs; it is \
             ignored"
        );
    }
    if let Some(warning) = widget::startup_warning(&artifacts) {
        tracing::warn!("{warning}");
    }
    let gcs = match artifacts.artifact_links {
        artifacts::LinksBackend::Gcs => {
            Some(Arc::new(gcs::GcsArtifactStore::from_config(&artifacts)?))
        }
        _ => None,
    };
    let mut service = render::RenderService::new(Arc::new(backend), limits)
        .with_font_packs_configured(font_pack_dir.is_some())
        .with_jobs(jobs);
    service = match gcs {
        Some(store) => service.with_artifact_store(artifacts, store),
        None => service.with_artifacts(artifacts),
    };
    Ok(Arc::new(service))
}
