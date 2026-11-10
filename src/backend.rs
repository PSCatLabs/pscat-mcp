//! Backends that execute a [`WorkerJob`].
//!
//! The production backend is [`SubprocessBackend`]: one short-lived
//! `render-worker` child process per job, killed at the deadline.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

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

/// Most bytes of worker stderr kept for diagnostics; the rest is drained.
const STDERR_KEEP_BYTES: usize = 8 * 1024;
/// Open file descriptors allowed in the worker (stdio plus a few spare).
#[cfg(unix)]
const WORKER_NOFILE: u64 = 64;
/// Extra CPU seconds past the deadline before the kernel kills the worker.
#[cfg(unix)]
const CPU_LIMIT_SLACK_SECS: u64 = 2;
/// How long to wait for a killed worker to be reaped before giving up on it
/// (the runtime's orphan reaper finishes the job).
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs each job in a fresh `render-worker` child process.
#[derive(Debug, Clone)]
pub struct SubprocessBackend {
    /// Executable started with the `render-worker` subcommand.
    pub exe: PathBuf,
    /// Limits used for the child's OS rlimits and output caps.
    pub limits: LimitsConfig,
    /// Passed to the worker as `--tag <tag>` when set. The worker ignores
    /// it; it lets tests find their worker processes (`pgrep -f <tag>`).
    pub tag: Option<String>,
}

impl SubprocessBackend {
    /// A backend starting `exe render-worker` for each job.
    pub fn new(exe: PathBuf, limits: LimitsConfig) -> Self {
        SubprocessBackend {
            exe,
            limits,
            tag: None,
        }
    }

    /// The same backend, passing `--tag <tag>` to every worker.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Most bytes accepted on the worker's stdout, from the configured
    /// limits (jobs built by the service never exceed them): base64
    /// artifacts (4/3 of the raw cap), JSON-escaped program output (at most 6 bytes per input
    /// byte, `\u0000`) and 1 MiB for everything else.
    fn stdout_cap(&self) -> usize {
        let l = &self.limits;
        (l.max_output_bytes / 3)
            .saturating_mul(4)
            .saturating_add(4)
            .saturating_add(l.max_program_output_bytes.saturating_mul(6))
            .saturating_add(1 << 20)
    }

    fn command(&self, deadline: Duration) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("render-worker");
        if let Some(tag) = &self.tag {
            cmd.arg("--tag").arg(tag);
        }
        cmd.env_clear()
            .env("PSCAT_NO_CHECKOUT", "1")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            let cpu_secs = deadline
                .as_secs()
                .saturating_add(u64::from(deadline.subsec_nanos() > 0))
                .saturating_add(CPU_LIMIT_SLACK_SECS);
            #[cfg(target_os = "linux")]
            let address_space = self.limits.address_space();
            // SAFETY: the closure runs in the forked child before exec and
            // only makes async-signal-safe system calls (getrlimit,
            // setrlimit, setsid); it does not allocate or take locks.
            unsafe {
                cmd.pre_exec(move || {
                    sys::limit(libc::RLIMIT_CORE, 0, 0)?;
                    sys::limit(libc::RLIMIT_CPU, cpu_secs, cpu_secs.saturating_add(1))?;
                    // Writes to regular files fail (SIGXFSZ); pipes are exempt.
                    sys::limit(libc::RLIMIT_FSIZE, 0, 0)?;
                    sys::limit(libc::RLIMIT_NOFILE, WORKER_NOFILE, WORKER_NOFILE)?;
                    #[cfg(target_os = "linux")]
                    if let Some(bytes) = address_space {
                        sys::limit(libc::RLIMIT_AS, bytes, bytes)?;
                    }
                    // Own session and process group, so the whole group can
                    // be killed at the deadline.
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = deadline;
        cmd
    }

    async fn run(&self, job: WorkerJob, deadline: Duration) -> Result<WorkerResult, BackendError> {
        let started = Instant::now();
        let input = serde_json::to_vec(&job)
            .map_err(|e| BackendError::Protocol(format!("cannot encode the job: {e}")))?;
        let source_bytes = job.source.len();
        let stdout_cap = self.stdout_cap();
        drop(job);

        let child = self.command(deadline).spawn().map_err(|e| {
            tracing::warn!(exe = %self.exe.display(), error = %e, "cannot start the render worker");
            BackendError::Spawn(format!("{}: {e}", self.exe.display()))
        })?;
        let mut worker = Worker::new(child);
        tracing::debug!(
            pid = worker.pid,
            source_bytes,
            job_bytes = input.len(),
            deadline_ms = deadline.as_millis() as u64,
            "render worker started"
        );

        let outcome = tokio::time::timeout(deadline, worker.communicate(input, stdout_cap)).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (status, stdout, stderr) = match outcome {
            Err(_) => {
                worker.kill_and_reap().await;
                tracing::warn!(
                    pid = worker.pid,
                    source_bytes,
                    elapsed_ms,
                    "render worker passed its deadline and was killed"
                );
                return Err(BackendError::Timeout);
            }
            Ok(Err(CommError::Overflow(bytes))) => {
                worker.kill_and_reap().await;
                tracing::warn!(
                    pid = worker.pid,
                    source_bytes,
                    bytes,
                    cap = stdout_cap,
                    "render worker output passed the cap and was killed"
                );
                return Err(BackendError::OutputOverflow(stdout_cap));
            }
            Ok(Err(CommError::Io(e))) => {
                worker.kill_and_reap().await;
                tracing::warn!(pid = worker.pid, error = %e, "talking to the render worker failed");
                return Err(BackendError::Crashed(format!("worker I/O failed: {e}")));
            }
            Ok(Ok(done)) => done,
        };

        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();
        if !status.success() {
            tracing::warn!(
                pid = worker.pid,
                status = %status,
                source_bytes,
                elapsed_ms,
                stderr_bytes = stderr.len(),
                "render worker exited abnormally"
            );
            let mut message = describe(status);
            if !stderr.is_empty() {
                message.push_str(": ");
                message.push_str(stderr);
            }
            return Err(BackendError::Crashed(message));
        }

        match serde_json::from_slice::<WorkerResult>(&stdout) {
            Ok(result) => {
                tracing::debug!(
                    pid = worker.pid,
                    source_bytes,
                    stdout_bytes = stdout.len(),
                    elapsed_ms,
                    steps = result.steps_used,
                    failed = result.failure.is_some(),
                    "render worker finished"
                );
                Ok(result)
            }
            Err(e) => {
                tracing::warn!(
                    pid = worker.pid,
                    stdout_bytes = stdout.len(),
                    error = %e,
                    "render worker output is not a result"
                );
                Err(BackendError::Protocol(format!(
                    "{e} ({} bytes of output)",
                    stdout.len()
                )))
            }
        }
    }
}

impl RenderBackend for SubprocessBackend {
    fn execute(&self, job: WorkerJob, deadline: Duration) -> BackendFuture<'_> {
        Box::pin(self.run(job, deadline))
    }
}

/// Exit status in words, naming the signal for a killed worker.
fn describe(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            let why = match signal {
                libc::SIGXCPU => " (CPU time limit)",
                libc::SIGXFSZ => " (file size limit)",
                libc::SIGKILL => " (killed, possibly out of memory)",
                libc::SIGSEGV | libc::SIGBUS => " (memory fault)",
                libc::SIGABRT => " (aborted, possibly out of memory)",
                _ => "",
            };
            return format!("worker killed by signal {signal}{why}");
        }
    }
    match status.code() {
        Some(code) => format!("worker exited with status {code}"),
        None => format!("worker ended: {status}"),
    }
}

enum CommError {
    Overflow(usize),
    Io(std::io::Error),
}

/// A running worker. Dropping it (for example when the request future is
/// cancelled) kills the worker's whole process group; `kill_on_drop` on the
/// [`Child`] then kills the worker itself and the runtime reaps it.
struct Worker {
    child: Child,
    pid: Option<u32>,
    /// Cleared once the worker has been reaped: its pid may then be reused,
    /// so the group must not be signalled again.
    live: bool,
}

impl Worker {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Worker {
            child,
            pid,
            live: true,
        }
    }

    /// SIGKILL the worker's process group (the worker is its leader).
    fn kill_group(&self) {
        kill_group_of(self.pid, self.live);
    }

    /// Kill the worker and wait for it, so it never outlives the request.
    async fn kill_and_reap(&mut self) {
        self.kill_group();
        let _ = self.child.start_kill();
        match tokio::time::timeout(REAP_TIMEOUT, self.child.wait()).await {
            Ok(_) => self.live = false,
            Err(_) => tracing::warn!(
                pid = self.pid,
                "killed render worker was not reaped in time"
            ),
        }
    }

    /// Send the job, collect stdout (capped) and stderr (truncated), and
    /// wait for the exit.
    async fn communicate(
        &mut self,
        input: Vec<u8>,
        stdout_cap: usize,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), CommError> {
        let (Some(mut stdin), Some(stdout), Some(stderr)) = (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        ) else {
            return Err(CommError::Io(std::io::Error::other("worker pipes missing")));
        };
        let write = async move {
            // A worker that exits early closes its stdin; its result (or
            // exit status) says why, so a broken pipe here is not an error.
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
            drop(stdin);
        };
        let pid = self.pid;
        let live = self.live;
        let out = async move {
            let ((), out) = tokio::join!(write, read_capped(stdout, stdout_cap));
            if out.is_err() {
                // Stop the worker now so stderr reaches EOF below.
                kill_group_of(pid, live);
            }
            out
        };
        let (out, err) = tokio::join!(out, read_truncated(stderr, STDERR_KEEP_BYTES));
        let stdout = out?;
        let stderr = err.map_err(CommError::Io)?;
        let status = self.child.wait().await.map_err(CommError::Io)?;
        self.live = false;
        Ok((status, stdout, stderr))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.live {
            tracing::debug!(
                pid = self.pid,
                "render worker dropped while running; killing it"
            );
            self.kill_group();
        }
    }
}

/// SIGKILL the process group led by `pid`, unless the leader was reaped.
fn kill_group_of(pid: Option<u32>, live: bool) {
    #[cfg(unix)]
    if live
        && let Some(pid) = pid
        && let Ok(pgid) = libc::pid_t::try_from(pid)
        && pgid > 0
    {
        // SAFETY: plain system call. The group exists while its leader is
        // unreaped, so the id cannot name another process group.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, live);
}

/// Read to EOF, failing with the byte count once more than `cap` arrive.
async fn read_capped(mut r: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, CommError> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut chunk).await.map_err(CommError::Io)?;
        if n == 0 {
            return Ok(buf);
        }
        if buf.len() + n > cap {
            return Err(CommError::Overflow(buf.len() + n));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read to EOF keeping the first `keep` bytes and discarding the rest, so a
/// chatty worker never blocks on a full pipe.
async fn read_truncated(mut r: impl AsyncRead + Unpin, keep: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok(buf);
        }
        let room = keep.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }
}

#[cfg(unix)]
mod sys {
    /// Type of the `RLIMIT_*` constants (glibc uses its own enum type).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(super) type Resource = libc::__rlimit_resource_t;
    /// Type of the `RLIMIT_*` constants.
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    pub(super) type Resource = libc::c_int;

    /// Lower a resource limit to `soft`/`hard`, never above the current hard
    /// limit (raising it would fail without privilege). Async-signal-safe:
    /// two system calls, no allocation.
    pub(super) fn limit(resource: Resource, soft: u64, hard: u64) -> std::io::Result<()> {
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `cur` is a valid, writable rlimit.
        if unsafe { libc::getrlimit(resource, &mut cur) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let clamp = |v: u64| -> libc::rlim_t {
            let v = v as libc::rlim_t;
            if cur.rlim_max == libc::RLIM_INFINITY {
                v
            } else {
                v.min(cur.rlim_max)
            }
        };
        let hard = clamp(hard);
        let new = libc::rlimit {
            rlim_cur: clamp(soft).min(hard),
            rlim_max: hard,
        };
        // SAFETY: `new` is a valid rlimit.
        if unsafe { libc::setrlimit(resource, &new) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
