//! Backends that execute a [`WorkerJob`].
//!
//! The production backend is [`SubprocessBackend`]: one short-lived
//! `render-worker` child process per job, killed at the deadline.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use crate::config::LimitsConfig;
use crate::worker::{WorkerJob, WorkerResult};

/// Why a backend produced no [`WorkerResult`].
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The job passed its wall-clock deadline; the worker was killed.
    #[error("the render passed its deadline and was killed")]
    Timeout,
    /// The worker could not be started.
    #[error("could not start the render worker: {0}")]
    Spawn(String),
    /// The worker exited abnormally (signal, OS limit, panic).
    #[error("the render worker failed: {0}")]
    Crashed(String),
    /// The worker wrote more than the output cap.
    #[error("the render worker's output exceeded {0} bytes")]
    OutputOverflow(usize),
    /// The worker's output could not be parsed.
    #[error("the render worker returned unreadable output: {0}")]
    Protocol(String),
}

/// Boxed future returned by [`RenderBackend::execute`].
pub type BackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkerResult, BackendError>> + Send + 'a>>;

/// Something that can run a job. Dropping the returned future must stop
/// the job (the subprocess backend kills its child).
pub trait RenderBackend: Send + Sync + 'static {
    /// Run `job`, giving up after `deadline`.
    fn execute(&self, job: WorkerJob, deadline: Duration) -> BackendFuture<'_>;
}

/// Runs each job in a fresh `render-worker` child process.
#[derive(Debug, Clone)]
pub struct SubprocessBackend {
    /// Executable started with the `render-worker` subcommand.
    pub exe: PathBuf,
    /// Limits used for the child's OS rlimits and output caps.
    pub limits: LimitsConfig,
}

impl SubprocessBackend {
    /// A backend starting `exe render-worker` for each job.
    pub fn new(exe: PathBuf, limits: LimitsConfig) -> Self {
        SubprocessBackend { exe, limits }
    }
}

impl RenderBackend for SubprocessBackend {
    fn execute(&self, job: WorkerJob, deadline: Duration) -> BackendFuture<'_> {
        Box::pin(async move {
            let _ = (job, deadline);
            todo!("subprocess backend")
        })
    }
}
