//! The sandbox signal: `get_renderer_info` reports `sandbox.worker_filter`
//! (what a worker said about its system-call filter in a job response) and
//! `sandbox.server_dumpable`, and the server logs each fact once.
//!
//! One real `serve-stdio` server answers `get_renderer_info`, three renders
//! and `get_renderer_info` again. Before any worker has run the filter is
//! `unknown`; afterwards it is `seccomp` on Linux and `none` elsewhere. The
//! startup line about the dumpable flag and the line about the worker's
//! filter each appear exactly once in the server's stderr, however many
//! renders ran. The Linux values (`seccomp`, `server_dumpable: false`) are
//! asserted on Linux only; on macOS the test checks `none` and `true`.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
/// How long to wait for one response, a render included.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

const WORKER_LINE: &str = "sandbox: the render worker";
const SERVER_LINE: &str = "sandbox: the server process";

struct Stdio2 {
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Stdio2 {
    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").expect("write to the server");
    }

    /// Call a tool and return its result object.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}));
        loop {
            let line = self
                .lines
                .recv_timeout(REPLY_TIMEOUT)
                .expect("a response before the timeout");
            let v: Value = serde_json::from_str(&line).expect("one JSON message per line");
            if v["id"] == json!(id) {
                assert!(v.get("error").is_none(), "{v}");
                return v["result"].clone();
            }
        }
    }
}

fn start() -> (std::process::Child, Stdio2, mpsc::Receiver<String>) {
    let mut child = Command::new(SERVER)
        .arg("serve-stdio")
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the server");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let (etx, errors) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if etx.send(line).is_err() {
                break;
            }
        }
    });
    let mut io = Stdio2 {
        stdin,
        lines,
        next_id: 1,
    };
    io.send(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "sandbox-signal-test", "version": "0"},
        }}),
    );
    let reply = io.lines.recv_timeout(REPLY_TIMEOUT).expect("initialize");
    assert!(reply.contains("\"result\""), "{reply}");
    io.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    (child, io, errors)
}

fn render(io: &mut Stdio2, n: u32) {
    let r = io.call(
        "render_postscript",
        json!({"source": format!("%!PS\n0 0 {n} {n} rectfill showpage\n"),
               "width": 40, "height": 40}),
    );
    assert_eq!(r["isError"], json!(false), "{r}");
}

#[test]
fn renderer_info_reports_the_sandbox_and_the_logs_say_it_once() {
    let (mut child, mut io, errors) = start();

    // No worker has run: the filter is not known yet, the dumpable flag is.
    let before = io.call("get_renderer_info", json!({}));
    let s = &before["structuredContent"];
    assert_eq!(s["sandbox"]["worker_filter"], json!("unknown"), "{s}");
    assert_eq!(
        s["sandbox"]["server_dumpable"],
        json!(!cfg!(target_os = "linux")),
        "{s}"
    );
    // Text and structuredContent carry the same information.
    let text: Value =
        serde_json::from_str(before["content"][0]["text"].as_str().expect("text content"))
            .expect("the text is the same JSON");
    assert_eq!(&text["sandbox"], &s["sandbox"]);
    // The fields that were there stay.
    for key in ["renderer", "server_version", "formats", "limits", "jobs"] {
        assert!(s.get(key).is_some(), "{key} is missing: {s}");
    }

    for n in [10, 20, 30] {
        render(&mut io, n);
    }

    let after = io.call("get_renderer_info", json!({}));
    let s = &after["structuredContent"];
    let want = if cfg!(target_os = "linux") {
        "seccomp"
    } else {
        "none"
    };
    assert_eq!(s["sandbox"]["worker_filter"], json!(want), "{s}");
    assert_eq!(
        s["sandbox"]["server_dumpable"],
        json!(!cfg!(target_os = "linux")),
        "{s}"
    );
    assert_eq!(s["sandbox"].as_object().unwrap().len(), 2, "{s}");

    // End of input ends the server; its stderr is then complete.
    drop(io);
    let status = child.wait().expect("the server stops at end of input");
    assert!(status.success(), "{status}");
    let log: Vec<String> = errors.iter().collect();
    let count = |needle: &str| log.iter().filter(|l| l.contains(needle)).count();
    assert_eq!(count("render started"), 3, "{log:#?}");
    assert_eq!(count(WORKER_LINE), 1, "{log:#?}");
    assert_eq!(count(SERVER_LINE), 1, "{log:#?}");
    let worker = log.iter().find(|l| l.contains(WORKER_LINE)).unwrap();
    assert!(
        worker.contains(&format!("\"worker_filter\":\"{want}\"")),
        "{worker}"
    );
    let server = log.iter().find(|l| l.contains(SERVER_LINE)).unwrap();
    assert!(
        server.contains(&format!(
            "\"server_dumpable\":{}",
            !cfg!(target_os = "linux")
        )),
        "{server}"
    );
    // The worker's line comes after the first render started, not at startup.
    let at = |needle: &str| log.iter().position(|l| l.contains(needle)).unwrap();
    assert!(at(SERVER_LINE) < at("render started"));
    assert!(at("render started") < at(WORKER_LINE));
}

/// The worker names its filter in its job response, and the backend keeps it
/// from the first job: unknown before, the same afterwards, a failed job
/// included.
#[tokio::test]
async fn the_worker_reports_its_filter_in_its_job_response() {
    use pscat_mcp::backend::{RenderBackend as _, SubprocessBackend};
    use pscat_mcp::config::LimitsConfig;
    use pscat_mcp::sandbox::WorkerFilter;
    use pscat_mcp::worker::{InterpLimits, JobMode, OutputFormat, WorkerJob};

    let want = if cfg!(target_os = "linux") {
        WorkerFilter::Seccomp
    } else {
        WorkerFilter::None
    };
    let backend = SubprocessBackend::new(SERVER.into(), LimitsConfig::default());
    assert_eq!(backend.worker_filter(), None);
    let job = |source: &str| WorkerJob {
        mode: JobMode::Render,
        source: source.into(),
        format: OutputFormat::Png,
        page_points: Some((40, 40)),
        dpi: 72.0,
        halftone: false,
        limits: InterpLimits {
            max_steps: Some(1_000_000),
            max_time_ms: Some(10_000),
            max_memory_bytes: Some(256 * 1024 * 1024),
            max_page_pixels: 16_000_000,
            max_pages: 10,
            max_output_bytes: 8 * 1024 * 1024,
            max_program_output_bytes: 16 * 1024,
        },
        generate: None,
        decided_height: None,
        sheet: None,
    };
    // A program that fails still gets an answer that names the filter.
    let failed = backend
        .execute(job("nosuchoperator"), Duration::from_secs(30))
        .await
        .expect("a result");
    assert!(failed.failure.is_some());
    assert_eq!(failed.sandbox.as_deref(), Some(want.as_str()));
    assert_eq!(backend.worker_filter(), Some(want));
    let ok = backend
        .execute(job("0 0 10 10 rectfill showpage"), Duration::from_secs(30))
        .await
        .expect("a result");
    assert!(ok.failure.is_none(), "{:?}", ok.failure);
    assert_eq!(ok.sandbox.as_deref(), Some(want.as_str()));
    assert_eq!(backend.worker_filter(), Some(want));
}
