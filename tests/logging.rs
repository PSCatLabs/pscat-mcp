//! The default log filter keeps client-chosen strings out of the logs.
//!
//! rmcp logs "Service initialized as server" at INFO from `rmcp::service`
//! with a Debug dump of the client's `initialize` parameters (`clientInfo`,
//! capabilities). The server's default filter turns that target down to
//! WARN; `RUST_LOG` still overrides the default completely. Each test sends
//! an `initialize` whose `clientInfo.name` is a marker, then checks the
//! server's stderr: at the default filter the marker is absent, and with
//! `RUST_LOG=rmcp=info` it is present, so the first check cannot pass for
//! want of the line.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const TOKEN: &str = "test-token-0123456789";
const MARKER: &str = "client-marker-7f3a9c";
const LINE: &str = "Service initialized as server";

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": MARKER, "version": "9.9.9"},
    }})
}

fn initialized() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

fn tools_list() -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
}

/// Run `serve-stdio` through the handshake and one request, and return its
/// stderr. `rust_log` of `None` leaves `RUST_LOG` unset.
fn stdio_stderr(rust_log: Option<&str>) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new(SERVER);
    cmd.arg("serve-stdio")
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(v) = rust_log {
        cmd.env("RUST_LOG", v);
    }
    let mut child = cmd.spawn().expect("start the server");
    let mut stdin = child.stdin.take().unwrap();
    for msg in [initialize(), initialized(), tools_list()] {
        writeln!(stdin, "{msg}").unwrap();
    }
    // End of input ends the server, after it has handled the lines above in
    // order: the handshake is logged before `tools/list` is served.
    drop(stdin);
    // A server that does not stop on end of input is killed after a deadline
    // rather than hanging the test.
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()));
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(out) => String::from_utf8_lossy(&out.expect("server output").stderr).into_owned(),
        Err(_) => {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            panic!("serve-stdio did not stop at end of input");
        }
    }
}

#[test]
fn stdio_default_filter_leaves_the_client_info_out() {
    let err = stdio_stderr(None);
    assert!(!err.contains(MARKER), "client string in the logs: {err}");
    assert!(!err.contains("peer_info"), "{err}");
    assert!(!err.contains(LINE), "{err}");
}

#[test]
fn stdio_rust_log_overrides_the_default() {
    let err = stdio_stderr(Some("rmcp=info"));
    assert!(err.contains(LINE), "the line is not emitted at all: {err}");
    assert!(err.contains(MARKER), "{err}");
}

struct Server {
    child: tokio::process::Child,
    base: String,
    logs: tokio::task::JoinHandle<String>,
    /// The stderr lines read so far, for a test that waits for one.
    seen: Arc<Mutex<String>>,
}

async fn start_http(rust_log: Option<&str>) -> Server {
    start_http_with(rust_log, &[]).await
}

/// `serve-http` on a free port with extra environment variables.
async fn start_http_with(rust_log: Option<&str>, env: &[(&str, &str)]) -> Server {
    let mut cmd = tokio::process::Command::new(SERVER);
    cmd.args(["serve-http", "--bind", "127.0.0.1", "--port", "0"])
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env_remove("RUST_LOG")
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(v) = rust_log {
        cmd.env("RUST_LOG", v);
    }
    let mut child = cmd.spawn().expect("start the server");
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    // The listening line is the server's own, at INFO, under either filter.
    let mut collected = String::new();
    let mut addr = String::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        collected.push_str(&line);
        collected.push('\n');
        if let Some(i) = line.find("\"addr\":\"") {
            addr = line[i + 8..].split('"').next().unwrap().to_string();
            break;
        }
    }
    assert!(!addr.is_empty(), "serve-http did not report an address");
    let seen = Arc::new(Mutex::new(collected.clone()));
    let shared = Arc::clone(&seen);
    let logs = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            let mut s = shared.lock().unwrap();
            s.push_str(&line);
            s.push('\n');
        }
        collected
    });
    Server {
        child,
        base: format!("http://{addr}/mcp"),
        logs,
        seen,
    }
}

/// POST one JSON-RPC message to the server, as a client would, and return the
/// status and the body.
async fn post(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    protocol_version: Option<&str>,
) -> (u16, String) {
    let mut req = http
        .post(url)
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(v) = protocol_version {
        req = req.header("mcp-protocol-version", v);
    }
    let resp = req.send().await.expect("request");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

/// Send `initialize` and then `tools/list` over HTTP, then stop the server
/// and return its stderr. The server is stateless: each request is served by
/// its own rmcp service, which logs its handshake before it answers.
async fn http_stderr(rust_log: Option<&str>) -> String {
    let mut server = start_http(rust_log).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let (status, body) = post(&http, &server.base, &initialize(), None).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pscat-mcp"), "{body}");
    let (status, body) = post(&http, &server.base, &tools_list(), Some("2025-06-18")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("render_postscript"), "{body}");

    server.child.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), server.logs)
        .await
        .expect("stderr closed")
        .unwrap()
}

// Over HTTP the server is stateless, and rmcp 3.9 builds the dumped
// `peer_info` itself (the protocol version from the request, a default
// `clientInfo`) instead of taking it from the client, so the marker is not in
// that line. The line is still one per request, and with the override it is
// there, so these two tests hold the same filter on the HTTP path.

#[tokio::test]
async fn http_default_filter_leaves_the_handshake_line_out() {
    let err = http_stderr(None).await;
    assert!(err.contains("listening"), "no server log at all: {err}");
    assert!(!err.contains(MARKER), "client string in the logs: {err}");
    assert!(!err.contains("peer_info"), "{err}");
    assert!(!err.contains(LINE), "{err}");
}

#[tokio::test]
async fn http_rust_log_overrides_the_default() {
    let err = http_stderr(Some("info,rmcp=info")).await;
    assert!(err.contains(LINE), "the line is not emitted at all: {err}");
    assert!(err.contains("peer_info"), "{err}");
}

// ------------------------------------------------- error codes in the logs
//
// pscat-cloud's monitoring counts failed renders by `code` (and `limit`) and
// the queue timeouts by `code=server_busy`. The values are the wire codes,
// from a fixed vocabulary: nothing the program wrote reaches a log line.

/// Strings a program puts in its source, its names and its error command, and
/// that must never appear in a log line.
const SOURCE_MARKER: &str = "src-marker-5b1e";
const NAME_MARKER: &str = "name-marker-77aa";
const COMMAND_MARKER: &str = "cmd-marker-c0de";

fn render_call(id: u64, source: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
        "name": "render_postscript",
        "arguments": {"source": source, "width": 20, "height": 20},
    }})
}

/// The JSON log lines of `logs` (others are skipped).
fn json_lines(logs: &str) -> Vec<Value> {
    logs.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Every line whose message is `message`. A line is one flat JSON object:
/// `timestamp`, `level`, `message`, its fields, `target` and `span`.
fn lines_with(logs: &str, message: &str) -> Vec<Value> {
    json_lines(logs)
        .into_iter()
        .filter(|l| l["message"] == message)
        .collect()
}

/// Call `render_postscript` and return the wire error code, if the call
/// failed.
async fn render_code(http: &reqwest::Client, base: &str, id: u64, source: &str) -> Option<String> {
    let (status, body) = post(http, base, &render_call(id, source), Some("2025-06-18")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body)
        .or_else(|_| {
            // An SSE body: the last `data:` event.
            let data = body
                .lines()
                .rev()
                .find_map(|l| l.strip_prefix("data:"))
                .unwrap_or("{}");
            serde_json::from_str(data.trim())
        })
        .unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
    v["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .map(str::to_string)
}

fn no_program_text(logs: &str) {
    for marker in [SOURCE_MARKER, NAME_MARKER, COMMAND_MARKER, TOKEN] {
        assert!(!logs.contains(marker), "{marker} in the logs: {logs}");
    }
}

async fn stop(mut server: Server) -> String {
    server.child.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), server.logs)
        .await
        .expect("stderr closed")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_render_logs_its_error_code_and_limit() {
    let server = start_http_with(
        None,
        &[
            ("PSCAT_MCP_MAX_STEPS", "1000000"),
            ("PSCAT_MCP_MAX_MEMORY_BYTES", "33554432"),
        ],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    // The marker strings are in the program: as a string, as a name the
    // program defines, and as the command the error names.
    let prefix = format!("({SOURCE_MARKER}) pop /{NAME_MARKER} 1 def ");
    let programs = [
        (
            "limit_exceeded",
            format!("{prefix}[ 0 1 10000000 {{ }} for ]"),
        ),
        ("limit_exceeded", format!("{prefix}{{ }} loop")),
        ("postscript_error", format!("{prefix}{COMMAND_MARKER}")),
        ("postscript_error", format!("{prefix}nosuchop")),
    ];
    for (i, (want, src)) in programs.iter().enumerate() {
        let got = render_code(&http, &server.base, i as u64 + 1, src).await;
        assert_eq!(got.as_deref(), Some(*want), "program {i}");
    }
    // A render that succeeds.
    assert_eq!(
        render_code(&http, &server.base, 9, &format!("{prefix}showpage")).await,
        None
    );

    let logs = stop(server).await;
    let finished = lines_with(&logs, "render finished");
    assert_eq!(finished.len(), 5, "{logs}");
    let field = |i: usize, name: &str| finished[i][name].as_str().map(str::to_string);
    let expect = [
        (true, Some("limit_exceeded"), Some("memory")),
        (true, Some("limit_exceeded"), Some("steps")),
        (true, Some("postscript_error"), None),
        (true, Some("postscript_error"), None),
        (false, None, None),
    ];
    for (i, (failed, code, limit)) in expect.iter().enumerate() {
        assert_eq!(finished[i]["failed"], *failed, "{}", finished[i]);
        assert_eq!(field(i, "code").as_deref(), *code, "{}", finished[i]);
        assert_eq!(field(i, "limit").as_deref(), *limit, "{}", finished[i]);
    }
    no_program_text(&logs);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interpreter_time_limit_logs_limit_time() {
    let server = start_http_with(
        None,
        &[
            ("PSCAT_MCP_MAX_STEPS", "0"),
            ("PSCAT_MCP_INTERP_TIME_LIMIT_MS", "300"),
        ],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let src = format!("({SOURCE_MARKER}) pop {{ }} loop");
    let got = render_code(&http, &server.base, 1, &src).await;
    assert_eq!(got.as_deref(), Some("limit_exceeded"));
    let logs = stop(server).await;
    let finished = lines_with(&logs, "render finished");
    assert_eq!(finished.len(), 1, "{logs}");
    assert_eq!(finished[0]["code"], "limit_exceeded", "{logs}");
    assert_eq!(finished[0]["limit"], "time", "{logs}");
    no_program_text(&logs);
}

/// The one slot is held by a program that never ends (no step or time limit,
/// a 60 s deadline), so the second request waits out the queue timeout
/// whatever the load; the test ends the first by killing the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_queue_timeout_logs_server_busy_with_the_wait() {
    const QUEUE_MS: u64 = 300;
    let server = start_http_with(
        None,
        &[
            ("PSCAT_MCP_MAX_CONCURRENT_RENDERS", "1"),
            ("PSCAT_MCP_QUEUE_TIMEOUT_MS", &QUEUE_MS.to_string()),
            ("PSCAT_MCP_RENDER_TIMEOUT_MS", "60000"),
            ("PSCAT_MCP_INTERP_TIME_LIMIT_MS", "0"),
            ("PSCAT_MCP_MAX_STEPS", "0"),
        ],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let (base, h) = (server.base.clone(), http.clone());
    let slow = tokio::spawn(async move {
        render_code(&h, &base, 1, &format!("({SOURCE_MARKER}) pop {{ }} loop")).await
    });
    // The slow program holds the slot once its render has started.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !server.seen.lock().unwrap().contains("render started") {
        assert!(std::time::Instant::now() < deadline, "no render started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let got = render_code(&http, &server.base, 2, &format!("({SOURCE_MARKER}) pop")).await;
    assert_eq!(got.as_deref(), Some("server_busy"));
    assert!(!slow.is_finished(), "the slow program ended by itself");
    // Ends the slow render (the server kills its worker when the client
    // goes) before the server is stopped.
    slow.abort();

    let logs = stop(server).await;
    let busy: Vec<Value> = json_lines(&logs)
        .into_iter()
        .filter(|l| l["code"] == "server_busy")
        .collect();
    assert_eq!(busy.len(), 1, "{logs}");
    let waited = busy[0]["queue_wait_ms"].as_u64().expect("queue_wait_ms");
    // The timer never fires early; a loaded machine only makes it later.
    assert!(
        (QUEUE_MS..QUEUE_MS + 30_000).contains(&waited),
        "queue_wait_ms {waited}"
    );
    assert_eq!(lines_with(&logs, "render started").len(), 1, "{logs}");
    no_program_text(&logs);
}
