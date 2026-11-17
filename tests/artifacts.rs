//! Artifact delivery (#139): the `delivery` argument, the memory artifact
//! store and `resources/read` of `pscat://render/<render_id>/<n>`, over an
//! in-process pipe (the stdio path) and over HTTP, with the real worker.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pscat_mcp::artifacts::{Delivery, MemoryArtifactStore};
use pscat_mcp::auth::{Authenticator, BearerTokenAuth};
use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::{ArtifactConfig, HttpConfig, JobsConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::http::{mcp_server_config, router};
use pscat_mcp::mcp::PscatMcp;
use pscat_mcp::render::RenderService;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation,
    ReadResourceRequestParams, ResourceContents,
};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

mod common;

type Client = RunningService<RoleClient, ClientConfig>;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");

fn worker() -> WorkerConfig {
    WorkerConfig {
        worker_exe: Some(env!("CARGO_BIN_EXE_pscat-mcp-server").into()),
        ..WorkerConfig::default()
    }
}

/// A service with async jobs on (as over stdio) and `artifacts`.
fn service_with(limits: LimitsConfig, artifacts: ArtifactConfig) -> Arc<RenderService> {
    pscat_mcp::build_service_with(
        limits,
        JobsConfig::default().enabled(),
        artifacts,
        &worker(),
    )
    .expect("service")
}

fn memory() -> ArtifactConfig {
    ArtifactConfig::default().memory()
}

async fn connect(service: Arc<RenderService>) -> Client {
    connect_with(service, ClientCapabilities::default()).await
}

async fn connect_with(service: Arc<RenderService>, caps: ClientCapabilities) -> Client {
    let (server_io, client_io) = tokio::io::duplex(1 << 22);
    let server = PscatMcp::new(service);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ClientConfig::new(caps, Implementation::from_build_env())
        .serve(client_io)
        .await
        .expect("client initialize")
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let Value::Object(map) = args else {
        unreachable!()
    };
    let r = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    serde_json::to_value(&r).unwrap()
}

/// `resources/read` of `uri`: the decoded bytes and the MIME type, or the
/// error's text.
async fn read(client: &Client, uri: &str) -> Result<(Vec<u8>, String), String> {
    let r = client
        .read_resource(ReadResourceRequestParams::new(uri))
        .await
        .map_err(|e| e.to_string())?;
    let [
        ResourceContents::BlobResourceContents {
            blob, mime_type, ..
        },
    ] = r.contents.as_slice()
    else {
        panic!("one blob: {r:?}");
    };
    Ok((
        b64().decode(blob).unwrap(),
        mime_type.clone().unwrap_or_default(),
    ))
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// A program of `pages` pages, each a different solid grey.
fn pages(n: usize) -> String {
    (0..n)
        .map(|i| format!("{} setgray 0 0 30 20 rectfill showpage ", i as f64 / 10.0))
        .collect()
}

fn blocks_of<'a>(r: &'a Value, kind: &str) -> Vec<&'a Value> {
    r["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == kind)
        .collect()
}

fn text_of(r: &Value) -> String {
    blocks_of(r, "text")
        .iter()
        .map(|b| b["text"].as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A result with what changes from one run to the next taken out: the
/// random `urn:pscat:render:` id and the timings.
fn normalise(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for key in ["elapsed_ms", "wall_ms"] {
                    if m.contains_key(key) {
                        m.insert(key.into(), json!(0));
                    }
                }
                m.values_mut().for_each(walk);
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::String(s) => {
                if let Some(i) = s.find("urn:pscat:render:") {
                    let end = (i + 17 + 36).min(s.len());
                    s.replace_range(i + 17..end, "UUID");
                }
                if let Some(i) = s.find(" in ")
                    && let Some(j) = s[i..].find(" ms")
                {
                    s.replace_range(i + 4..i + j, "N");
                }
            }
            _ => {}
        }
    }
    walk(&mut v);
    v
}

/// The four tools with an argument set each.
fn tool_calls() -> Vec<(&'static str, Value)> {
    vec![
        (
            "render_postscript",
            json!({"source": pages(2), "width": 30, "height": 20}),
        ),
        (
            "render_postscript",
            json!({"source": pages(2), "width": 30, "height": 20, "format": "svg"}),
        ),
        (
            "render_postscript",
            json!({"source": pages(1), "width": 30, "height": 20, "format": "pdf"}),
        ),
        (
            "chalkboard_math",
            json!({"latex": "x^2", "width": 400, "height": 200, "dpi": 36}),
        ),
        ("handwrite", json!({"text": "hi", "width": 120})),
        (
            "review_sheet",
            json!({"entries": [{"source": pages(1), "width": 30, "height": 20}]}),
        ),
    ]
}

fn with_delivery(args: &Value, delivery: &str) -> Value {
    let mut a = args.clone();
    a["delivery"] = json!(delivery);
    a
}

const LINK_KEYS: [&str; 4] = ["render_id", "download_url", "expires_at", "resource_link"];

#[tokio::test(flavor = "multi_thread")]
async fn inline_results_are_unchanged_with_or_without_a_store() {
    let plain = connect(service_with(
        LimitsConfig::default(),
        ArtifactConfig::default(),
    ))
    .await;
    let linked = connect(service_with(LimitsConfig::default(), memory())).await;
    for (tool, args) in tool_calls() {
        let base = normalise(call(&plain, tool, args.clone()).await);
        assert_ne!(base["isError"], true, "{tool}: {base}");
        let text = base.to_string();
        for key in LINK_KEYS {
            assert!(!text.contains(key), "{tool}: {key} in {text}");
        }
        let variants = [
            (&plain, with_delivery(&args, "inline")),
            // `auto` with no store is inline.
            (&plain, with_delivery(&args, "auto")),
            (&linked, args.clone()),
            (&linked, with_delivery(&args, "inline")),
            // Under the 1 MiB threshold.
            (&linked, with_delivery(&args, "auto")),
        ];
        for (i, (client, a)) in variants.into_iter().enumerate() {
            let got = normalise(call(client, tool, a).await);
            assert_eq!(got, base, "{tool} variant {i}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_store_url_and_both_are_refused() {
    let client = connect(service_with(
        LimitsConfig::default(),
        ArtifactConfig::default(),
    ))
    .await;
    for (tool, args) in tool_calls() {
        for d in ["url", "both", "link"] {
            let r = call(&client, tool, with_delivery(&args, d)).await;
            assert_eq!(r["isError"], true, "{tool} {d}: {r}");
            let err = &r["structuredContent"]["error"];
            assert_eq!(err["code"], "invalid_request", "{tool} {d}: {r}");
            assert_eq!(err["input"]["code"], "delivery", "{tool} {d}: {r}");
            if d != "link" {
                let msg = err["message"].as_str().unwrap();
                assert!(msg.contains("retry with delivery: \"inline\""), "{msg}");
                assert!(text_of(&r).contains("retry with delivery: \"inline\""));
            }
            assert!(blocks_of(&r, "image").is_empty());
        }
    }
    // start_render too, before a job is accepted.
    let r = call(
        &client,
        "start_render",
        json!({"source": pages(1), "delivery": "url"}),
    )
    .await;
    assert_eq!(
        r["structuredContent"]["error"]["input"]["code"], "delivery",
        "{r}"
    );
}

/// The links of a `url` or `both` result, checked against its manifest:
/// one `resource_link` per artifact, the same URI as `download_url`, named
/// in the text.
fn links_of(r: &Value, expect: usize) -> (String, Vec<String>) {
    assert_ne!(r["isError"], true, "{r}");
    let s = &r["structuredContent"];
    let id = s["render_id"].as_str().expect("render_id").to_string();
    let artifacts = s["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), expect, "{r}");
    let links = blocks_of(r, "resource_link");
    assert_eq!(links.len(), expect, "{r}");
    let text = text_of(r);
    let mut uris = Vec::new();
    for (i, (a, l)) in artifacts.iter().zip(&links).enumerate() {
        let url = a["download_url"].as_str().unwrap();
        assert_eq!(url, format!("pscat://render/{id}/{}", i + 1));
        assert_eq!(l["uri"], url);
        assert_eq!(l["mimeType"], a["mime_type"]);
        assert_eq!(l["size"], a["bytes"]);
        assert!(a["expires_at"].as_str().unwrap().ends_with('Z'), "{a}");
        assert!(text.contains(url), "{text}");
        uris.push(url.to_string());
    }
    assert!(text.contains(&id), "{text}");
    (id, uris)
}

#[tokio::test(flavor = "multi_thread")]
async fn url_gives_a_manifest_of_links_and_no_bytes() {
    let client = connect(service_with(LimitsConfig::default(), memory())).await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(3), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    let (_, uris) = links_of(&r, 3);
    assert!(blocks_of(&r, "image").is_empty(), "{r}");
    assert!(blocks_of(&r, "resource").is_empty(), "{r}");
    let s = &r["structuredContent"];
    assert_eq!(s["page_count"], 3);
    for (i, a) in s["artifacts"].as_array().unwrap().iter().enumerate() {
        assert_eq!(a["returned_as"], "link");
        assert_eq!(a["page"], i + 1);
        let (bytes, mime) = read(&client, &uris[i]).await.unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes.len() as u64, a["bytes"].as_u64().unwrap());
        assert!(bytes.starts_with(b"\x89PNG"));
    }
    // A link past the last artifact, or to no render, is not found.
    let past = uris[0].replace("/1", "/4");
    assert!(read(&client, &past).await.is_err());
    // Small: a link result costs the context almost nothing.
    assert!(r.to_string().len() < 4096, "{}", r.to_string().len());
}

#[tokio::test(flavor = "multi_thread")]
async fn both_serves_the_same_bytes_inline_and_by_link() {
    let client = connect(service_with(LimitsConfig::default(), memory())).await;
    // PNG: two pages.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(2), "width": 30, "height": 20, "delivery": "both"}),
    )
    .await;
    let (_, uris) = links_of(&r, 2);
    let images = blocks_of(&r, "image");
    assert_eq!(images.len(), 2);
    for (img, uri) in images.iter().zip(&uris) {
        let inline = b64().decode(img["data"].as_str().unwrap()).unwrap();
        assert_eq!(read(&client, uri).await.unwrap().0, inline);
    }
    assert_eq!(
        r["structuredContent"]["artifacts"][0]["returned_as"],
        "image"
    );
    // SVG: text resources inline.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(2), "width": 30, "height": 20, "format": "svg", "delivery": "both"}),
    )
    .await;
    let (_, uris) = links_of(&r, 2);
    let inline = blocks_of(&r, "resource");
    assert_eq!(inline.len(), 2);
    for (res, uri) in inline.iter().zip(&uris) {
        let svg = res["resource"]["text"].as_str().unwrap();
        let (bytes, mime) = read(&client, uri).await.unwrap();
        assert_eq!(mime, "image/svg+xml");
        assert_eq!(bytes, svg.as_bytes());
    }
    // PDF: one document.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(2), "width": 30, "height": 20, "format": "pdf", "delivery": "both"}),
    )
    .await;
    let (_, uris) = links_of(&r, 1);
    let blob = blocks_of(&r, "resource")[0]["resource"]["blob"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        read(&client, &uris[0]).await.unwrap().0,
        b64().decode(blob).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_render_tool_returns_links() {
    let client = connect(service_with(LimitsConfig::default(), memory())).await;
    for (tool, args) in tool_calls() {
        let r = call(&client, tool, with_delivery(&args, "url")).await;
        assert_ne!(r["isError"], true, "{tool}: {r}");
        assert!(blocks_of(&r, "image").is_empty(), "{tool}: {r}");
        let s = &r["structuredContent"];
        let uri = if tool == "review_sheet" {
            assert_eq!(s["returned_as"], "link");
            assert!(s["cells"].is_array(), "diagnostics stay inline: {r}");
            s["download_url"].as_str().unwrap().to_string()
        } else {
            let n = s["artifacts"].as_array().unwrap().len();
            links_of(&r, n).1.remove(0)
        };
        assert_eq!(blocks_of(&r, "resource_link")[0]["uri"], uri.as_str());
        assert!(text_of(&r).contains(&uri), "{tool}");
        let (bytes, _) = read(&client, &uri).await.unwrap();
        assert!(!bytes.is_empty(), "{tool}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_links_only_over_the_threshold_and_diagnostics_stay_inline() {
    let config = ArtifactConfig {
        artifact_threshold_bytes: 10,
        ..memory()
    };
    let client = connect(service_with(LimitsConfig::default(), config)).await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": format!("(hello) print {}", pages(1)), "width": 30, "height": 20, "delivery": "auto"}),
    )
    .await;
    links_of(&r, 1);
    assert_eq!(
        r["structuredContent"]["artifacts"][0]["returned_as"],
        "link"
    );
    assert_eq!(r["structuredContent"]["program_output"], "hello");
    assert!(text_of(&r).contains("hello"));
    // A failed render has no links, only its error.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": "nosuchop", "delivery": "url"}),
    )
    .await;
    assert_eq!(r["isError"], true);
    assert_eq!(r["structuredContent"]["error"]["code"], "postscript_error");
    assert!(blocks_of(&r, "resource_link").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_default_applies_and_an_argument_overrides_it() {
    let config = ArtifactConfig {
        artifact_delivery: Delivery::Url,
        ..memory()
    };
    let client = connect(service_with(LimitsConfig::default(), config)).await;
    let args = json!({"source": pages(1), "width": 30, "height": 20});
    let r = call(&client, "render_postscript", args.clone()).await;
    links_of(&r, 1);
    let r = call(&client, "render_postscript", with_delivery(&args, "inline")).await;
    assert_eq!(blocks_of(&r, "image").len(), 1);
    assert!(r["structuredContent"].get("render_id").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quota_refuses_with_a_clear_code() {
    let limits = LimitsConfig {
        max_output_bytes: 4096,
        ..LimitsConfig::default()
    };
    let config = ArtifactConfig {
        artifact_quota_bytes: 4096 + 1024,
        ..memory()
    };
    let service = service_with(limits, config);
    let client = connect(service.clone()).await;
    let args = json!({"source": pages(1), "width": 30, "height": 20, "delivery": "url"});
    let mut refused = None;
    for _ in 0..40 {
        let r = call(&client, "render_postscript", args.clone()).await;
        if r["isError"] == true {
            refused = Some(r);
            break;
        }
    }
    let r = refused.expect("the quota was never reached");
    let err = &r["structuredContent"]["error"];
    assert_eq!(err["code"], "results_budget_exceeded", "{r}");
    assert_eq!(err["input"]["code"], "artifact_quota_bytes", "{r}");
    assert_eq!(err["input"]["limit_bytes"], 4096 + 1024, "{r}");
    assert!(blocks_of(&r, "resource_link").is_empty());
    // Inline still works at the quota.
    let r = call(&client, "render_postscript", with_delivery(&args, "inline")).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(service.job_store().artifact_charged_bytes("local") <= 4096 + 1024);
}

#[tokio::test(flavor = "multi_thread")]
async fn links_expire_after_the_ttl_and_free_the_quota() {
    let limits = LimitsConfig::default();
    let base = RenderService::new(
        Arc::new(SubprocessBackend::new(
            env!("CARGO_BIN_EXE_pscat-mcp-server").into(),
            limits.clone(),
        )),
        limits,
    );
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = {
        let now = now.clone();
        Arc::new(move || *now.lock().unwrap())
    };
    let store = MemoryArtifactStore::with_clock(
        base.job_store().clone(),
        Duration::from_secs(60),
        1 << 26,
        clock,
    );
    let service = Arc::new(base.with_artifact_store(memory(), Arc::new(store)));
    let client = connect(service.clone()).await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(1), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    let (_, uris) = links_of(&r, 1);
    assert!(service.job_store().artifact_charged_bytes("local") > 0);
    *now.lock().unwrap() += Duration::from_secs(59);
    assert!(read(&client, &uris[0]).await.is_ok());
    *now.lock().unwrap() += Duration::from_secs(1);
    let err = read(&client, &uris[0]).await.unwrap_err();
    assert!(err.contains("no such stored artifact"), "{err}");
    assert!(!err.contains(&uris[0]), "the error echoes the link: {err}");
    assert_eq!(service.job_store().artifact_charged_bytes("local"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_applies_its_delivery_when_the_result_is_fetched() {
    let client = connect(service_with(LimitsConfig::default(), memory())).await;
    let r = call(
        &client,
        "start_render",
        json!({"source": pages(2), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    let id = r["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let s = call(&client, "render_status", json!({"job_id": id})).await;
        if s["structuredContent"]["state"] == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "{s}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = call(&client, "render_result", json!({"job_id": id})).await;
    let (render_id, uris) = links_of(&first, 2);
    assert!(blocks_of(&first, "image").is_empty());
    let again = call(&client, "render_result", json!({"job_id": id})).await;
    assert_eq!(
        links_of(&again, 2).0,
        render_id,
        "fetched twice, stored once"
    );
    assert!(
        read(&client, &uris[1])
            .await
            .unwrap()
            .0
            .starts_with(b"\x89PNG")
    );
    // Inline jobs are untouched.
    let r = call(&client, "start_render", json!({"source": pages(1)})).await;
    assert!(r["structuredContent"].get("render_id").is_none());
}

// ------------------------------------------------------------------- HTTP

const TOKEN_A: &str = "artifact-token-aaaaaaaaaaaa";
const TOKEN_B: &str = "artifact-token-bbbbbbbbbbbb";

async fn start_http(service: Arc<RenderService>) -> (String, CancellationToken) {
    let auth: Arc<dyn Authenticator> = Arc::new(BearerTokenAuth::new([TOKEN_A, TOKEN_B]));
    let ct = CancellationToken::new();
    let http = HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        auth_tokens: vec![],
        auth_token_file: None,
        allow_unauthenticated: false,
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        allowed_origins: vec![],
        shutdown_grace_secs: 2,
        async_jobs: false,
    };
    let app = router(service, auth, mcp_server_config(&http, ct.child_token()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await;
    });
    (format!("http://{addr}/mcp"), ct)
}

async fn rpc(url: &str, token: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .body(body.to_string())
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    serde_json::from_str(&resp.text().await.unwrap()).expect("json")
}

#[tokio::test(flavor = "multi_thread")]
async fn over_http_another_principal_cannot_read_a_render() {
    let service = pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig::default(),
        memory(),
        &worker(),
    )
    .unwrap();
    let (url, _stop) = start_http(service).await;
    let r = rpc(
        &url,
        TOKEN_A,
        "tools/call",
        json!({"name": "render_postscript",
               "arguments": {"source": pages(1), "width": 30, "height": 20, "delivery": "url"}}),
    )
    .await;
    let (id, uris) = links_of(&r["result"], 1);
    let ok = rpc(&url, TOKEN_A, "resources/read", json!({"uri": uris[0]})).await;
    let blob = ok["result"]["contents"][0]["blob"].as_str().expect("blob");
    assert!(b64().decode(blob).unwrap().starts_with(b"\x89PNG"));

    let foreign = rpc(&url, TOKEN_B, "resources/read", json!({"uri": uris[0]})).await;
    let never = uuid::Uuid::new_v4().to_string();
    let unknown = rpc(
        &url,
        TOKEN_B,
        "resources/read",
        json!({"uri": format!("pscat://render/{never}/1")}),
    )
    .await;
    assert!(foreign.get("result").is_none(), "{foreign}");
    assert_eq!(foreign["error"], unknown["error"], "indistinguishable");
    assert!(!foreign.to_string().contains(&id), "{foreign}");
}

// ------------------------------------------------------- the real binary

#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_binary_serves_links_from_its_environment() {
    let client = common::start_stdio(SERVER, &[("PSCAT_MCP_ARTIFACT_LINKS", "memory")]).await;
    let Value::Object(args) =
        json!({"source": pages(2), "width": 30, "height": 20, "delivery": "url"})
    else {
        unreachable!()
    };
    let r = client
        .call_tool(CallToolRequestParams::new("render_postscript").with_arguments(args))
        .await
        .unwrap();
    let r = serde_json::to_value(&r).unwrap();
    let (_, uris) = links_of(&r, 2);
    let read = client
        .read_resource(ReadResourceRequestParams::new(&uris[1]))
        .await
        .unwrap();
    let ResourceContents::BlobResourceContents { blob, .. } = &read.contents[0] else {
        panic!("{read:?}");
    };
    assert!(b64().decode(blob).unwrap().starts_with(b"\x89PNG"));
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    let links = &info.structured_content.unwrap()["artifact_links"];
    assert_eq!(links["backend"], "memory", "{links}");
    assert_eq!(links["ttl_seconds"], 1800, "{links}");
    client.cancel().await.ok();
}

/// Run `serve-stdio` with `env` and no client; return its exit status and
/// stderr. A server that starts waits for input, so it is given none.
fn refused_start(env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = std::process::Command::new(SERVER);
    cmd.arg("serve-stdio")
        .env("PSCAT_MCP_LOG_FORMAT", "pretty")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the server");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn gcs_and_a_default_without_links_refuse_to_start() {
    let (ok, err) = refused_start(&[("PSCAT_MCP_ARTIFACT_LINKS", "gcs")]);
    assert!(!ok, "{err}");
    assert!(
        err.contains("not implemented yet") && err.contains("#140"),
        "{err}"
    );
    let (ok, err) = refused_start(&[("PSCAT_MCP_ARTIFACT_DELIVERY", "url")]);
    assert!(!ok, "{err}");
    assert!(err.contains("PSCAT_MCP_ARTIFACT_LINKS is off"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quick_tasks_call_applies_its_delivery() {
    let service = pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig {
            tasks: true,
            ..JobsConfig::default()
        }
        .enabled(),
        memory(),
        &worker(),
    )
    .unwrap();
    assert!(service.jobs().tasks_enabled());
    let client = connect_with(
        service,
        ClientCapabilities::builder().enable_tasks().build(),
    )
    .await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(1), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    let (_, uris) = links_of(&r, 1);
    assert!(read(&client, &uris[0]).await.is_ok());
    let r = call(
        &client,
        "handwrite",
        json!({"text": "hi", "width": 120, "delivery": "both"}),
    )
    .await;
    links_of(&r, 1);
    assert_eq!(blocks_of(&r, "image").len(), 1);
}
