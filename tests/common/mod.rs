//! Shared helper for the tests that start the real server binary over stdio.
//!
//! It does two things the per-file copies it replaces did not:
//!
//! * the child's stderr is piped and drained into a bounded buffer for the
//!   whole life of the test, so a chatty server can never block on a full
//!   pipe (or on a parent that stopped reading);
//! * if `initialize` times out or fails, the panic message carries the
//!   child's stderr, its process state (`ps`) and the machine load, so a
//!   stall under load leaves evidence instead of one line.

use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tokio::io::AsyncReadExt as _;

pub type Client = RunningService<RoleClient, ()>;

/// How long a freshly started server gets to answer `initialize`. The
/// server does no work before it (no worker is started, nothing is read from
/// disk), so on an idle machine this takes milliseconds; the rest is
/// headroom for a loaded runner.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// The most stderr kept for the failure report (the tail).
const STDERR_KEEP: usize = 16 * 1024;

/// Start `exe serve-stdio` with the extra environment variables and return
/// a client that finished `initialize`.
pub async fn start_stdio(exe: impl AsRef<OsStr>, env: &[(&str, &str)]) -> Client {
    let cmd = tokio::process::Command::new(exe).configure(|c| {
        c.arg("serve-stdio")
            .env("PSCAT_MCP_LOG_FORMAT", "pretty")
            .env("RUST_LOG", "warn");
        for (k, v) in env {
            c.env(k, v);
        }
    });
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let pid = transport.id();
    let log = Arc::new(Mutex::new(Vec::<u8>::new()));
    if let Some(mut stderr) = stderr {
        let log = log.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                // Still show it: libtest prints a failing test's stderr.
                eprint!("{}", String::from_utf8_lossy(&buf[..n]));
                let mut log = log.lock().unwrap();
                log.extend_from_slice(&buf[..n]);
                if log.len() > STDERR_KEEP {
                    let cut = log.len() - STDERR_KEEP;
                    log.drain(..cut);
                }
            }
        });
    }
    let serve = ().serve(transport);
    tokio::pin!(serve);
    tokio::select! {
        done = &mut serve => match done {
            Ok(client) => client,
            Err(e) => panic!("initialize failed: {e}\n{}", report(pid, &log)),
        },
        _ = tokio::time::sleep(INITIALIZE_TIMEOUT) => {
            // Look at the child before dropping `serve` kills it.
            let report = report(pid, &log);
            panic!("initialize timed out after {INITIALIZE_TIMEOUT:?}\n{report}");
        }
    }
}

/// What is known about the child and the machine, for a failure message.
fn report(pid: Option<u32>, log: &Mutex<Vec<u8>>) -> String {
    let mut out = String::new();
    match pid {
        Some(pid) => {
            let ps = std::process::Command::new("ps")
                .args(["-o", "pid,ppid,stat,etime,time,pcpu,rss,command", "-p"])
                .arg(pid.to_string())
                .output();
            match ps {
                Ok(p) if p.status.success() => out.push_str(&format!(
                    "child process:\n{}",
                    String::from_utf8_lossy(&p.stdout)
                )),
                Ok(_) => out.push_str(&format!("child process {pid}: already gone\n")),
                Err(e) => out.push_str(&format!("child process {pid}: ps failed: {e}\n")),
            }
        }
        None => out.push_str("child process: already exited\n"),
    }
    if let Ok(up) = std::process::Command::new("uptime").output() {
        out.push_str(&format!(
            "load: {}\n",
            String::from_utf8_lossy(&up.stdout).trim()
        ));
    }
    let stderr = String::from_utf8_lossy(&log.lock().unwrap()).into_owned();
    if stderr.trim().is_empty() {
        out.push_str("child stderr: (empty)\n");
    } else {
        out.push_str(&format!("child stderr:\n{stderr}\n"));
    }
    out
}
