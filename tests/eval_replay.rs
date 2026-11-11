//! Replay of recorded client traffic (issue #40, part 2) and the capture
//! mode that records it. No model and no client binary: the fixtures in
//! `evals/server/fixtures/` hold what real clients sent over HTTP (headers,
//! `initialize` capabilities, `tools/list`, `tools/call`), and this test sends
//! the same requests to the in-process server and checks the replies are 2xx
//! with valid JSON-RPC.
//!
//! Each fixture says how it was obtained (`real-capture` or `synthesized`),
//! from which client and version. A fixture request may carry a
//! `known_failure`: the status the server returns today and the issue that
//! tracks it. The request's `expect` is what it must do once fixed, so the
//! test fails when the failure goes away (flip the fixture then).
//!
//! Recording: run the server with `--capture-file` (see
//! `docs/LOCAL_TESTING.md`, "Server-layer evals") and convert the capture with
//! `evals/server/capture_to_fixture.py`. The ignored test
//! `record_rmcp_client_traffic` produces the capture for the rmcp client.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

use pscat_mcp::auth::{Authenticator, BearerTokenAuth};
use pscat_mcp::capture::Capture;
use pscat_mcp::config::{HttpConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::http::{mcp_server_config, router_with_capture};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "replay-token-0123456789abcdef";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn http_config() -> HttpConfig {
    HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        auth_tokens: vec![],
        auth_token_file: None,
        allow_unauthenticated: false,
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
        allowed_origins: vec![],
        shutdown_grace_secs: 2,
        async_jobs: false,
    }
}

struct TestServer {
    base: String,
    ct: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

async fn start(capture: Option<Arc<Capture>>) -> TestServer {
    let service = pscat_mcp::build_service(
        LimitsConfig::default(),
        &WorkerConfig {
            worker_exe: Some(env!("CARGO_BIN_EXE_pscat-mcp-server").into()),
        },
    )
    .expect("service");
    let auth: Arc<dyn Authenticator> = Arc::new(BearerTokenAuth::new([TOKEN]));
    let ct = CancellationToken::new();
    let cfg = mcp_server_config(&http_config(), ct.child_token());
    let app = router_with_capture(service, auth, cfg, capture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await;
    });
    TestServer {
        base: format!("http://{addr}"),
        ct,
    }
}

// ---------------------------------------------------------------------------
// Fixtures

fn load_fixtures() -> Vec<(String, Value)> {
    let dir = repo().join("evals/server/fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            let v: Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            (p.file_name().unwrap().to_string_lossy().into_owned(), v)
        })
        .collect()
}

/// Every fixture must say where it came from, so nobody mistakes a guess for
/// a capture.
fn check_metadata(file: &str, fx: &Value) {
    let obtained = fx["obtained"].as_str().unwrap_or("");
    assert!(
        ["real-capture", "synthesized"].contains(&obtained),
        "{file}: obtained must be real-capture or synthesized, got {obtained:?}"
    );
    for key in ["id", "how", "captured_on"] {
        assert!(
            fx[key].as_str().is_some_and(|s| !s.is_empty()),
            "{file}: missing {key}"
        );
    }
    assert!(
        fx["client"]["name"].as_str().is_some(),
        "{file}: missing client.name"
    );
    assert!(
        fx["requests"].as_array().is_some_and(|r| !r.is_empty()),
        "{file}: no requests"
    );
    if obtained == "real-capture" {
        assert!(
            fx["client"]["version"].as_str().is_some(),
            "{file}: a real capture names the client version"
        );
    }
    // Tokens are scrubbed: a fixture never holds a credential.
    let text = fx.to_string();
    assert!(
        !text.contains("Bearer ") && !text.to_lowercase().contains("\"authorization\":\""),
        "{file}: looks like it holds a credential"
    );
}

/// The JSON-RPC message in a response body (JSON, or the last SSE `data:`
/// event that carries a result or error).
fn parse_rpc(content_type: &str, text: &str) -> Option<Value> {
    if content_type.contains("text/event-stream") {
        text.lines()
            .rev()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v.get("result").is_some() || v.get("error").is_some())
    } else {
        serde_json::from_str(text).ok()
    }
}

struct Outcome {
    status: u16,
    problems: Vec<String>,
}

/// Send one fixture request and check its `expect` (default: 2xx and valid
/// JSON-RPC, or 202 for a notification).
async fn replay_request(base: &str, req: &Value) -> Outcome {
    let method = req["method"].as_str().unwrap_or("POST");
    let path = req["path"].as_str().unwrap_or("/mcp");
    let client = reqwest::Client::new();
    let url = format!("{base}{path}");
    let mut builder = match method {
        "POST" => client.post(&url),
        "GET" => client.get(&url),
        "DELETE" => client.delete(&url),
        other => panic!("unsupported fixture method {other}"),
    };
    for (name, value) in req["headers"].as_object().into_iter().flatten() {
        // The Host header is the recording server's; the test server's own
        // address goes out instead.
        if name != "host" {
            builder = builder.header(name.as_str(), value.as_str().unwrap_or(""));
        }
    }
    if req["authorization"]["scheme"] == "Bearer" {
        builder = builder.header("Authorization", format!("Bearer {TOKEN}"));
    }
    if let Some(body) = req.get("body")
        && !body.is_null()
    {
        builder = builder.body(match body {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    let resp = builder.send().await.expect("request");
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = resp.text().await.unwrap_or_default();

    let expect = &req["expect"];
    let mut problems = Vec::new();
    // Status.
    if let Some(list) = expect["status_in"].as_array() {
        if !list.iter().any(|s| s.as_u64() == Some(u64::from(status))) {
            problems.push(format!("HTTP {status}, want one of {list:?}"));
        }
    } else if let Some(s) = expect["status"].as_u64() {
        if u64::from(status) != s {
            problems.push(format!("HTTP {status}, want {s}"));
        }
    } else if !(200..300).contains(&status) {
        problems.push(format!(
            "HTTP {status}, want 2xx: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    // JSON-RPC, for a POST request with an id answered with 200.
    let body = req.get("body").cloned().unwrap_or(Value::Null);
    let id = body.get("id").cloned();
    if problems.is_empty() && method == "POST" && (200..300).contains(&status) {
        match (&id, status) {
            (Some(id), _) => match parse_rpc(&ctype, &text) {
                None => problems.push(format!("no JSON-RPC message in the reply: {text:.200}")),
                Some(rpc) => {
                    if rpc["jsonrpc"] != "2.0" {
                        problems.push(format!("reply is not jsonrpc 2.0: {rpc:.200}"));
                    }
                    if &rpc["id"] != id {
                        problems.push(format!("reply id {} != request id {id}", rpc["id"]));
                    }
                    if rpc.get("result").is_some() == rpc.get("error").is_some() {
                        problems.push("reply has both or neither of result and error".into());
                    }
                    let want_tool_error = expect["is_error"].as_bool().unwrap_or(false);
                    if body["method"] == "tools/call"
                        && rpc["result"]["isError"].as_bool().unwrap_or(false) != want_tool_error
                    {
                        problems.push(format!("tool result isError != {want_tool_error}"));
                    }
                    if let Some(needle) = expect["result_contains"].as_str()
                        && !rpc.to_string().contains(needle)
                    {
                        problems.push(format!("reply lacks {needle:?}"));
                    }
                }
            },
            (None, s) if s != 202 && s != 200 => {
                problems.push(format!("notification answered with {s}, want 202"))
            }
            _ => {}
        }
    }
    Outcome { status, problems }
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_recorded_client_traffic() {
    let srv = start(None).await;
    let fixtures = load_fixtures();
    assert!(!fixtures.is_empty(), "no fixtures");
    let mut failures = Vec::new();
    let mut rows = Vec::new();
    for (file, fx) in &fixtures {
        check_metadata(file, fx);
        for (i, req) in fx["requests"].as_array().unwrap().iter().enumerate() {
            let name = req["name"].as_str().unwrap_or("request");
            let out = replay_request(&srv.base, req).await;
            let label = format!("{}#{} {}", fx["id"].as_str().unwrap(), i + 1, name);
            if let Some(kf) = req.get("known_failure") {
                // Known failure: it must still fail, with the recorded status.
                let issue = &kf["issue"];
                if out.problems.is_empty() {
                    failures.push(format!(
                        "{label}: #{issue} appears fixed (HTTP {} now meets the request's expect); \
                         remove known_failure from {file}",
                        out.status
                    ));
                } else if u64::from(out.status) != kf["status"].as_u64().unwrap_or(0) {
                    failures.push(format!(
                        "{label}: known failure #{issue} was HTTP {}, now HTTP {}: {:?}",
                        kf["status"], out.status, out.problems
                    ));
                } else {
                    rows.push(format!(
                        "{label}: HTTP {} (known failure #{issue})",
                        out.status
                    ));
                }
            } else if out.problems.is_empty() {
                rows.push(format!("{label}: HTTP {}", out.status));
            } else {
                failures.push(format!("{label}: {}", out.problems.join("; ")));
            }
        }
    }
    println!("{}", rows.join("\n"));
    assert!(
        failures.is_empty(),
        "replay failed:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Capture mode

fn read_capture(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("capture line {l}: {e}")))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_mode_records_requests_and_never_a_token() {
    let dir = std::env::temp_dir().join(format!("pscat-capture-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture.jsonl");
    let srv = start(Some(Arc::new(Capture::open(&path).unwrap()))).await;

    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "capture-test", "version": "1"}}});
    let c = reqwest::Client::new();
    // A valid request with extra headers a client might carry credentials in,
    // and a credential in the query string.
    let ok = c
        .post(format!("{}/mcp?access_token=QUERY-SECRET", srv.base))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("User-Agent", "capture-test/1.0")
        .header("X-Api-Key", "HEADER-SECRET")
        .header("Cookie", "session=COOKIE-SECRET")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    // A wrong token never reaches the capture layer (and is not recorded).
    let denied = c
        .post(format!("{}/mcp", srv.base))
        .header("Authorization", "Bearer WRONG-TOKEN-SECRET")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    // Only 406 is expected to show the status of a request rmcp refuses.
    let refused = c
        .post(format!("{}/mcp", srv.base))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    let refused_status = refused.status().as_u16();

    let raw = std::fs::read_to_string(&path).unwrap();
    for secret in [
        TOKEN,
        "QUERY-SECRET",
        "HEADER-SECRET",
        "COOKIE-SECRET",
        "WRONG-TOKEN-SECRET",
    ] {
        assert!(
            !raw.contains(secret),
            "{secret} leaked into the capture file:\n{raw}"
        );
    }
    let records = read_capture(&path);
    assert_eq!(records.len(), 2, "authenticated requests only: {raw}");
    let first = &records[0];
    assert_eq!(first["method"], "POST");
    assert_eq!(first["path"], "/mcp");
    assert_eq!(first["query"], true);
    assert_eq!(first["authorization"]["scheme"], "Bearer");
    assert_eq!(
        first["headers"]["accept"],
        "application/json, text/event-stream"
    );
    assert_eq!(first["headers"]["user-agent"], "capture-test/1.0");
    assert_eq!(first["response"]["status"], 200);
    assert_eq!(first["body_truncated"], false);
    let body: Value = serde_json::from_str(first["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["method"], "initialize");
    let names = first["other_header_names"].as_array().unwrap();
    assert!(names.contains(&json!("x-api-key")) && names.contains(&json!("cookie")));
    // The refused request is recorded with the status it got.
    assert_eq!(records[1]["response"]["status"], refused_status);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "capture file mode");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capture_mode_is_refused_off_loopback() {
    let mut http = http_config();
    assert!(pscat_mcp::http::check_capture_bind(&http).is_ok());
    http.bind = "::1".parse().unwrap();
    assert!(pscat_mcp::http::check_capture_bind(&http).is_ok());
    http.bind = "0.0.0.0".parse().unwrap();
    let err = pscat_mcp::http::check_capture_bind(&http).unwrap_err();
    assert!(err.to_string().contains("loopback bind address"), "{err}");

    // End to end: the binary exits with the message and creates no file.
    let dir = std::env::temp_dir().join(format!("pscat-capture-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture.jsonl");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .args([
            "serve-http",
            "--bind",
            "0.0.0.0",
            "--port",
            "0",
            "--capture-file",
        ])
        .arg(&path)
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must not start: {stderr}");
    assert!(
        stderr.contains("--capture-file") && stderr.contains("loopback"),
        "{stderr}"
    );
    assert!(!path.exists(), "the capture file must not be created");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Server binary with `--capture-file`, driven by the rmcp client (the client
/// Codex is built on): checks the CLI flag end to end, and with
/// `PSCAT_EVAL_RECORD_DIR` set keeps the capture there for
/// `capture_to_fixture.py`.
///
/// `cargo test --test eval_replay record_rmcp_client_traffic -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "records a fixture; run on demand"]
async fn record_rmcp_client_traffic() {
    let keep = std::env::var("PSCAT_EVAL_RECORD_DIR")
        .ok()
        .map(PathBuf::from);
    let dir = keep.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("pscat-rec-{}", uuid::Uuid::new_v4().simple()))
    });
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rmcp-client.capture.jsonl");
    let _ = std::fs::remove_file(&path);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .args([
            "serve-http",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--capture-file",
        ])
        .arg(&path)
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // The first log line with an address.
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut addr = String::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        if let Some(i) = line.find("\"addr\":\"") {
            addr = line[i + 8..].split('"').next().unwrap().to_string();
            break;
        }
    }
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    assert!(!addr.is_empty(), "serve-http did not report an address");

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(TOKEN),
    );
    let client = ().serve(transport).await.expect("initialize");
    let _ = client.list_all_tools().await.expect("tools/list");
    for (tool, args) in [
        ("get_renderer_info", json!({})),
        (
            "render_postscript",
            json!({"source": "%!PS\n0 0 1 setrgbcolor 0 0 100 50 rectfill\nshowpage\n", "width": 100, "height": 50}),
        ),
    ] {
        let Value::Object(args) = args else { panic!() };
        client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
            .await
            .expect("tools/call");
    }
    client.cancel().await.ok();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let records = read_capture(&path);
    let methods: Vec<String> = records
        .iter()
        .map(|r| {
            let b: Value =
                serde_json::from_str(r["body"].as_str().unwrap_or("null")).unwrap_or_default();
            format!(
                "{} {}",
                r["method"].as_str().unwrap(),
                b["method"].as_str().unwrap_or("")
            )
        })
        .collect();
    println!("captured: {methods:?} -> {}", path.display());
    assert!(methods.contains(&"POST initialize".to_string()));
    assert!(methods.contains(&"POST tools/list".to_string()));
    assert!(methods.contains(&"POST tools/call".to_string()));
    assert!(!std::fs::read_to_string(&path).unwrap().contains(TOKEN));
    if keep.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
