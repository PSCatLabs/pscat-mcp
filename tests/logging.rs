//! The default log filter keeps client-chosen strings out of the logs.
//!
//! rmcp logs "Service initialized as server" at INFO from `rmcp::service`
//! with a Debug dump of the client's `initialize` parameters (`clientInfo`,
//! capabilities). The server's default filter turns that target down to
//! WARN, and `src/telemetry.rs` holds every `rmcp` target to WARN whatever
//! `RUST_LOG` says. Each test sends an `initialize` whose `clientInfo.name`
//! is a marker, then checks the server's stderr: the marker is absent at the
//! default filter and with `RUST_LOG` raising `rmcp` to INFO and to TRACE.

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
fn stdio_rust_log_cannot_bring_the_client_info_back() {
    for rust_log in ["rmcp=info", "rmcp=trace", "trace"] {
        let err = stdio_stderr(Some(rust_log));
        assert!(!err.contains(MARKER), "RUST_LOG={rust_log}: {err}");
        assert!(!err.contains("peer_info"), "RUST_LOG={rust_log}: {err}");
        assert!(!err.contains(LINE), "RUST_LOG={rust_log}: {err}");
    }
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
// that line. The line is still one per request (when rmcp is allowed to log
// it), so these two tests hold the same filter on the HTTP path.

#[tokio::test]
async fn http_default_filter_leaves_the_handshake_line_out() {
    let err = http_stderr(None).await;
    assert!(err.contains("listening"), "no server log at all: {err}");
    assert!(!err.contains(MARKER), "client string in the logs: {err}");
    assert!(!err.contains("peer_info"), "{err}");
    assert!(!err.contains(LINE), "{err}");
}

#[tokio::test]
async fn http_rust_log_cannot_bring_the_handshake_line_back() {
    for rust_log in ["info,rmcp=info", "info,rmcp=trace", "trace"] {
        let err = http_stderr(Some(rust_log)).await;
        assert!(err.contains("listening"), "no server log at all: {err}");
        assert!(!err.contains(LINE), "RUST_LOG={rust_log}: {err}");
        assert!(!err.contains("peer_info"), "RUST_LOG={rust_log}: {err}");
    }
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

// ------------------------------------------------ artifact links in the logs
//
// A link to a stored artifact is a capability (a signed URL will be one
// outright): no log line and no capture record may hold it, its render id,
// or the artifact's bytes (#139).

/// Render with `delivery: url` and read the link back, then return the
/// render id, a base64 chunk of the artifact, the logs and the capture file.
async fn links_round_trip(rust_log: Option<&str>) -> (String, String, String, String) {
    let capture = std::env::temp_dir().join(format!(
        "pscat-mcp-links-capture-{}.jsonl",
        uuid::Uuid::new_v4().simple()
    ));
    let capture_path = capture.to_string_lossy().into_owned();
    let server = start_http_with(
        rust_log,
        &[
            ("PSCAT_MCP_ARTIFACT_LINKS", "memory"),
            ("PSCAT_MCP_CAPTURE_FILE", &capture_path),
        ],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": "render_postscript",
        "arguments": {"source": "0.3 setgray 0 0 40 40 rectfill", "width": 40, "height": 40,
                      "delivery": "url"},
    }});
    let (status, body) = post(&http, &server.base, &call, Some("2025-06-18")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    let id = v["result"]["structuredContent"]["render_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no render_id: {body}"))
        .to_string();
    let uri = format!("pscat://render/{id}/1");
    let read = json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read",
                      "params": {"uri": uri}});
    // As a 2026-07-28 client would, the read names the URI in `Mcp-Name`
    // too, a header capture mode records.
    let resp = http
        .post(&server.base)
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .header("mcp-method", "resources/read")
        .header("mcp-name", &uri)
        .body(read.to_string())
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).expect("json");
    let blob = v["result"]["contents"][0]["blob"]
        .as_str()
        .unwrap_or_else(|| panic!("no blob: {body}"));
    let chunk = blob[blob.len() / 2..blob.len() / 2 + 24.min(blob.len() / 2)].to_string();
    // A read of a link that does not exist: the error must not echo it.
    let missing = json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
                         "params": {"uri": format!("pscat://render/{id}/9")}});
    let (_, body) = post(&http, &server.base, &missing, Some("2025-06-18")).await;
    assert!(body.contains("error"), "{body}");
    assert!(
        !body.contains(&id),
        "the not-found error echoes the link: {body}"
    );
    let logs = stop(server).await;
    let captured = std::fs::read_to_string(&capture).unwrap_or_default();
    let _ = std::fs::remove_file(&capture);
    (id, chunk, logs, captured)
}

fn no_links(id: &str, chunk: &str, logs: &str, captured: &str) {
    assert!(logs.contains("listening"), "no server log at all: {logs}");
    for (what, text) in [("logs", logs), ("capture file", captured)] {
        assert!(!text.contains(id), "the render id is in the {what}: {text}");
        assert!(!text.contains(chunk), "artifact bytes are in the {what}");
    }
    assert!(!logs.contains("pscat://render/"), "{logs}");
    // The capture recorded the read, with the link redacted.
    assert!(captured.contains("resources/read"), "{captured}");
    assert!(
        captured.contains("pscat://render/[redacted]/1"),
        "{captured}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn links_never_reach_the_logs_or_the_capture_file() {
    let (id, chunk, logs, captured) = links_round_trip(None).await;
    no_links(&id, &chunk, &logs, &captured);
}

/// The server's own code at its most verbose: still no link.
#[tokio::test(flavor = "multi_thread")]
async fn links_stay_out_of_the_logs_at_trace() {
    let (id, chunk, logs, captured) = links_round_trip(Some("info,pscat_mcp=trace")).await;
    no_links(&id, &chunk, &logs, &captured);
}

/// `rmcp` logs whole messages at DEBUG and TRACE; `RUST_LOG` cannot turn
/// that on (the `rmcp` clamp in `src/telemetry.rs`). A `RUST_LOG` that names
/// rmcp, and one that raises everything: neither the render id, a link nor
/// the bytes are in the logs, and no rmcp line is below WARN.
#[tokio::test(flavor = "multi_thread")]
async fn rmcp_trace_cannot_log_links() {
    for rust_log in ["rmcp=trace,pscat_mcp=trace", "trace"] {
        let (id, chunk, logs, captured) = links_round_trip(Some(rust_log)).await;
        no_links(&id, &chunk, &logs, &captured);
        assert_no_rmcp_below_warn(&logs);
    }
}

/// Every JSON line whose target is `rmcp` or under it is WARN or ERROR.
fn assert_no_rmcp_below_warn(logs: &str) {
    for line in json_lines(logs) {
        let target = line["target"].as_str().unwrap_or_default();
        if target == "rmcp" || target.starts_with("rmcp::") {
            let level = line["level"].as_str().unwrap_or_default();
            assert!(
                level == "WARN" || level == "ERROR",
                "an rmcp {level} line got through: {line}"
            );
        }
    }
}

/// The clamp keeps rmcp's WARN lines: a request that carries a browser
/// `Origin` is rejected by rmcp, which logs a WARN, at `trace` as well.
#[tokio::test(flavor = "multi_thread")]
async fn rmcp_warnings_still_get_through_at_trace() {
    let mut server = start_http(Some("trace")).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp = http
        .post(&server.base)
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("origin", "https://elsewhere.example")
        .body(initialize().to_string())
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 403);
    // The server logs from its own tasks: wait for the line.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !server.seen.lock().unwrap().contains("disallowed Origin") {
        assert!(
            std::time::Instant::now() < deadline,
            "no rmcp WARN line: {}",
            server.seen.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.child.kill().await.unwrap();
    let logs = tokio::time::timeout(Duration::from_secs(30), server.logs)
        .await
        .expect("stderr closed")
        .unwrap();
    let warned: Vec<Value> = json_lines(&logs)
        .into_iter()
        .filter(|l| {
            l["level"] == "WARN"
                && l["target"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("rmcp::"))
        })
        .collect();
    assert_eq!(warned.len(), 1, "{logs}");
    assert_no_rmcp_below_warn(&logs);
}

/// A short-link path (`/a/<render_id>/...`, #163) is logged as the fixed
/// route name, and its query not at all, whether or not short links are on
/// and at every filter.
#[tokio::test(flavor = "multi_thread")]
async fn a_short_link_path_and_query_never_reach_the_logs() {
    const ID: &str = "4f1c2b3a-5d6e-4f70-8a9b-0c1d2e3f4a5b";
    const TOKEN_TEXT: &str = "tokenMarker0123456789x";
    for rust_log in [None, Some("info,pscat_mcp=trace"), Some("trace")] {
        let server = start_http(rust_log).await;
        let root = server.base.strip_suffix("/mcp").unwrap().to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = http
            .get(format!(
                "{root}/a/{ID}/1.png?e=1999999999&s={TOKEN_TEXT}&utm_source=chatgpt.com"
            ))
            .send()
            .await
            .unwrap();
        // No short links on this server: not found.
        assert_eq!(resp.status().as_u16(), 404, "{rust_log:?}");
        let logs = stop(server).await;
        assert!(logs.contains("/a/[redacted]"), "{rust_log:?}: {logs}");
        for secret in [ID, TOKEN_TEXT, "utm_source", "1999999999"] {
            assert!(!logs.contains(secret), "{rust_log:?}: {secret} in {logs}");
        }
    }
}
