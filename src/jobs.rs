//! The async job store: renders that run past one tool call.
//!
//! Two client-facing shapes share this one store: the explicit tools
//! (`start_render`, `render_status`, `render_result`, `cancel_render`, as in
//! the PSCat plugin) and the MCP Tasks extension (`tools/call` answered with
//! a task handle, then `tasks/get` and `tasks/cancel`). Both live in
//! `src/mcp.rs`; [`RenderService`](crate::render::RenderService) owns the
//! store and runs the jobs, so every MCP handler (HTTP builds one per
//! request) sees the same jobs.
//!
//! * **Ownership.** A job belongs to the principal that started it (the
//!   bearer token's identity over HTTP, the local user over stdio). Any
//!   other principal gets the same `unknown_job` as for an id that never
//!   existed. Job ids are random UUIDs (v4) and carry nothing else.
//! * **Limits.** Caps on unfinished jobs per principal and in all
//!   (`server_busy`), a budget of result bytes held in memory
//!   (`results_budget_exceeded`): a job reserves its largest possible
//!   result when it is accepted and keeps only what it produced once it
//!   finishes. A finished job is kept for the result TTL after it finished,
//!   and fetching does not remove it; when the budget is short, results
//!   already fetched are dropped first, oldest first.
//! * **Running.** A queued job waits for a running-job permit (at most
//!   `max_running_jobs`) and then for a render slot shared with every
//!   synchronous call, with no queue timeout. Cancelling drops the render,
//!   which kills the worker (`backend::Worker`'s drop).
//!
//! Nothing here knows about MCP: results are kept as
//! [`RenderOutput`]/[`RenderError`] and formatted by the tool layer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::config::{JOB_MIN_CHARGE_BYTES, JobsConfig, LimitsConfig};
use crate::render::{ErrorCode, RenderError, RenderOutput};

/// The principal of every stdio call: there is one local user.
pub const LOCAL_PRINCIPAL: &str = "local";

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted, waiting for a running-job permit or a render slot.
    Queued,
    /// Its worker is running.
    Running,
    /// Finished with a result.
    Done,
    /// Finished with an error (a PostScript error, a limit, the deadline).
    Failed,
    /// Cancelled before it finished; there is no result.
    Cancelled,
}

impl JobState {
    /// Whether the job has finished (done, failed or cancelled).
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled
        )
    }
}

/// What `render_status` reports about a job. Small by design.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct JobStatus {
    /// The job's id.
    pub job_id: String,
    /// The tool the job runs (`render_postscript`, ...).
    pub tool: String,
    /// Where the job is.
    pub state: JobState,
    /// Seconds since the job was accepted (until it finished, once it has).
    pub elapsed_seconds: f64,
    /// Seconds the job's worker may run before it is killed.
    pub timeout_seconds: f64,
    /// True when a cancel was asked for and the job has not yet stopped.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cancel_requested: bool,
    /// For a finished job: seconds until it and its result are dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<f64>,
    /// For a failed job: why (its code and message; the full report is in
    /// `render_result`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_error: Option<RenderError>,
    /// When the job was accepted (for the Tasks extension's timestamps).
    #[serde(skip)]
    pub created_at: SystemTime,
    /// When the job last changed state.
    #[serde(skip)]
    pub updated_at: SystemTime,
}

/// A finished job's outcome, as `render_result` returns it.
#[derive(Debug, Clone)]
pub struct JobResult {
    /// The tool the job ran.
    pub tool: String,
    /// The render, or why it failed.
    pub outcome: Result<Arc<RenderOutput>, Arc<RenderError>>,
}

enum State {
    Queued,
    Running,
    Done(Arc<RenderOutput>),
    Failed(Arc<RenderError>),
    Cancelled,
}

impl State {
    fn name(&self) -> JobState {
        match self {
            State::Queued => JobState::Queued,
            State::Running => JobState::Running,
            State::Done(_) => JobState::Done,
            State::Failed(_) => JobState::Failed,
            State::Cancelled => JobState::Cancelled,
        }
    }
}

struct Job {
    owner: String,
    tool: String,
    created: Instant,
    created_at: SystemTime,
    updated_at: SystemTime,
    finished: Option<Instant>,
    state: State,
    cancel: CancellationToken,
    /// Bytes charged against the results budget: the largest result while
    /// unfinished, what it holds once finished.
    charge: usize,
    /// Whether its result was fetched at least once.
    fetched: bool,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, Job>,
    /// Sum of every job's `charge`.
    charged: usize,
}

/// The async job store. See the module documentation.
pub struct JobStore {
    config: JobsConfig,
    timeout: Duration,
    max_result_bytes: usize,
    running: Arc<Semaphore>,
    inner: Mutex<Inner>,
    closed: AtomicBool,
}

impl std::fmt::Debug for JobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobStore")
            .field("enabled", &self.config.enabled)
            .finish()
    }
}

impl JobStore {
    /// A store with `config`, for renders under `limits`.
    pub fn new(config: JobsConfig, limits: &LimitsConfig) -> Self {
        JobStore {
            timeout: config.job_timeout(),
            max_result_bytes: JobsConfig::max_result_bytes(limits),
            running: Arc::new(Semaphore::new(config.running_jobs(limits))),
            config,
            inner: Mutex::new(Inner::default()),
            closed: AtomicBool::new(false),
        }
    }

    /// The settings in force.
    pub fn config(&self) -> &JobsConfig {
        &self.config
    }

    /// Whether async jobs are served.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Whether the Tasks extension is served.
    pub fn tasks_enabled(&self) -> bool {
        self.config.enabled && self.config.tasks
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accept a job for `owner` running `tool`: check the caps and reserve
    /// its largest result. Returns its id and cancellation token.
    pub(crate) fn insert(
        &self,
        owner: &str,
        tool: &str,
    ) -> Result<(String, CancellationToken), RenderError> {
        if !self.enabled() {
            return Err(RenderError::new(
                ErrorCode::InvalidRequest,
                "async jobs are not enabled on this server",
            ));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(RenderError::new(
                ErrorCode::ServerBusy,
                "the server is shutting down",
            ));
        }
        let mut inner = self.lock();
        self.sweep(&mut inner);
        let unfinished = |o: Option<&str>| {
            inner
                .jobs
                .values()
                .filter(|j| !j.state.name().is_finished() && o.is_none_or(|o| j.owner == o))
                .count()
        };
        let mine = unfinished(Some(owner));
        if mine >= self.config.max_jobs_per_principal {
            return Err(RenderError::new(
                ErrorCode::ServerBusy,
                format!(
                    "you have {mine} async jobs queued or running, this server's limit per \
                     client; wait for one to finish or cancel one"
                ),
            ));
        }
        let all = unfinished(None);
        if all >= self.config.max_jobs {
            return Err(RenderError::new(
                ErrorCode::ServerBusy,
                format!(
                    "{all} async jobs are queued or running, this server's limit; try again \
                     later"
                ),
            ));
        }
        let need = self.max_result_bytes;
        let budget = self.config.job_results_budget_bytes;
        // Results already fetched were delivered: they make room first,
        // oldest first.
        while inner.charged + need > budget {
            let oldest = inner
                .jobs
                .iter()
                .filter(|(_, j)| j.fetched && j.finished.is_some())
                .min_by_key(|(_, j)| j.finished)
                .map(|(id, _)| id.clone());
            let Some(id) = oldest else { break };
            if let Some(job) = inner.jobs.remove(&id) {
                inner.charged -= job.charge;
            }
        }
        if inner.charged + need > budget {
            return Err(RenderError::new(
                ErrorCode::ResultsBudgetExceeded,
                format!(
                    "job results waiting to be fetched hold {} of this server's {budget} bytes, \
                     and a job reserves {need}; fetch or cancel a job, or wait for results to \
                     expire",
                    inner.charged
                ),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let now = SystemTime::now();
        inner.charged += need;
        inner.jobs.insert(
            id.clone(),
            Job {
                owner: owner.to_string(),
                tool: tool.to_string(),
                created: Instant::now(),
                created_at: now,
                updated_at: now,
                finished: None,
                state: State::Queued,
                cancel: cancel.clone(),
                charge: need,
                fetched: false,
            },
        );
        Ok((id, cancel))
    }

    /// Wait for a running-job permit.
    pub(crate) async fn running_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.running.clone().acquire_owned().await.ok()
    }

    /// Record that `id`'s worker is starting.
    pub(crate) fn mark_running(&self, id: &str) {
        if let Some(job) = self.lock().jobs.get_mut(id)
            && matches!(job.state, State::Queued)
        {
            job.state = State::Running;
            job.updated_at = SystemTime::now();
        }
    }

    /// Record `id`'s end: `None` for cancelled.
    pub(crate) fn finish(&self, id: &str, outcome: Option<Result<RenderOutput, RenderError>>) {
        let mut inner = self.lock();
        let Some(job) = inner.jobs.get_mut(id) else {
            return;
        };
        let (state, charge) = match outcome {
            None => (State::Cancelled, JOB_MIN_CHARGE_BYTES),
            Some(Ok(out)) => {
                let bytes: usize = out.artifacts.iter().map(|a| a.data_base64.len()).sum();
                let charge = bytes + out.program_output.len() + JOB_MIN_CHARGE_BYTES;
                (State::Done(Arc::new(out)), charge)
            }
            Some(Err(err)) => {
                let charge = err.program_output.as_ref().map_or(0, String::len)
                    + err.postscript.as_ref().map_or(0, |p| p.report.len())
                    + err.message.len()
                    + JOB_MIN_CHARGE_BYTES;
                (State::Failed(Arc::new(err)), charge)
            }
        };
        let old = std::mem::replace(&mut job.charge, charge);
        job.state = state;
        job.finished = Some(Instant::now());
        job.updated_at = SystemTime::now();
        inner.charged = inner.charged - old + charge;
    }

    /// Drop the finished jobs older than the TTL.
    fn sweep(&self, inner: &mut Inner) {
        let ttl = self.config.result_ttl();
        let mut freed = 0;
        inner.jobs.retain(|_, j| {
            let keep = j.finished.is_none_or(|f| f.elapsed() < ttl);
            if !keep {
                freed += j.charge;
            }
            keep
        });
        inner.charged -= freed;
    }

    fn get<'a>(
        &self,
        inner: &'a mut Inner,
        owner: &str,
        id: &str,
    ) -> Result<&'a mut Job, RenderError> {
        self.sweep(inner);
        match inner.jobs.get_mut(id) {
            Some(job) if job.owner == owner => Ok(job),
            _ => Err(self.unknown(id)),
        }
    }

    fn unknown(&self, id: &str) -> RenderError {
        let shown: String = id.chars().take(64).collect();
        RenderError::new(
            ErrorCode::UnknownJob,
            format!(
                "no job {shown:?}: never started by this client, or finished more than {} s ago \
                 and dropped",
                self.config.result_ttl().as_secs()
            ),
        )
    }

    fn facts(&self, id: &str, job: &Job) -> JobStatus {
        let end = job.finished.unwrap_or_else(Instant::now);
        let elapsed = end.saturating_duration_since(job.created).as_secs_f64();
        let expires = job.finished.map(|f| {
            let left = self.config.result_ttl().saturating_sub(f.elapsed());
            (left.as_secs_f64() * 10.0).round() / 10.0
        });
        JobStatus {
            job_id: id.to_string(),
            tool: job.tool.clone(),
            state: job.state.name(),
            elapsed_seconds: (elapsed * 10.0).round() / 10.0,
            timeout_seconds: self.timeout.as_secs_f64(),
            cancel_requested: job.finished.is_none() && job.cancel.is_cancelled(),
            expires_in_seconds: expires,
            job_error: match &job.state {
                State::Failed(e) => Some(RenderError {
                    // The status stays small: the report and stdout are in
                    // `render_result`.
                    postscript: None,
                    program_output: None,
                    ..(**e).clone()
                }),
                _ => None,
            },
            created_at: job.created_at,
            updated_at: job.updated_at,
        }
    }

    /// `owner`'s job `id`.
    pub fn status(&self, owner: &str, id: &str) -> Result<JobStatus, RenderError> {
        let mut inner = self.lock();
        let job = self.get(&mut inner, owner, id)?;
        Ok(self.facts(id, job))
    }

    /// `owner`'s job `id`, and its result when it has one.
    pub fn status_and_result(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<(JobStatus, Option<JobResult>), RenderError> {
        let mut inner = self.lock();
        let job = self.get(&mut inner, owner, id)?;
        let outcome = match &job.state {
            State::Done(out) => Some(Ok(out.clone())),
            State::Failed(err) => Some(Err(err.clone())),
            _ => None,
        };
        if outcome.is_some() {
            job.fetched = true;
        }
        let facts = self.facts(id, job);
        let result = outcome.map(|outcome| JobResult {
            tool: job.tool.clone(),
            outcome,
        });
        Ok((facts, result))
    }

    /// The result of `owner`'s finished job `id`. A job still queued or
    /// running is `job_not_finished`; a cancelled one, `cancelled`.
    pub fn result(&self, owner: &str, id: &str) -> Result<JobResult, RenderError> {
        let (facts, result) = self.status_and_result(owner, id)?;
        result.ok_or_else(|| match facts.state {
            JobState::Cancelled => RenderError::new(
                ErrorCode::Cancelled,
                format!("job {id} was cancelled; it has no result"),
            ),
            state => RenderError::new(
                ErrorCode::JobNotFinished,
                format!(
                    "job {id} is {}; poll render_status until it is done or failed",
                    serde_json::to_value(state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                ),
            ),
        })
    }

    /// Ask `owner`'s job `id` to stop. A finished job is left as it is.
    pub(crate) fn request_cancel(&self, owner: &str, id: &str) -> Result<(), RenderError> {
        let mut inner = self.lock();
        let job = self.get(&mut inner, owner, id)?;
        if job.finished.is_none() {
            job.cancel.cancel();
            job.updated_at = SystemTime::now();
        }
        Ok(())
    }

    /// Refuse new jobs and cancel every unfinished one.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for job in self.lock().jobs.values() {
            if job.finished.is_none() {
                job.cancel.cancel();
            }
        }
    }

    /// Jobs queued or running.
    pub fn unfinished(&self) -> usize {
        self.lock()
            .jobs
            .values()
            .filter(|j| j.finished.is_none())
            .count()
    }

    /// Bytes charged against the results budget.
    pub fn charged_bytes(&self) -> usize {
        self.lock().charged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(config: JobsConfig) -> JobStore {
        JobStore::new(config.enabled(), &LimitsConfig::default())
    }

    #[test]
    fn caps_per_principal_and_in_all() {
        let s = store(JobsConfig {
            max_jobs_per_principal: 2,
            max_jobs: 3,
            ..JobsConfig::default()
        });
        s.insert("a", "render_postscript").unwrap();
        s.insert("a", "render_postscript").unwrap();
        let e = s.insert("a", "render_postscript").unwrap_err();
        assert_eq!(e.code, ErrorCode::ServerBusy);
        assert!(e.message.contains("per client"), "{e}");
        s.insert("b", "render_postscript").unwrap();
        let e = s.insert("c", "render_postscript").unwrap_err();
        assert_eq!(e.code, ErrorCode::ServerBusy);
        assert!(e.message.contains("this server's limit"), "{e}");
    }

    #[test]
    fn foreign_and_unknown_ids_look_the_same() {
        let s = store(JobsConfig::default());
        let (id, _) = s.insert("a", "render_postscript").unwrap();
        assert_eq!(s.status("a", &id).unwrap().state, JobState::Queued);
        let foreign = s.status("b", &id).unwrap_err();
        let unknown = s.status("b", "nope").unwrap_err();
        assert_eq!(foreign.code, ErrorCode::UnknownJob);
        assert_eq!(unknown.code, ErrorCode::UnknownJob);
        assert_eq!(
            foreign.message.replace(&id, "ID"),
            unknown.message.replace("nope", "ID")
        );
        assert!(s.request_cancel("b", &id).is_err());
        assert!(s.result("b", &id).is_err());
    }

    #[test]
    fn the_budget_reserves_the_largest_result_and_frees_fetched_ones() {
        let limits = LimitsConfig::default();
        let largest = JobsConfig::max_result_bytes(&limits);
        let s = store(JobsConfig {
            job_results_budget_bytes: largest * 2,
            ..JobsConfig::default()
        });
        let (a, _) = s.insert("p", "render_postscript").unwrap();
        let (b, _) = s.insert("p", "render_postscript").unwrap();
        let e = s.insert("p", "render_postscript").unwrap_err();
        assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded);
        // A finished job keeps only what it holds.
        s.finish(&a, Some(Err(RenderError::new(ErrorCode::Timeout, "slow"))));
        assert!(s.charged_bytes() < largest * 2);
        let e = s.insert("p", "render_postscript").unwrap_err();
        assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded, "unfetched kept");
        // Once fetched, it makes room.
        assert!(s.result("p", &a).is_ok_and(|r| r.outcome.is_err()));
        s.insert("p", "render_postscript").unwrap();
        assert_eq!(s.status("p", &a).unwrap_err().code, ErrorCode::UnknownJob);
        s.finish(&b, None);
        assert_eq!(s.result("p", &b).unwrap_err().code, ErrorCode::Cancelled);
    }

    #[test]
    fn finished_jobs_expire_after_the_ttl() {
        let s = store(JobsConfig {
            job_result_ttl_ms: 50,
            ..JobsConfig::default()
        });
        let (id, _) = s.insert("p", "render_postscript").unwrap();
        assert_eq!(
            s.result("p", &id).unwrap_err().code,
            ErrorCode::JobNotFinished
        );
        s.finish(&id, None);
        assert_eq!(s.status("p", &id).unwrap().state, JobState::Cancelled);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(s.status("p", &id).unwrap_err().code, ErrorCode::UnknownJob);
        assert_eq!(s.charged_bytes(), 0);
    }

    #[test]
    fn disabled_and_closed_stores_refuse_jobs() {
        let off = JobStore::new(JobsConfig::default(), &LimitsConfig::default());
        assert_eq!(
            off.insert("p", "render_postscript").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let s = store(JobsConfig::default());
        let (_, cancel) = s.insert("p", "render_postscript").unwrap();
        s.close();
        assert!(cancel.is_cancelled());
        assert_eq!(
            s.insert("p", "render_postscript").unwrap_err().code,
            ErrorCode::ServerBusy
        );
    }
}
