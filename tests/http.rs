//! HTTP transport tests: an in-process server on 127.0.0.1:0 built from
//! `pscat_mcp::http::router`, rendering with the real worker binary.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use pscat_mcp::auth::{Authenticator, BearerTokenAuth};
use pscat_mcp::backend::{BackendFuture, RenderBackend, SubprocessBackend};
use pscat_mcp::config::{HttpConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::http::{authenticator, mcp_server_config, router};
use pscat_mcp::render::RenderService;
use pscat_mcp::worker::WorkerJob;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-token-0123456789";
const ACCEPT: &str = "application/json, text/event-stream";

fn worker_exe() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_pscat-mcp-server").into()
}

fn http_config(bind: IpAddr) -> HttpConfig {
    HttpConfig {
        bind,
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

fn real_service(limits: LimitsConfig) -> Arc<RenderService> {
    pscat_mcp::build_service(
        limits,
        &WorkerConfig {
            worker_exe: Some(worker_exe()),
            ..WorkerConfig::default()
        },
    )
    .expect("service")
}

struct TestServer {
    base: String,
    service: Arc<RenderService>,
    ct: CancellationToken,
}

impl TestServer {
    fn mcp(&self) -> String {
        format!("{}/mcp", self.base)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

async fn start(service: Arc<RenderService>) -> TestServer {
    let auth: Arc<dyn Authenticator> = Arc::new(BearerTokenAuth::new([TOKEN]));
    let ct = CancellationToken::new();
    let cfg = mcp_server_config(
        &http_config(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ct.child_token(),
    );
    let app = router(service.clone(), auth, cfg);
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
        service,
        ct,
    }
}

fn initialize_body(version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "legacy-test", "version": "1.0" }
        }
    })
}

fn render_call(id: u64, source: &str, w: u32, h: u32) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "render_postscript",
            "arguments": { "source": source, "width": w, "height": h }
        }
    })
}

/// POST a JSON-RPC message; returns status, content type and the JSON-RPC
/// message (parsed from JSON or from the last SSE `data:` event).
async fn post(
    url: &str,
    token: Option<&str>,
    protocol_version: Option<&str>,
    body: &Value,
) -> (u16, String, Value) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .body(body.to_string());
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(v) = protocol_version {
        req = req.header("MCP-Protocol-Version", v);
    }
    let resp = req.send().await.expect("request");
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = resp.text().await.unwrap_or_default();
    (status, ctype.clone(), parse_body(&ctype, &text))
}

fn parse_body(ctype: &str, text: &str) -> Value {
    if ctype.contains("text/event-stream") {
        // The response is the last event carrying a JSON-RPC result/error.
        text.lines()
            .rev()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v.get("result").is_some() || v.get("error").is_some())
            .unwrap_or(Value::Null)
    } else {
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }
}

/// Decode a base64 PNG to (width, height, RGB triplets).
fn decode_png(b64: &str) -> (u32, u32, Vec<[u8; 3]>) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .expect("PNG buffer size fits in memory")
    ];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let data = &buf[..info.buffer_size()];
    let px: Vec<[u8; 3]> = match info.color_type {
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| [c[0], c[1], c[2]])
            .collect(),
        png::ColorType::Rgba => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| [c[0], c[1], c[2]])
            .collect(),
        png::ColorType::Grayscale => data.iter().map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| [c[0], c[0], c[0]])
            .collect(),
        other => panic!("unexpected PNG color type {other:?}"),
    };
    assert_eq!(px.len(), (info.width * info.height) as usize);
    (info.width, info.height, px)
}

/// The first image block of a tool result (JSON form).
fn image_of(result: &Value) -> Option<&str> {
    result["content"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == "image" && c["mimeType"] == "image/png")
        .and_then(|c| c["data"].as_str())
}

fn error_code(result: &Value) -> Option<&str> {
    result["structuredContent"]["error"]["code"].as_str()
}

fn solid(r: u8, g: u8, b: u8) -> String {
    format!("{r} {g} {b} setrgbcolor -10 -10 80 80 rectfill showpage")
}

// ---------------------------------------------------------------- E: auth

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_requests_never_execute() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let init = initialize_body("2025-06-18");
    let call = render_call(2, &solid(1, 0, 0), 20, 20);

    for token in [None, Some("wrong-token"), Some("")] {
        for body in [&init, &call] {
            let resp = {
                let mut req = reqwest::Client::new()
                    .post(srv.mcp())
                    .header("Content-Type", "application/json")
                    .header("Accept", ACCEPT)
                    .body(body.to_string());
                if let Some(t) = token {
                    req = req.header("Authorization", format!("Bearer {t}"));
                }
                req.send().await.unwrap()
            };
            assert_eq!(resp.status(), 401, "token {token:?}");
            assert_eq!(
                resp.headers()["www-authenticate"],
                r#"Bearer realm="pscat-mcp""#
            );
            let body: Value = resp.json().await.unwrap();
            assert!(body["error"].is_string(), "{body}");
            assert!(!body.to_string().contains("wrong-token"));
        }
    }
    // Basic auth with the right secret is still the wrong scheme.
    let resp = reqwest::Client::new()
        .post(srv.mcp())
        .header("Authorization", format!("Basic {TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .body(call.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
    assert_eq!(srv.service.counters.rejected.load(Ordering::SeqCst), 0);

    // /healthz needs no auth and carries a request id.
    let resp = reqwest::get(format!("{}/healthz", srv.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("x-request-id"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({ "status": "ok" }));
    // A client-supplied request id is echoed.
    let resp = reqwest::Client::new()
        .get(format!("{}/healthz", srv.base))
        .header("x-request-id", "abc-123")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-request-id"], "abc-123");
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn rmcp_client_with_token_renders_png() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.mcp()).auth_header(TOKEN),
    );
    let client = ().serve(transport).await.expect("initialize");
    let info = client.peer_info().expect("server info");
    let info = serde_json::to_value(&*info).unwrap();
    eprintln!("negotiated protocolVersion: {}", info["protocolVersion"]);

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"render_postscript"), "{names:?}");

    let args = json!({ "source": solid(0, 0, 1), "width": 40, "height": 30 });
    let result = client
        .call_tool(
            CallToolRequestParams::new("render_postscript")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool");
    let result = serde_json::to_value(&result).unwrap();
    assert_ne!(result["isError"], true, "{result}");
    let (w, h, px) = decode_png(image_of(&result).expect("image content"));
    assert_eq!((w, h), (40, 30));
    assert!(px.iter().all(|p| *p == [0, 0, 255]));
    client.cancel().await.ok();
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------ legacy clients (raw HTTP)

/// The current protocol (2026-07-28, SEP-2567): no `initialize`, no session;
/// each request carries the protocol version and client info in `_meta`.
#[tokio::test(flavor = "multi_thread")]
async fn current_protocol_sessionless_call_renders() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let mut call = render_call(7, &solid(0, 0, 1), 32, 20);
    call["params"]["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "1.0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    // SEP-2243 standard headers mirror the method and tool name.
    let send = |token: Option<&'static str>| {
        let mut req = reqwest::Client::new()
            .post(srv.mcp())
            .header("Content-Type", "application/json")
            .header("Accept", ACCEPT)
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "render_postscript")
            .body(call.to_string());
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        req.send()
    };
    let r = send(Some(TOKEN)).await.expect("request");
    let status = r.status().as_u16();
    let ctype = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let resp: Value = serde_json::from_str(&r.text().await.expect("body")).expect("json");
    assert_eq!(status, 200, "{resp}");
    assert!(ctype.contains("application/json"), "{ctype}");
    let result = &resp["result"];
    assert_ne!(result["isError"], true, "{resp}");
    assert_eq!(result["structuredContent"]["ok"], true, "{resp}");
    let (w, h, px) = decode_png(image_of(result).expect("image content"));
    assert_eq!((w, h), (32, 20));
    assert!(px.iter().all(|p| *p == [0, 0, 255]));

    // Unauthenticated, the same request never reaches the tool.
    let status = send(None).await.expect("request").status().as_u16();
    assert_eq!(status, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_protocol_clients_work_statelessly() {
    let srv = start(real_service(LimitsConfig::default())).await;
    for version in ["2025-06-18", "2025-03-26"] {
        let (status, ctype, init) =
            post(&srv.mcp(), Some(TOKEN), None, &initialize_body(version)).await;
        assert_eq!(status, 200, "{version}: {init}");
        assert!(ctype.contains("application/json"), "{version}: {ctype}");
        assert_eq!(init["result"]["protocolVersion"], version, "{init}");
        eprintln!(
            "{version}: initialize -> {ctype}, server {}",
            init["result"]["serverInfo"]
        );

        // No session id, no initialized notification: each POST stands alone.
        for header in [Some(version), None] {
            let (status, ctype, resp) = post(
                &srv.mcp(),
                Some(TOKEN),
                header,
                &render_call(2, &solid(0, 1, 0), 24, 16),
            )
            .await;
            assert_eq!(status, 200, "{version} header={header:?}: {resp}");
            eprintln!("{version} header={header:?}: tools/call -> {ctype}");
            let result = &resp["result"];
            assert_ne!(result["isError"], true, "{resp}");
            let (w, h, px) = decode_png(image_of(result).expect("image"));
            assert_eq!((w, h), (24, 16));
            assert!(px.iter().all(|p| *p == [0, 255, 0]));
        }
    }
}

// ------------------------------------------------ guide prompts and resources

/// The rmcp client over HTTP: capabilities, `resources/list` and `read`,
/// `prompts/list` and `get`, and the error codes for unknown names. None of
/// it starts a worker. `tests/guide.rs` covers the content over stdio.
#[tokio::test(flavor = "multi_thread")]
async fn rmcp_client_reads_guide_resources_and_prompts() {
    use rmcp::model::{ErrorCode, GetPromptRequestParams, ReadResourceRequestParams};
    use rmcp::service::ServiceError;

    let srv = start(real_service(LimitsConfig::default())).await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.mcp()).auth_header(TOKEN),
    );
    let client = ().serve(transport).await.expect("initialize");
    let info = client.peer_info().expect("server info");
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());

    let resources = client.list_all_resources().await.expect("resources/list");
    assert_eq!(resources.len(), pscat_mcp::guide::files().len());
    for r in &resources {
        let read = client
            .read_resource(ReadResourceRequestParams::new(r.uri.clone()))
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", r.uri));
        let v = serde_json::to_value(&read).unwrap();
        let text = v["contents"][0]["text"].as_str().expect("text");
        let path = r.uri.strip_prefix("pscat://guide/").unwrap();
        let original = pscat_mcp::guide::files()
            .iter()
            .find(|(p, _)| *p == path)
            .unwrap()
            .1;
        assert!(text.ends_with(original), "{}", r.uri);
        assert_eq!(v["contents"][0]["mimeType"], "text/markdown");
    }
    let prompts = client.list_all_prompts().await.expect("prompts/list");
    assert_eq!(prompts.len(), 3);
    let got = client
        .get_prompt(
            GetPromptRequestParams::new("chart")
                .with_arguments(json!({"kind": "line"}).as_object().unwrap().clone()),
        )
        .await
        .expect("prompts/get");
    let text = &got.messages[0].content.as_text().unwrap().text;
    assert!(
        text.contains("line chart with PSCat's lib/dataviz.ps"),
        "{text}"
    );

    let code = |e: ServiceError| match e {
        ServiceError::McpError(d) => d.code,
        other => panic!("not an MCP error: {other}"),
    };
    let e = client
        .read_resource(ReadResourceRequestParams::new("pscat://guide/missing.md"))
        .await
        .unwrap_err();
    assert_eq!(code(e), ErrorCode::RESOURCE_NOT_FOUND);
    let e = client
        .get_prompt(GetPromptRequestParams::new("missing"))
        .await
        .unwrap_err();
    assert_eq!(code(e), ErrorCode::INVALID_PARAMS);
    client.cancel().await.ok();
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

/// The same over raw stateless HTTP, at 2026-07-28 (sessionless, `_meta`,
/// `Mcp-Method`/`Mcp-Name`) and at 2025-06-18.
#[tokio::test(flavor = "multi_thread")]
async fn guide_requests_work_statelessly_at_every_protocol_version() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "1.0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let requests = [
        ("resources/list", None, json!({})),
        (
            "resources/read",
            Some("pscat://guide/psviz/SKILL.md"),
            json!({"uri": "pscat://guide/psviz/SKILL.md"}),
        ),
        ("prompts/list", None, json!({})),
        (
            "prompts/get",
            Some("diagram"),
            json!({"name": "diagram", "arguments": {"format": "png"}}),
        ),
        (
            "resources/read",
            Some("pscat://guide/none.md"),
            json!({"uri": "pscat://guide/none.md"}),
        ),
    ];
    for (i, (method, name, params)) in requests.iter().enumerate() {
        // 2026-07-28: no initialize; the request carries everything.
        let mut p = params.clone();
        p["_meta"] = meta.clone();
        let body = json!({"jsonrpc": "2.0", "id": i + 10, "method": method, "params": p});
        let mut req = reqwest::Client::new()
            .post(srv.mcp())
            .header("Content-Type", "application/json")
            .header("Accept", ACCEPT)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", *method);
        if let Some(n) = name {
            req = req.header("Mcp-Name", *n);
        }
        let r = req.body(body.to_string()).send().await.expect("request");
        let status = r.status().as_u16();
        let resp: Value = serde_json::from_str(&r.text().await.unwrap()).expect("json");
        assert_eq!(status, 200, "{method}: {resp}");
        // 2025-06-18: the same request on its own, after an initialize.
        let (s, _, init) = post(
            &srv.mcp(),
            Some(TOKEN),
            None,
            &initialize_body("2025-06-18"),
        )
        .await;
        assert_eq!(s, 200, "{init}");
        let legacy = json!({"jsonrpc": "2.0", "id": i + 20, "method": method, "params": params});
        let (s, _, old) = post(&srv.mcp(), Some(TOKEN), Some("2025-06-18"), &legacy).await;
        assert_eq!(s, 200, "{method}: {old}");
        // Resource not found is -32002 before 2026-07-28 and invalid params
        // (-32602) from it on; rmcp maps the code per negotiated version.
        for (resp, not_found) in [(&resp, -32602), (&old, -32002)] {
            if name == &Some("pscat://guide/none.md") {
                assert_eq!(resp["error"]["code"], not_found, "{resp}");
                assert_eq!(resp["error"]["data"]["uri"], "pscat://guide/none.md");
                continue;
            }
            let result = &resp["result"];
            assert!(resp["error"].is_null(), "{method}: {resp}");
            match *method {
                "resources/list" => assert!(result["resources"].as_array().unwrap().len() > 10),
                "resources/read" => assert!(
                    result["contents"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("psviz")
                ),
                "prompts/list" => assert_eq!(result["prompts"].as_array().unwrap().len(), 3),
                _ => assert!(
                    result["messages"][0]["content"]["text"]
                        .as_str()
                        .unwrap()
                        .contains("render_postscript")
                ),
            }
        }
    }
}

// ----------------------------------------------------------- F: concurrency

/// Wraps the real backend, recording how many jobs run at once.
struct CountingBackend {
    inner: SubprocessBackend,
    current: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
    delay: Duration,
}

struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RenderBackend for CountingBackend {
    fn execute(&self, job: WorkerJob, deadline: Duration) -> BackendFuture<'_> {
        Box::pin(async move {
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = InFlight(self.current.clone());
            self.max.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inner.execute(job, deadline).await
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_renders_are_isolated_and_bounded() {
    let limits = LimitsConfig {
        max_concurrent_renders: Some(2),
        ..LimitsConfig::default()
    };
    let max = Arc::new(AtomicUsize::new(0));
    let backend = CountingBackend {
        inner: SubprocessBackend::new(worker_exe(), limits.clone()),
        current: Arc::new(AtomicUsize::new(0)),
        max: max.clone(),
        delay: Duration::from_millis(300),
    };
    let service = Arc::new(RenderService::new(Arc::new(backend), limits));
    let srv = start(service).await;

    let colors: [[u8; 3]; 6] = [
        [0, 0, 0],
        [1, 0, 0],
        [0, 1, 0],
        [0, 0, 1],
        [1, 1, 0],
        [0, 1, 1],
    ];
    let mut tasks = Vec::new();
    for (i, c) in colors.iter().enumerate() {
        let url = srv.mcp();
        let src = solid(c[0], c[1], c[2]);
        let expect = c.map(|v| v * 255);
        tasks.push(tokio::spawn(async move {
            let (status, _, resp) = post(
                &url,
                Some(TOKEN),
                None,
                &render_call(i as u64 + 10, &src, 50, 50),
            )
            .await;
            (Some(expect), status, resp)
        }));
    }
    let url = srv.mcp();
    tasks.push(tokio::spawn(async move {
        let src = "1 0 0 setrgbcolor 0 0 50 50 rectfill nosuchoperator showpage";
        let (status, _, resp) = post(&url, Some(TOKEN), None, &render_call(99, src, 50, 50)).await;
        (None, status, resp)
    }));

    for t in tasks {
        let (expect, status, resp) = t.await.unwrap();
        assert_eq!(status, 200, "{resp}");
        let result = &resp["result"];
        match expect {
            Some(rgb) => {
                assert_ne!(result["isError"], true, "{resp}");
                let (w, h, px) = decode_png(image_of(result).expect("image"));
                assert_eq!((w, h), (50, 50));
                assert!(
                    px.iter().all(|p| *p == rgb),
                    "expected every pixel {rgb:?}, first is {:?}",
                    px[0]
                );
            }
            None => {
                assert_eq!(result["isError"], true, "{resp}");
                assert_eq!(error_code(result), Some("postscript_error"), "{resp}");
                assert!(image_of(result).is_none());
            }
        }
    }
    assert_eq!(
        max.load(Ordering::SeqCst),
        2,
        "in-flight renders must reach but never exceed 2"
    );
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn full_queue_returns_server_busy() {
    let srv = start(real_service(LimitsConfig {
        max_concurrent_renders: Some(1),
        queue_timeout_ms: 100,
        render_timeout_ms: 1500,
        ..LimitsConfig::default()
    }))
    .await;

    let url = srv.mcp();
    let slow = tokio::spawn(async move {
        post(&url, Some(TOKEN), None, &render_call(1, "{ } loop", 20, 20)).await
    });
    // Wait until the slow job holds the only slot.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while srv.service.counters.executed.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "slow job never started"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let (status, _, resp) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &render_call(2, &solid(1, 0, 0), 20, 20),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["result"]["isError"], true, "{resp}");
    assert_eq!(error_code(&resp["result"]), Some("server_busy"), "{resp}");

    let (status, _, slow) = slow.await.unwrap();
    assert_eq!(status, 200);
    let code = error_code(&slow["result"]);
    assert!(
        matches!(code, Some("limit_exceeded") | Some("timeout")),
        "slow job ended with {code:?}: {slow}"
    );
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------ configuration rules

#[test]
fn unauthenticated_mode_requires_loopback() {
    let mut open = http_config(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    open.allow_unauthenticated = true;
    let err = authenticator(&open).err().expect("0.0.0.0 must be refused");
    assert!(err.to_string().contains("loopback"), "{err}");

    // Refused even when tokens are also configured.
    open.auth_tokens = vec!["x".into()];
    assert!(authenticator(&open).is_err());

    let mut local = http_config(IpAddr::V4(Ipv4Addr::LOCALHOST));
    local.allow_unauthenticated = true;
    let auth = authenticator(&local).expect("loopback is allowed");
    assert!(auth.describe().contains("unauthenticated"));

    let v6 = HttpConfig {
        bind: "::1".parse().unwrap(),
        ..local.clone()
    };
    assert!(authenticator(&v6).is_ok());

    // No tokens and no opt-in: refuse to start.
    let none = http_config(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let err = authenticator(&none)
        .err()
        .expect("no tokens must be refused");
    assert!(
        err.to_string().contains("no auth tokens configured"),
        "{err}"
    );

    // Tokens from a file.
    let dir = std::env::temp_dir().join(format!("pscat-mcp-http-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("tokens");
    std::fs::write(&file, "# comment\n\nfile-token\n").unwrap();
    let from_file = HttpConfig {
        auth_token_file: Some(file),
        ..http_config(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    };
    let auth = authenticator(&from_file).expect("file tokens");
    assert!(auth.describe().contains("1 token"), "{}", auth.describe());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_origin_checks_reject_after_auth() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let body = initialize_body("2025-06-18").to_string();
    let send = |host: Option<&'static str>, origin: Option<&'static str>| {
        let mut req = reqwest::Client::new()
            .post(srv.mcp())
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "application/json")
            .header("Accept", ACCEPT)
            .body(body.clone());
        if let Some(h) = host {
            req = req.header("Host", h);
        }
        if let Some(o) = origin {
            req = req.header("Origin", o);
        }
        req.send()
    };
    assert_eq!(send(None, None).await.unwrap().status(), 200);
    assert_eq!(
        send(Some("evil.example"), None).await.unwrap().status(),
        403
    );
    assert_eq!(
        send(Some("localhost:1234"), None).await.unwrap().status(),
        200
    );
    assert_eq!(
        send(None, Some("https://evil.example"))
            .await
            .unwrap()
            .status(),
        403
    );

    // `*` disables the Host check.
    let mut any = http_config(IpAddr::V4(Ipv4Addr::LOCALHOST));
    any.allowed_hosts = vec!["*".into()];
    let cfg = mcp_server_config(&any, CancellationToken::new());
    assert!(cfg.allowed_hosts.is_empty());
    let mut listed = any.clone();
    listed.allowed_hosts = vec!["pscat.example".into()];
    listed.allowed_origins = vec!["https://app.example:443".into()];
    let cfg = mcp_server_config(&listed, CancellationToken::new());
    assert_eq!(cfg.allowed_hosts, vec!["pscat.example".to_string()]);
    assert_eq!(
        cfg.allowed_origins,
        vec!["https://app.example:443".to_string()]
    );
    assert!(!cfg.legacy_session_mode && cfg.json_response);
}

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_lets_slightly_large_sources_reach_the_tool() {
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_handwrite_bytes: 1024,
        ..LimitsConfig::default()
    }))
    .await;
    // Over the source limit but well under the body limit: structured error.
    let src = format!("% {}\nshowpage", "x".repeat(2000));
    let (status, _, resp) =
        post(&srv.mcp(), Some(TOKEN), None, &render_call(1, &src, 10, 10)).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        error_code(&resp["result"]),
        Some("source_too_large"),
        "{resp}"
    );

    // Far over the body limit (1024 * 6 + 64 KiB): 413 before rmcp.
    let huge = format!("% {}\nshowpage", "x".repeat(200 * 1024));
    let (status, _, _) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &render_call(2, &huge, 10, 10),
    )
    .await;
    assert_eq!(status, 413);

    // An unauthenticated huge body is a 401, not a 413: auth runs first.
    let (status, _, _) = post(&srv.mcp(), None, None, &render_call(3, &huge, 10, 10)).await;
    assert_eq!(status, 401);
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

/// The body limit follows the largest text a tool takes, so a `handwrite`
/// text past what the source limit would let through still gets a structured
/// answer, not a 413.
#[tokio::test(flavor = "multi_thread")]
async fn body_limit_follows_the_handwrite_text_limit() {
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_handwrite_bytes: 100 * 1024,
        ..LimitsConfig::default()
    }))
    .await;
    let note = |id: u64, text: String| {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "handwrite", "arguments": { "text": text } }
        })
    };
    // Past the source limit's body limit (1024 * 6 + 64 KiB), under the text
    // limit's: a note that is read and answered (here: too tall).
    let (status, _, resp) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &note(1, "hi ".repeat(30 * 1024)),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        error_code(&resp["result"]),
        Some("invalid_dimensions"),
        "{resp}"
    );
    // Over the text limit, under the body limit: structured.
    let (status, _, resp) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &note(2, "hi ".repeat(40 * 1024)),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        error_code(&resp["result"]),
        Some("source_too_large"),
        "{resp}"
    );
    // Far over the body limit (100 KiB * 6 + 64 KiB): 413 before rmcp.
    let (status, _, _) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &note(3, "hi ".repeat(300 * 1024)),
    )
    .await;
    assert_eq!(status, 413);
}

/// The body limit also follows a `review_sheet`'s sources together, which
/// may be larger than one source.
#[tokio::test(flavor = "multi_thread")]
async fn body_limit_follows_the_sheet_source_limit() {
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_sheet_source_bytes: Some(100 * 1024),
        max_sheet_cells: 200,
        ..LimitsConfig::default()
    }))
    .await;
    let sheet = |id: u64, entries: usize| {
        let entries: Vec<Value> = (0..entries)
            .map(|_| json!({ "source": "%".repeat(1000) }))
            .collect();
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "review_sheet", "arguments": { "entries": entries } }
        })
    };
    // About 90 KB of sources: past the source limit's body limit (1024 * 6 +
    // 64 KiB), within the sheet's. Read and answered: 90 letter-size cells
    // are over the pixel budget, and nothing runs.
    let (status, _, resp) = post(&srv.mcp(), Some(TOKEN), None, &sheet(1, 90)).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        error_code(&resp["result"]),
        Some("too_many_pixels"),
        "{resp}"
    );
    // Over the sheet's source limit: structured.
    let (status, _, resp) = post(&srv.mcp(), Some(TOKEN), None, &sheet(2, 110)).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        error_code(&resp["result"]),
        Some("source_too_large"),
        "{resp}"
    );
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

// ------------------------------------------- cancellation and chunked bodies

/// Processes whose command line contains `tag`.
fn processes_with(tag: &str) -> Vec<String> {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(tag)
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

async fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Stateless HTTP has no session to route `notifications/cancelled` to, so
/// on a deployment the client giving up means closing the connection. That
/// must kill the worker and free the slot (issue #5).
#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_kills_the_worker_and_frees_the_slot() {
    let tag = format!("pscat-mcp-disconnect-{}", uuid::Uuid::new_v4());
    let limits = LimitsConfig {
        max_concurrent_renders: Some(1),
        queue_timeout_ms: 500,
        // Only the disconnect can stop the loop within the test's time frame.
        render_timeout_ms: 60_000,
        interp_time_limit_ms: Some(0),
        max_steps: 0,
        ..LimitsConfig::default()
    };
    let backend = SubprocessBackend::new(worker_exe(), limits.clone()).with_tag(tag.clone());
    let srv = start(Arc::new(RenderService::new(Arc::new(backend), limits))).await;

    let url = srv.mcp();
    let request = tokio::spawn(async move {
        post(&url, Some(TOKEN), None, &render_call(1, "{ } loop", 20, 20)).await
    });
    wait_for("worker started", Duration::from_secs(10), || {
        !processes_with(&tag).is_empty()
    })
    .await;

    // Dropping the task drops its reqwest client and closes the connection.
    request.abort();
    let _ = request.await;

    wait_for(
        "worker killed after disconnect",
        Duration::from_secs(5),
        || processes_with(&tag).is_empty(),
    )
    .await;

    let (status, _, resp) = post(
        &srv.mcp(),
        Some(TOKEN),
        None,
        &render_call(2, &solid(0, 1, 0), 20, 20),
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 2);
}

/// Send `body` as a chunked POST /mcp over a raw socket (no Content-Length)
/// and return the response status code.
async fn post_chunked(base: &str, body: &[u8]) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("http://");
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nAccept: {ACCEPT}\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    s.write_all(head.as_bytes()).await.expect("headers");
    // The server may answer and close before the whole body is sent, so
    // write errors are expected and ignored.
    for chunk in body.chunks(8192) {
        let frame = [format!("{:x}\r\n", chunk.len()).as_bytes(), chunk, b"\r\n"].concat();
        if s.write_all(&frame).await.is_err() {
            break;
        }
    }
    let _ = s.write_all(b"0\r\n\r\n").await;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    text.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in {text:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn chunked_body_over_the_limit_is_413_and_never_renders() {
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_handwrite_bytes: 1024,
        ..LimitsConfig::default()
    }))
    .await;

    // Control: a small chunked request goes through, so the raw client is
    // a valid HTTP client.
    let small = render_call(1, &solid(1, 0, 0), 10, 10).to_string();
    assert_eq!(post_chunked(&srv.base, small.as_bytes()).await, 200);
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 1);

    // Far over the body limit (1024 * 6 + 64 KiB), with no Content-Length.
    let huge = format!("% {}\nshowpage", "x".repeat(200 * 1024));
    let body = render_call(2, &huge, 10, 10).to_string();
    assert_eq!(post_chunked(&srv.base, body.as_bytes()).await, 413);
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 1);
}

/// A client that is still uploading an oversized body gets the 413, not a
/// connection reset: the server drains moderately oversized bodies before
/// answering (issue #20). Repeated because the failure was a race.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_bodies_get_a_clean_413_every_time() {
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_handwrite_bytes: 1024,
        ..LimitsConfig::default()
    }))
    .await;
    // About 3x the body limit, well under the drain cap.
    let huge = format!("% {}\nshowpage", "x".repeat(200 * 1024));
    let body = render_call(1, &huge, 10, 10);
    for i in 0..25 {
        let (status, _, _) = post(&srv.mcp(), Some(TOKEN), None, &body).await;
        assert_eq!(status, 413, "Content-Length request {i}");
        assert_eq!(
            post_chunked(&srv.base, body.to_string().as_bytes()).await,
            413
        );
    }
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

/// A client without a valid token gets the 401, not a connection reset,
/// even when its body is larger than the socket buffers and the body limit:
/// the server reads and discards a rejected body up to the drain cap
/// (issue #30).
#[tokio::test(flavor = "multi_thread")]
async fn rejected_large_bodies_get_a_clean_401_every_time() {
    // Body limit 1024 * 6 + 64 KiB, about 71 KB; the body is 2 MB.
    let srv = start(real_service(LimitsConfig {
        max_source_bytes: 1024,
        max_handwrite_bytes: 1024,
        ..LimitsConfig::default()
    }))
    .await;
    let big = format!("% {}\nshowpage", "x".repeat(2 * 1024 * 1024));
    let body = render_call(1, &big, 10, 10);
    for i in 0..10 {
        let (status, _, _) = post(&srv.mcp(), None, None, &body).await;
        assert_eq!(status, 401, "no token, request {i}");
        let (status, _, _) = post(&srv.mcp(), Some("wrong-token"), None, &body).await;
        assert_eq!(status, 401, "wrong token, request {i}");
    }
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

/// Send only the headers of a POST /mcp declaring a 1 GB body and return the
/// status line, which must arrive without the body being sent.
async fn status_for_huge_declared_length(base: &str, token: Option<&str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("http://");
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth}\
         Content-Type: application/json\r\nAccept: {ACCEPT}\r\n\
         Content-Length: 1000000000\r\n\r\n"
    );
    s.write_all(head.as_bytes()).await.expect("headers");
    // Read until the status line is complete (or EOF).
    let mut response = Vec::new();
    let read_status_line = async {
        let mut chunk = [0u8; 256];
        while !response.windows(2).any(|w| w == b"\r\n") {
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read_status_line)
        .await
        .expect("answered without the body");
    String::from_utf8_lossy(&response).into_owned()
}

/// A declared length beyond the drain cap is refused without reading, with
/// or without a valid token.
#[tokio::test(flavor = "multi_thread")]
async fn huge_declared_length_is_refused_unread() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let text = status_for_huge_declared_length(&srv.base, Some(TOKEN)).await;
    assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    let text = status_for_huge_declared_length(&srv.base, None).await;
    assert!(text.starts_with("HTTP/1.1 401"), "{text}");
}

/// POST `body` with exactly the given `Accept` header values (none: no
/// header) and return the status, content type and raw text.
async fn post_accept(
    srv: &TestServer,
    token: Option<&str>,
    accepts: &[&str],
    body: &Value,
) -> (u16, String, String) {
    let mut req = reqwest::Client::new()
        .post(srv.mcp())
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for a in accepts {
        req = req.header("Accept", *a);
    }
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.expect("request");
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (status, ctype, resp.text().await.unwrap_or_default())
}

/// Issue #17: a client that lists only `application/json` is served, with a
/// JSON body, for `initialize` and for a tool call.
#[tokio::test(flavor = "multi_thread")]
async fn accept_json_only_post_gets_a_json_answer() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let init = initialize_body("2025-06-18");
    let call = render_call(2, &solid(0, 1, 0), 24, 16);
    for accept in ["application/json", "Application/JSON; charset=utf-8"] {
        for (body, what) in [(&init, "initialize"), (&call, "tools/call")] {
            let (status, ctype, text) = post_accept(&srv, Some(TOKEN), &[accept], body).await;
            assert_eq!(status, 200, "{accept} {what}: {text}");
            assert!(
                ctype.contains("application/json"),
                "{accept} {what}: {ctype}"
            );
            let resp: Value = serde_json::from_str(&text).expect("a JSON body");
            assert!(resp.get("result").is_some(), "{accept} {what}: {resp}");
            if what == "tools/call" {
                let result = &resp["result"];
                assert_eq!(result["structuredContent"]["ok"], true, "{resp}");
                let (w, h, px) = decode_png(image_of(result).expect("image content"));
                assert_eq!((w, h), (24, 16));
                assert!(px.iter().all(|p| *p == [0, 255, 0]));
            }
        }
    }
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 2);
}

/// A missing `Accept` and `*/*` / `application/*` mean "anything" (RFC 9110
/// 12.5.1), and the answer is JSON, so they are served.
#[tokio::test(flavor = "multi_thread")]
async fn missing_and_wildcard_accept_are_served_as_json() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let init = initialize_body("2025-06-18");
    let cases: [(&str, &[&str]); 6] = [
        ("none", &[]),
        ("*/*", &["*/*"]),
        ("*/*;q=0.8", &["*/*;q=0.8"]),
        ("application/*", &["application/*"]),
        ("text/html + */*", &["text/html, */*;q=0.1"]),
        ("two headers", &["text/html", "application/json"]),
    ];
    for (name, accepts) in cases {
        let (status, ctype, text) = post_accept(&srv, Some(TOKEN), accepts, &init).await;
        assert_eq!(status, 200, "{name}: {text}");
        assert!(ctype.contains("application/json"), "{name}: {ctype}");
        let resp: Value = serde_json::from_str(&text).expect("a JSON body");
        assert!(resp.get("result").is_some(), "{name}: {resp}");
    }
}

/// An `Accept` that does not allow JSON keeps rmcp's `406`, and the fix-up
/// runs only after authentication, the Host check and the body limit.
#[tokio::test(flavor = "multi_thread")]
async fn accept_that_excludes_json_stays_406_and_auth_comes_first() {
    let srv = start(real_service(LimitsConfig::default())).await;
    let init = initialize_body("2025-06-18");
    let refused: [(&str, &[&str]); 6] = [
        ("text/html", &["text/html"]),
        ("sse only", &["text/event-stream"]),
        ("json q=0", &["application/json;q=0"]),
        (
            "json q=0 + sse",
            &["application/json;q=0, text/event-stream"],
        ),
        ("wildcard q=0", &["*/*;q=0"]),
        ("other wildcard", &["text/*"]),
    ];
    for (name, accepts) in refused {
        let (status, _, text) = post_accept(&srv, Some(TOKEN), accepts, &init).await;
        assert_eq!(status, 406, "{name}: {text}");
        assert!(text.contains("Not Acceptable"), "{name}: {text}");
    }
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);

    // No credentials: 401 for every Accept, never 406 and never a render.
    for accepts in [
        &["application/json"][..],
        &[][..],
        &["*/*"][..],
        &["text/html"][..],
    ] {
        for token in [None, Some("wrong-token")] {
            let (status, _, text) = post_accept(&srv, token, accepts, &init).await;
            assert_eq!(status, 401, "{accepts:?} {token:?}: {text}");
        }
    }

    // Host and Origin checks still apply to a JSON-only client.
    let resp = reqwest::Client::new()
        .post(srv.mcp())
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Host", "evil.example")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // The body limit still answers 413 for a JSON-only client.
    let huge = render_call(3, &"%".repeat(2 * 1024 * 1024), 8, 8);
    let (status, _, _) = post_accept(&srv, Some(TOKEN), &["application/json"], &huge).await;
    assert_eq!(status, 413);
    assert_eq!(srv.service.counters.executed.load(Ordering::SeqCst), 0);
}

/// GET is untouched by the `Accept` fix-up: this stateless server has no
/// event stream to open, so rmcp answers 405 whatever `Accept` says.
#[tokio::test(flavor = "multi_thread")]
async fn get_is_unchanged_by_the_accept_fixup() {
    let srv = start(real_service(LimitsConfig::default())).await;
    for accept in ["application/json", "*/*", "text/event-stream"] {
        let resp = reqwest::Client::new()
            .get(srv.mcp())
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Accept", accept)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 405, "GET with {accept}");
    }
}
