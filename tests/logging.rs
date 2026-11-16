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
}

async fn start_http(rust_log: Option<&str>) -> Server {
    let mut cmd = tokio::process::Command::new(SERVER);
    cmd.args(["serve-http", "--bind", "127.0.0.1", "--port", "0"])
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env_remove("RUST_LOG")
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
    let logs = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    });
    Server {
        child,
        base: format!("http://{addr}/mcp"),
        logs,
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

#[tokio::test]
async fn http_default_filter_leaves_the_client_info_out() {
    let err = http_stderr(None).await;
    assert!(!err.contains(MARKER), "client string in the logs: {err}");
    assert!(!err.contains("peer_info"), "{err}");
    assert!(!err.contains(LINE), "{err}");
}

#[tokio::test]
async fn http_rust_log_overrides_the_default() {
    let err = http_stderr(Some("info,rmcp=info")).await;
    assert!(err.contains(LINE), "the line is not emitted at all: {err}");
    assert!(err.contains(MARKER), "{err}");
}
