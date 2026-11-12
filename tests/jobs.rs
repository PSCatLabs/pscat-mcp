//! Async render jobs (issue #33): `start_render`, `render_status`,
//! `render_result`, `cancel_render`, and the MCP Tasks extension, with the
//! real worker. Deadlines are short and explicit (a 1 s sync deadline,
//! jobs of a few seconds) and each test sets its own render slots, so these
//! stay fast and add no load to `tests/backend.rs`.
//!
//! The slow program waits on `realtime`, so it takes the same wall time on
//! any machine: longer than the sync deadline, far shorter than the job's.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pscat_mcp::auth::{Authenticator, BearerTokenAuth};
use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::{HttpConfig, JobsConfig, LimitsConfig};
use pscat_mcp::http::{mcp_server_config, router};
use pscat_mcp::mcp::{PscatMcp, Transport};
use pscat_mcp::render::RenderService;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
    ClientCapabilities, ClientConfig, ClientRequest, GetTaskParams, Implementation,
    ProtocolVersion, ResourceContents, TaskPayload, TaskStatus,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, PeerRequestOptions, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

mod common;

const SYNC_TIMEOUT_MS: u64 = 1_000;
const JOB_TIMEOUT_MS: u64 = 15_000;
/// Waits 2 s of wall time, then fills the page red.
const SLOW_RED: &str = "realtime /t0 exch def { realtime t0 sub 2000 gt { exit } if } loop \
                        1 0 0 setrgbcolor -10 -10 100 100 rectfill showpage";
/// How long a finished job's result is kept in the expiry test: the time
/// the first fetch has, and so (at most) how long the test takes.
const RESULT_TTL_MS: u64 = 5_000;
const QUICK_BLUE: &str = "0 0 1 setrgbcolor -10 -10 100 100 rectfill showpage";
/// Never ends: only a kill stops it (steps and interpreter time are off).
const FOREVER: &str = "{ } loop";

/// PSCat's own time limit is off, so a program past the deadline is always
/// `timeout` (the kill), whatever the load: with it on, a worker slow to
/// start could see either `timeout` or `limit_exceeded`.
fn limits() -> LimitsConfig {
    LimitsConfig {
        render_timeout_ms: SYNC_TIMEOUT_MS,
        interp_time_limit_ms: Some(0),
        max_concurrent_renders: Some(2),
        queue_timeout_ms: 2_000,
        ..LimitsConfig::default()
    }
}

/// For tests whose synchronous calls must succeed: a deadline no load can
/// reach.
fn relaxed() -> LimitsConfig {
    LimitsConfig {
        render_timeout_ms: 20_000,
        queue_timeout_ms: 10_000,
        ..limits()
    }
}

fn unlimited() -> LimitsConfig {
    LimitsConfig {
        max_steps: 0,
        ..limits()
    }
}

fn jobs() -> JobsConfig {
    JobsConfig {
        job_timeout_ms: JOB_TIMEOUT_MS,
        ..JobsConfig::default()
    }
    .enabled()
}

/// A service on the real worker; every worker gets `--tag <tag>`.
fn service(limits: LimitsConfig, jobs: JobsConfig, tag: &str) -> Arc<RenderService> {
    jobs.check(&limits).expect("job settings");
    let backend = SubprocessBackend::new(
        env!("CARGO_BIN_EXE_pscat-mcp-server").into(),
        limits.clone(),
    )
    .with_tag(tag.to_string());
    Arc::new(RenderService::new(Arc::new(backend), limits).with_jobs(jobs))
}

fn tag() -> String {
    format!("pscat-mcp-jobs-{}", uuid::Uuid::new_v4())
}

type Client = RunningService<RoleClient, ClientConfig>;

/// An in-process server over a pipe (the stdio path), and a client that
/// declares the Tasks extension or not.
async fn connect(service: Arc<RenderService>, tasks: bool) -> Client {
    let (server_io, client_io) = tokio::io::duplex(1 << 22);
    let server = PscatMcp::new(service);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let caps = if tasks {
        ClientCapabilities::builder().enable_tasks().build()
    } else {
        ClientCapabilities::default()
    };
    ClientConfig::new(caps, Implementation::from_build_env())
        .serve(client_io)
        .await
        .expect("client initialize")
}

fn params(tool: &str, args: Value) -> CallToolRequestParams {
    let Value::Object(map) = args else {
        unreachable!()
    };
    CallToolRequestParams::new(tool.to_string()).with_arguments(map)
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let r = client
        .call_tool(params(tool, args))
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    serde_json::to_value(&r).unwrap()
}

fn structured(r: &Value) -> &Value {
    &r["structuredContent"]
}

fn error_code(r: &Value) -> &str {
    r["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or("")
}

fn state(r: &Value) -> &str {
    r["structuredContent"]["state"].as_str().unwrap_or("")
}

/// The one PNG in a result: its size and every pixel.
fn png_of(r: &Value) -> (u32, u32, Vec<[u8; 4]>) {
    let data = r["content"]
        .as_array()
        .and_then(|c| c.iter().find(|b| b["type"] == "image"))
        .and_then(|b| b["data"].as_str())
        .unwrap_or_else(|| panic!("no image in {r}"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let px = buf[..info.buffer_size()]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    (info.width, info.height, px)
}

fn processes_with(tag: &str) -> usize {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(tag)
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&out.stdout).lines().count()
}

async fn wait_for(what: &str, timeout: Duration, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond().await {
        assert!(Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll `render_status` until the job finishes; returns every state seen,
/// in order, without repeats, and the last status.
async fn poll_until_finished(client: &Client, id: &str) -> (Vec<String>, Value) {
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = call(client, "render_status", json!({ "job_id": id })).await;
        assert_eq!(structured(&r)["ok"], true, "{r}");
        let s = state(&r).to_string();
        if seen.last() != Some(&s) {
            seen.push(s.clone());
        }
        if matches!(s.as_str(), "done" | "failed" | "cancelled") {
            return (seen, r);
        }
        assert!(Instant::now() < deadline, "job never finished: {seen:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn start_job(client: &Client, source: &str) -> String {
    let r = call(
        client,
        "start_render",
        json!({ "source": source, "width": 20, "height": 10 }),
    )
    .await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(structured(&r)["ok"], true, "{r}");
    assert_eq!(state(&r), "queued", "{r}");
    let id = structured(&r)["job_id"].as_str().unwrap().to_string();
    // A random UUID: nothing to guess, no token material.
    assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_outlives_the_sync_deadline_and_its_result_is_fetched() {
    let tag = tag();
    let svc = service(
        limits(),
        JobsConfig {
            max_running_jobs: Some(1),
            ..jobs()
        },
        &tag,
    );
    let client = connect(svc.clone(), false).await;

    // Synchronously, the same program passes the deadline.
    let sync = call(
        &client,
        "render_postscript",
        json!({ "source": SLOW_RED, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&sync), "timeout", "{sync}");

    let first = start_job(&client, SLOW_RED).await;
    // One job runs at a time, so the second waits queued behind the first.
    let second = start_job(&client, QUICK_BLUE).await;
    wait_for("the first job runs", Duration::from_secs(10), async || {
        state(&call(&client, "render_status", json!({ "job_id": first })).await) == "running"
    })
    .await;
    let r = call(&client, "render_status", json!({ "job_id": second })).await;
    assert_eq!(state(&r), "queued", "{r}");
    // Too early for a result.
    let early = call(&client, "render_result", json!({ "job_id": first })).await;
    assert_eq!(early["isError"], true);
    assert_eq!(error_code(&early), "job_not_finished", "{early}");

    let (seen, last) = poll_until_finished(&client, &first).await;
    assert_eq!(seen, ["running", "done"], "{last}");
    let status = structured(&last);
    assert!(status["elapsed_seconds"].as_f64().unwrap() >= 1.9, "{last}");
    assert_eq!(status["timeout_seconds"], 15.0);
    assert!(status["expires_in_seconds"].as_f64().unwrap() > 3000.0);
    // render_status stays small.
    assert!(last.to_string().len() < 2048, "{last}");

    let result = call(&client, "render_result", json!({ "job_id": first })).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(structured(&result)["ok"], true);
    assert_eq!(structured(&result)["format"], "png");
    let (w, h, px) = png_of(&result);
    assert_eq!((w, h), (20, 10));
    assert!(px.iter().all(|p| *p == [255, 0, 0, 255]), "not all red");
    // It can be fetched again until it expires.
    let again = call(&client, "render_result", json!({ "job_id": first })).await;
    assert_eq!(png_of(&again).2, px);

    let (seen, _) = poll_until_finished(&client, &second).await;
    assert_eq!(seen.last().map(String::as_str), Some("done"));
    let blue = call(&client, "render_result", json!({ "job_id": second })).await;
    assert!(png_of(&blue).2.iter().all(|p| *p == [0, 0, 255, 255]));

    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 2);
    // The sync render and the two jobs.
    assert_eq!(svc.counters.executed.load(Ordering::SeqCst), 3);
    assert_eq!(processes_with(&tag), 0);
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_job_keeps_the_render_error() {
    let svc = service(limits(), jobs(), &tag());
    let client = connect(svc, false).await;
    let id = start_job(&client, "1 0 div").await;
    let (_, last) = poll_until_finished(&client, &id).await;
    assert_eq!(state(&last), "failed", "{last}");
    assert_eq!(
        structured(&last)["job_error"]["code"],
        "postscript_error",
        "{last}"
    );
    let r = call(&client, "render_result", json!({ "job_id": id })).await;
    assert_eq!(r["isError"], true);
    assert_eq!(error_code(&r), "postscript_error", "{r}");
    assert_eq!(
        structured(&r)["error"]["postscript"]["name"],
        "undefinedresult"
    );
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_job_kills_its_worker() {
    let tag = tag();
    let svc = service(unlimited(), jobs(), &tag);
    let client = connect(svc.clone(), false).await;
    let id = start_job(&client, FOREVER).await;
    wait_for("the worker starts", Duration::from_secs(10), async || {
        processes_with(&tag) == 1
    })
    .await;
    let r = call(&client, "render_status", json!({ "job_id": id })).await;
    assert_eq!(state(&r), "running", "{r}");

    let r = call(&client, "cancel_render", json!({ "job_id": id })).await;
    assert_eq!(structured(&r)["ok"], true, "{r}");
    assert_eq!(state(&r), "cancelled", "{r}");
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    assert_eq!(svc.counters.jobs_cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(svc.jobs().unfinished(), 0);

    let r = call(&client, "render_result", json!({ "job_id": id })).await;
    assert_eq!(error_code(&r), "cancelled", "{r}");
    // Cancelling again leaves it as it is.
    let r = call(&client, "cancel_render", json!({ "job_id": id })).await;
    assert_eq!(state(&r), "cancelled", "{r}");
    // A queued job is cancelled before it starts: no worker at all.
    let svc1 = service(
        unlimited(),
        JobsConfig {
            max_running_jobs: Some(1),
            ..jobs()
        },
        &tag,
    );
    let client1 = connect(svc1.clone(), false).await;
    let running = start_job(&client1, FOREVER).await;
    let queued = start_job(&client1, FOREVER).await;
    let r = call(&client1, "cancel_render", json!({ "job_id": queued })).await;
    assert_eq!(state(&r), "cancelled", "{r}");
    call(&client1, "cancel_render", json!({ "job_id": running })).await;
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    assert!(svc1.counters.executed.load(Ordering::SeqCst) <= 1);
    client.cancel().await.ok();
    client1.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_deadline_is_killed_and_fails_with_timeout() {
    let tag = tag();
    let svc = service(
        unlimited(),
        JobsConfig {
            job_timeout_ms: 1_000,
            ..jobs()
        },
        &tag,
    );
    let client = connect(svc.clone(), false).await;
    let id = start_job(&client, FOREVER).await;
    let (seen, last) = poll_until_finished(&client, &id).await;
    assert_eq!(seen.last().map(String::as_str), Some("failed"), "{last}");
    assert_eq!(structured(&last)["job_error"]["code"], "timeout", "{last}");
    assert_eq!(processes_with(&tag), 0);
    assert_eq!(svc.counters.timed_out.load(Ordering::SeqCst), 1);
    client.cancel().await.ok();
}

/// The TTL runs from the moment the job finishes, whether or not its result
/// was fetched (#86 changed which jobs a budget may evict early, not when
/// they expire). The TTL only has to outlast the first fetch, so it is long
/// enough for a loaded machine; the test then polls until the job is gone
/// instead of sleeping past a guess.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_result_is_gone() {
    let svc = service(
        limits(),
        JobsConfig {
            job_result_ttl_ms: RESULT_TTL_MS,
            ..jobs()
        },
        &tag(),
    );
    let client = connect(svc.clone(), false).await;
    let id = start_job(&client, QUICK_BLUE).await;
    let (_, status) = poll_until_finished(&client, &id).await;
    let left = structured(&status)["expires_in_seconds"]
        .as_f64()
        .expect("a finished job reports when it expires");
    assert!(
        left > 0.0,
        "the result expired before it could be fetched; raise RESULT_TTL_MS: {status}"
    );
    let r = call(&client, "render_result", json!({ "job_id": id })).await;
    assert_ne!(
        r["isError"], true,
        "fetched after {RESULT_TTL_MS} ms ttl: {r}"
    );

    // Poll for the expiry; the deadline is far past the TTL.
    let deadline = Instant::now() + Duration::from_millis(RESULT_TTL_MS) + Duration::from_secs(60);
    loop {
        let r = call(&client, "render_status", json!({ "job_id": id })).await;
        if r["isError"] == true {
            assert_eq!(error_code(&r), "unknown_job", "{r}");
            break;
        }
        assert!(Instant::now() < deadline, "the result never expired: {r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for tool in ["render_status", "render_result", "cancel_render"] {
        let r = call(&client, tool, json!({ "job_id": id })).await;
        assert_eq!(r["isError"], true, "{tool}: {r}");
        assert_eq!(error_code(&r), "unknown_job", "{tool}: {r}");
    }
    assert_eq!(svc.jobs().charged_bytes(), 0);
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn job_caps_and_the_results_budget_are_enforced() {
    let tag = tag();
    let svc = service(
        unlimited(),
        JobsConfig {
            max_jobs_per_principal: 2,
            max_jobs: 3,
            ..jobs()
        },
        &tag,
    );
    let client = connect(svc.clone(), false).await;
    let a = start_job(&client, FOREVER).await;
    let b = start_job(&client, FOREVER).await;
    let r = call(
        &client,
        "start_render",
        json!({ "source": FOREVER, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(r["isError"], true);
    assert_eq!(structured(&r)["ok"], false);
    assert_eq!(error_code(&r), "server_busy", "{r}");
    assert!(
        structured(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("per client"),
        "{r}"
    );
    call(&client, "cancel_render", json!({ "job_id": a })).await;
    call(&client, "cancel_render", json!({ "job_id": b })).await;
    // A request refused before a job exists starts nothing.
    let executed = svc.counters.executed.load(Ordering::SeqCst);
    let r = call(
        &client,
        "start_render",
        json!({ "source": "showpage", "format": "jpeg" }),
    )
    .await;
    assert_eq!(error_code(&r), "unsupported_format", "{r}");
    assert_eq!(svc.counters.executed.load(Ordering::SeqCst), executed);
    client.cancel().await.ok();

    // The results budget: room for one largest result and two small ones.
    let small = LimitsConfig {
        max_output_bytes: 30_000,
        ..unlimited()
    };
    let largest = JobsConfig::max_result_bytes(&small);
    let svc = service(
        small,
        JobsConfig {
            job_results_budget_bytes: largest + 2 * pscat_mcp::config::JOB_MIN_CHARGE_BYTES,
            ..jobs()
        },
        &tag,
    );
    let client = connect(svc.clone(), false).await;
    let running = start_job(&client, FOREVER).await;
    // A running job reserves its largest result: no room for another.
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&r), "results_budget_exceeded", "{r}");
    // Cancelled, it keeps only its bookkeeping, which room is made from:
    // the next job starts, and the third drops the cancelled one.
    call(&client, "cancel_render", json!({ "job_id": running })).await;
    let done = start_job(&client, QUICK_BLUE).await;
    poll_until_finished(&client, &done).await;
    let r = call(&client, "render_status", json!({ "job_id": running })).await;
    assert_eq!(state(&r), "cancelled", "kept until room is needed: {r}");
    let third = start_job(&client, QUICK_BLUE).await;
    poll_until_finished(&client, &third).await;
    let r = call(&client, "render_status", json!({ "job_id": running })).await;
    assert_eq!(error_code(&r), "unknown_job", "{r}");
    // Finished results that were never fetched are kept.
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&r), "results_budget_exceeded", "{r}");
    // Once fetched, it makes room.
    call(&client, "render_result", json!({ "job_id": done })).await;
    start_job(&client, QUICK_BLUE).await;
    let r = call(&client, "render_status", json!({ "job_id": done })).await;
    assert_eq!(error_code(&r), "unknown_job", "{r}");
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_principal_over_its_share_of_the_budget_is_told_which_limit() {
    let small = LimitsConfig {
        max_output_bytes: 30_000,
        ..unlimited()
    };
    let largest = JobsConfig::max_result_bytes(&small);
    // Room for three reservations, one reservation and a little
    // bookkeeping (a cancelled job keeps some) per principal.
    let min = pscat_mcp::config::JOB_MIN_CHARGE_BYTES;
    let share = largest + 2 * min;
    let svc = service(
        small,
        JobsConfig {
            job_results_budget_bytes: 3 * share,
            job_results_per_principal_bytes: Some(share),
            ..jobs()
        },
        &tag(),
    );
    assert_eq!(svc.jobs().principal_share_bytes(), share);
    let client = connect(svc.clone(), false).await;
    let info = call(&client, "get_renderer_info", json!({})).await;
    assert_eq!(
        structured(&info)["jobs"]["results_per_principal_bytes"],
        share,
        "{info}"
    );
    let running = start_job(&client, FOREVER).await;
    // Budget is left (two more reservations), but not for this principal.
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(r["isError"], true, "{r}");
    assert_eq!(error_code(&r), "results_budget_exceeded", "{r}");
    let input = &structured(&r)["error"]["input"];
    assert_eq!(input["code"], "results_per_principal_bytes", "{r}");
    assert_eq!(input["limit_bytes"], share, "{r}");
    assert_eq!(input["reached_bytes"], 2 * largest, "{r}");
    assert!(
        structured(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("per client"),
        "{r}"
    );
    // Cancelling frees the reservation but for its bookkeeping, and the
    // principal can start again.
    call(&client, "cancel_render", json!({ "job_id": running })).await;
    let done = start_job(&client, QUICK_BLUE).await;
    poll_until_finished(&client, &done).await;
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn argument_errors_get_the_envelope() {
    let svc = service(limits(), jobs(), &tag());
    let client = connect(svc, false).await;
    for (tool, args) in [
        ("start_render", json!({ "width": 10 })),
        ("start_render", json!({ "source": "showpage", "width": -1 })),
        ("render_status", json!({})),
        ("render_result", json!({ "job_id": 7 })),
        ("cancel_render", json!({ "job_id": [] })),
    ] {
        let r = call(&client, tool, args).await;
        assert_eq!(r["isError"], true, "{tool}: {r}");
        assert_eq!(structured(&r)["ok"], false, "{tool}: {r}");
        assert_eq!(error_code(&r), "invalid_request", "{tool}: {r}");
    }
    let r = call(&client, "render_status", json!({ "job_id": "nope" })).await;
    assert_eq!(error_code(&r), "unknown_job", "{r}");
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_kills_running_jobs() {
    let tag = tag();
    let svc = service(unlimited(), jobs(), &tag);
    let client = connect(svc.clone(), false).await;
    start_job(&client, FOREVER).await;
    wait_for("the worker starts", Duration::from_secs(10), async || {
        processes_with(&tag) == 1
    })
    .await;
    svc.shutdown_jobs(Duration::from_secs(2)).await;
    assert_eq!(svc.jobs().unfinished(), 0);
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&r), "server_busy", "{r}");
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_handler_without_a_principal_owns_nothing() {
    // An HTTP handler reached without the auth layer (no request parts, so
    // no principal) is refused rather than given the local identity.
    let svc = service(limits(), jobs(), &tag());
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    let server = PscatMcp::with_transport(svc.clone(), Transport::Http);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::from_build_env(),
    )
    .serve(client_io)
    .await
    .unwrap();
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&r), "invalid_request", "{r}");
    assert!(
        structured(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no authenticated client"),
        "{r}"
    );
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 0);
    let r = call(&client, "render_status", json!({ "job_id": "x" })).await;
    assert_eq!(error_code(&r), "invalid_request", "{r}");
    client.cancel().await.ok();
}

/// The real `serve-stdio` binary, started through a symlink whose path
/// carries a unique tag: the server and every worker it starts (the
/// worker executable is the same link) can be found with `pgrep -f`.
#[tokio::test(flavor = "multi_thread")]
async fn closing_stdin_kills_a_running_job_before_the_server_exits() {
    let tag = tag();
    let dir = std::env::temp_dir().join(&tag);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("pscat-mcp-server");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_pscat-mcp-server"), &exe).unwrap();
    let mut child = tokio::process::Command::new(&exe)
        .arg("serve-stdio")
        .env("PSCAT_MCP_WORKER_EXE", &exe)
        .env("PSCAT_MCP_MAX_STEPS", "0")
        .env("PSCAT_MCP_INTERP_TIME_LIMIT_MS", "0")
        .env("PSCAT_MCP_JOB_TIMEOUT_MS", "60000")
        .env("PSCAT_MCP_LOG_FORMAT", "pretty")
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let client = ().serve((stdout, stdin)).await.expect("initialize");
    let workers = |tag: &str| {
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("{tag}/pscat-mcp-server render-worker"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    };
    let r = client
        .call_tool(params(
            "start_render",
            json!({ "source": FOREVER, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    wait_for(
        "the job's worker runs",
        Duration::from_secs(10),
        async || workers(&tag) == 1,
    )
    .await;

    // Close the server's stdin: the session ends, the job is killed, and
    // only then does the server exit.
    drop(client);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the server exits")
        .unwrap();
    assert!(status.success(), "{status}");
    // The worker was SIGKILLed before the server exited; give the kernel a
    // moment to reap it.
    wait_for("no worker left", Duration::from_secs(2), async || {
        workers(&tag) == 0
    })
    .await;
    assert_eq!(processes_with(&tag), 0, "nothing tagged survives");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- the tool list

#[tokio::test(flavor = "multi_thread")]
async fn job_tools_are_listed_only_when_jobs_are_on() {
    let job_tools = [
        "cancel_render",
        "render_result",
        "render_status",
        "start_render",
    ];
    for (jobs, listed) in [(jobs(), true), (JobsConfig::default(), false)] {
        let svc = service(limits(), jobs, &tag());
        let client = connect(svc, false).await;
        let tools = client.list_all_tools().await.unwrap();
        for name in job_tools {
            assert_eq!(
                tools.iter().any(|t| t.name == name),
                listed,
                "{name} listed: {listed}"
            );
        }
        let info = call(&client, "get_renderer_info", json!({})).await;
        assert_eq!(structured(&info)["jobs"]["enabled"], listed, "{info}");
        let render = tools
            .iter()
            .find(|t| t.name == "render_postscript")
            .unwrap();
        assert_eq!(
            render
                .description
                .as_deref()
                .unwrap()
                .contains("start_render"),
            listed
        );
        // The guide's note follows the router: the job tools are missing
        // from it exactly when they are not listed.
        let read: rmcp::model::ReadResourceRequestParams =
            serde_json::from_value(json!({ "uri": "pscat://guide/pscat/SKILL.md" })).unwrap();
        let doc = serde_json::to_value(client.read_resource(read).await.unwrap()).unwrap();
        let text = doc["contents"][0]["text"].as_str().unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("The tools are"))
            .expect("tools line");
        let missing = line.split("The plugin's").nth(1).unwrap_or("");
        for name in job_tools {
            assert_eq!(
                missing.contains(&format!("`{name}`")),
                !listed,
                "{name}: {line}"
            );
        }
        assert_eq!(text.contains("a `start_render` job"), listed);
        // No Tasks extension unless asked for.
        let caps = serde_json::to_value(&client.peer_info().unwrap().capabilities).unwrap();
        assert!(caps.get("extensions").is_none(), "{caps}");
        client.cancel().await.ok();
    }
}

// ------------------------------------------------------- the Tasks extension

/// The Tasks extension on, with calls becoming tasks after `after_ms`. A
/// Tasks call starts under the job limits, so the job deadline is at least
/// every sync deadline here (`relaxed()`'s 20 s).
fn tasks_after(after_ms: u64) -> JobsConfig {
    JobsConfig {
        tasks: true,
        task_after_ms: after_ms,
        job_timeout_ms: 30_000,
        ..jobs()
    }
}

/// A threshold far below `SLOW_RED`'s 2 s and the 1 s sync deadline: a slow
/// call always becomes a task.
const SHORT_THRESHOLD_MS: u64 = 200;
/// A threshold no quick call reaches under load (below `relaxed()`'s sync
/// deadline).
const LONG_THRESHOLD_MS: u64 = 15_000;

fn tasks_on() -> JobsConfig {
    tasks_after(SHORT_THRESHOLD_MS)
}

/// Poll `tasks/get` until the task is terminal.
async fn poll_task(client: &Client, id: &str) -> rmcp::model::DetailedTask {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = client
            .peer()
            .get_task(GetTaskParams::new(id.to_string()))
            .await
            .unwrap();
        if got.task.status().is_terminal() {
            return got.task;
        }
        assert!(Instant::now() < deadline, "the task never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tasks_client_gets_a_task_and_a_plain_client_a_result() {
    let tag = tag();
    let svc = service(relaxed(), tasks_on(), &tag);
    let tasks = connect(svc.clone(), true).await;
    let caps = serde_json::to_value(&tasks.peer_info().unwrap().capabilities).unwrap();
    assert!(
        caps["extensions"]["io.modelcontextprotocol/tasks"].is_object(),
        "{caps}"
    );
    // Prompts and resources (#55) are still advertised.
    assert!(caps["prompts"].is_object(), "{caps}");
    assert!(caps["resources"].is_object(), "{caps}");
    assert!(caps["tools"].is_object(), "{caps}");

    // A program past the task threshold completes as a task.
    let response = tasks
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": SLOW_RED, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task, got {response:?}")
    };
    let id = created.task.task_id.clone();
    assert_eq!(created.task.status, TaskStatus::Working);
    assert!(uuid::Uuid::parse_str(&id).is_ok());
    assert_eq!(created.task.poll_interval_ms, Some(1000));
    let finished = poll_task(&tasks, &id).await;
    let TaskPayload::Completed { result } = finished.payload else {
        panic!("expected completed, got {finished:?}")
    };
    let result: CallToolResult = serde_json::from_value(Value::Object(result)).unwrap();
    let result = serde_json::to_value(&result).unwrap();
    assert_ne!(result["isError"], true, "{result}");
    let (w, h, px) = png_of(&result);
    assert_eq!((w, h), (20, 10));
    assert!(px.iter().all(|p| *p == [255, 0, 0, 255]));
    // The same job is visible to the explicit tools.
    let status = call(&tasks, "render_status", json!({ "job_id": id })).await;
    assert_eq!(state(&status), "done", "{status}");

    // Refused before a job exists: a synchronous error, not a task.
    let response = tasks
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": "showpage", "format": "jpeg" }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Complete(r) = response else {
        panic!("expected a complete result")
    };
    assert_eq!(r.is_error, Some(true));

    // Tools that do not run as tasks answer synchronously.
    let response = tasks
        .call_tool_once(params("get_renderer_info", json!({})))
        .await
        .unwrap();
    let CallToolResponse::Complete(info) = response else {
        panic!("expected a complete result")
    };
    let info = serde_json::to_value(&info).unwrap();
    assert_eq!(
        structured(&info)["jobs"]["task_after_ms"],
        SHORT_THRESHOLD_MS,
        "{info}"
    );

    // A client without the extension gets today's synchronous result.
    let plain = connect(svc.clone(), false).await;
    let r = call(
        &plain,
        "render_postscript",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert!(png_of(&r).2.iter().all(|p| *p == [0, 0, 255, 255]));
    tasks.cancel().await.ok();
    plain.cancel().await.ok();
}

/// With a threshold of 0, every eligible call is a task at once (the
/// behaviour before #57), a failed render included.
#[tokio::test(flavor = "multi_thread")]
async fn with_a_zero_threshold_every_eligible_call_is_a_task() {
    let svc = service(relaxed(), tasks_after(0), &tag());
    let client = connect(svc.clone(), true).await;

    let response = client
        .call_tool_once(params("handwrite", json!({ "text": "hello" })))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task")
    };
    let finished = poll_task(&client, &created.task.task_id).await;
    let TaskPayload::Completed { result } = finished.payload else {
        panic!("expected completed, got {finished:?}")
    };
    assert_ne!(result["isError"], true, "{result:?}");
    assert_eq!(result["structuredContent"]["artifacts"][0]["width_px"], 612);

    // A failed render is a completed task whose result has isError.
    let response = client
        .call_tool_once(params("render_postscript", json!({ "source": "1 0 div" })))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task")
    };
    let finished = poll_task(&client, &created.task.task_id).await;
    let TaskPayload::Completed { result } = finished.payload else {
        panic!("expected completed, got {finished:?}")
    };
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "postscript_error"
    );
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 2);
    client.cancel().await.ok();
}

/// A quick call from a Tasks client returns its result directly and never
/// touches the job store: here the client's one job and the server's one
/// running-job permit are taken, and the budget holds one largest result,
/// all held by that job. A call that went through the store would be
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_quick_call_from_a_tasks_client_skips_the_job_caps_and_budget() {
    let tag = tag();
    let limits = LimitsConfig {
        max_steps: 0,
        ..relaxed()
    };
    let largest = JobsConfig::max_result_bytes(&limits);
    let svc = service(
        limits,
        JobsConfig {
            max_jobs_per_principal: 1,
            max_jobs: 1,
            job_results_budget_bytes: largest,
            ..tasks_after(LONG_THRESHOLD_MS)
        },
        &tag,
    );
    let client = connect(svc.clone(), true).await;
    let forever = start_job(&client, FOREVER).await;
    wait_for(
        "the job's worker starts",
        Duration::from_secs(10),
        async || processes_with(&tag) == 1,
    )
    .await;
    let charged = svc.jobs().charged_bytes();
    assert_eq!(charged, largest);
    // A job can't be started: the cap.
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(error_code(&r), "server_busy", "{r}");

    for (tool, args) in [
        (
            "render_postscript",
            json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
        ),
        ("handwrite", json!({ "text": "hi" })),
    ] {
        let response = client.call_tool_once(params(tool, args)).await.unwrap();
        let CallToolResponse::Complete(r) = response else {
            panic!("{tool}: expected a direct result, got {response:?}")
        };
        let r = serde_json::to_value(&r).unwrap();
        assert_ne!(r["isError"], true, "{tool}: {r}");
        assert_eq!(structured(&r)["ok"], true, "{tool}: {r}");
        if tool == "render_postscript" {
            assert!(png_of(&r).2.iter().all(|p| *p == [0, 0, 255, 255]));
        }
    }
    // A quick failure is direct too, with the tool's usual envelope.
    let response = client
        .call_tool_once(params("render_postscript", json!({ "source": "1 0 div" })))
        .await
        .unwrap();
    let CallToolResponse::Complete(r) = response else {
        panic!("expected a direct result, got {response:?}")
    };
    let r = serde_json::to_value(&r).unwrap();
    assert_eq!(error_code(&r), "postscript_error", "{r}");

    // The store saw none of it.
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 1);
    assert_eq!(svc.jobs().unfinished(), 1);
    assert_eq!(svc.jobs().charged_bytes(), charged);
    call(&client, "cancel_render", json!({ "job_id": forever })).await;
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    client.cancel().await.ok();
}

/// A slow call becomes a task with the worker it started: one worker, one
/// execution, and it outlives the 1 s sync deadline under the job's.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_call_becomes_a_task_and_keeps_its_worker() {
    let tag = tag();
    let svc = service(limits(), tasks_on(), &tag);
    let client = connect(svc.clone(), true).await;
    let response = client
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": SLOW_RED, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task, got {response:?}")
    };
    let id = created.task.task_id.clone();
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 1);
    // It is a job now: running from the start, and charged.
    let status = svc.jobs().status("local", &id).unwrap();
    assert_eq!(status.state, pscat_mcp::jobs::JobState::Running);
    assert!(svc.jobs().charged_bytes() > 0);
    let mut most = 0;
    let finished = loop {
        most = most.max(processes_with(&tag));
        let got = client
            .peer()
            .get_task(GetTaskParams::new(id.clone()))
            .await
            .unwrap();
        if got.task.status().is_terminal() {
            break got.task;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(most <= 1, "{most} workers");
    let TaskPayload::Completed { result } = finished.payload else {
        panic!("expected completed, got {finished:?}")
    };
    let result = Value::Object(result);
    assert_ne!(result["isError"], true, "{result}");
    assert!(png_of(&result).2.iter().all(|p| *p == [255, 0, 0, 255]));
    assert_eq!(svc.counters.executed.load(Ordering::SeqCst), 1);
    assert_eq!(svc.counters.timed_out.load(Ordering::SeqCst), 0);
    // Finished: it holds only its result; its slot and permit are free.
    assert_eq!(svc.jobs().unfinished(), 0);
    assert!(svc.jobs().charged_bytes() < JobsConfig::max_result_bytes(&limits()));
    let r = call(
        &client,
        "start_render",
        json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
    )
    .await;
    let quick = structured(&r)["job_id"].as_str().unwrap().to_string();
    let (_, last) = poll_until_finished(&client, &quick).await;
    assert_eq!(state(&last), "done", "{last}");
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_promoted_task_kills_its_worker() {
    let tag = tag();
    let svc = service(unlimited(), tasks_on(), &tag);
    let client = connect(svc.clone(), true).await;

    // review_sheet is not task-eligible: it answers synchronously.
    let response = client
        .call_tool_once(params(
            "review_sheet",
            json!({ "entries": [{ "source": "showpage", "width": 10, "height": 10 }] }),
        ))
        .await
        .unwrap();
    assert!(matches!(response, CallToolResponse::Complete(_)));

    let response = client
        .call_tool_once(params("render_postscript", json!({ "source": FOREVER })))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task")
    };
    let id = created.task.task_id.clone();
    wait_for("the worker runs", Duration::from_secs(10), async || {
        processes_with(&tag) == 1
    })
    .await;
    client
        .peer()
        .cancel_task(CancelTaskParams::new(id.clone()))
        .await
        .unwrap();
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    wait_for(
        "the task is cancelled",
        Duration::from_secs(5),
        async || svc.jobs().unfinished() == 0,
    )
    .await;
    let got = client
        .peer()
        .get_task(GetTaskParams::new(id.clone()))
        .await
        .unwrap();
    assert_eq!(got.task.status(), TaskStatus::Cancelled);
    assert_eq!(svc.counters.jobs_cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(svc.counters.executed.load(Ordering::SeqCst), 2);
    // An unknown task is rmcp's usual invalid-params error.
    let err = client
        .peer()
        .get_task(GetTaskParams::new("nope".to_string()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown task"), "{err}");
    client.cancel().await.ok();
}

/// Before it becomes a task, a call is cancelled like a synchronous one:
/// `notifications/cancelled` kills its worker, and no job is made.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_cancelled_before_the_threshold_kills_its_worker() {
    let tag = tag();
    let limits = LimitsConfig {
        max_steps: 0,
        ..relaxed()
    };
    let svc = service(limits, tasks_after(LONG_THRESHOLD_MS), &tag);
    let client = connect(svc.clone(), true).await;
    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params(
                "render_postscript",
                json!({ "source": FOREVER, "width": 20, "height": 20 }),
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("send");
    wait_for("the worker starts", Duration::from_secs(10), async || {
        processes_with(&tag) == 1
    })
    .await;
    handle
        .cancel(Some("test".into()))
        .await
        .expect("send notifications/cancelled");
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 0);
    assert_eq!(svc.jobs().charged_bytes(), 0);
    client.cancel().await.ok();
}

/// A call past the threshold that cannot become a task (no running-job
/// permit, its client's job cap) carries on synchronously and is stopped at
/// the sync deadline, as a synchronous call is.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_call_that_cannot_become_a_task_meets_the_sync_deadline() {
    let tag = tag();
    let svc = service(
        unlimited(),
        JobsConfig {
            max_jobs_per_principal: 1,
            ..tasks_on()
        },
        &tag,
    );
    let client = connect(svc.clone(), true).await;
    let forever = start_job(&client, FOREVER).await;
    wait_for(
        "the job's worker starts",
        Duration::from_secs(10),
        async || processes_with(&tag) == 1,
    )
    .await;
    let response = client
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": FOREVER, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Complete(r) = response else {
        panic!("expected a direct result, got {response:?}")
    };
    let r = serde_json::to_value(&r).unwrap();
    assert_eq!(error_code(&r), "timeout", "{r}");
    assert!(
        structured(&r)["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(&format!("{SYNC_TIMEOUT_MS} ms"))),
        "{r}"
    );
    // Its worker is gone; the job's still runs.
    wait_for(
        "the call's worker is gone",
        Duration::from_secs(5),
        async || processes_with(&tag) == 1,
    )
    .await;
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 1);
    assert_eq!(svc.counters.timed_out.load(Ordering::SeqCst), 1);
    assert_eq!(svc.jobs().unfinished(), 1);
    call(&client, "cancel_render", json!({ "job_id": forever })).await;
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_tasks_on_a_tasks_client_gets_a_result() {
    let svc = service(relaxed(), jobs(), &tag());
    let client = connect(svc, true).await;
    let response = client
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": QUICK_BLUE, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Complete(r) = response else {
        panic!("expected a complete result, got {response:?}")
    };
    assert_ne!(r.is_error, Some(true), "{r:?}");
    client.cancel().await.ok();
}

// ------------------------------------------------------------------- HTTP

const TOKEN_A: &str = "token-a-0123456789abcdef";
const TOKEN_B: &str = "token-b-0123456789abcdef";

struct Http {
    base: String,
    ct: CancellationToken,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

async fn serve_http(service: Arc<RenderService>) -> Http {
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
        shutdown_grace_secs: 1,
        async_jobs: true,
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
    Http {
        base: format!("http://{addr}/mcp"),
        ct,
    }
}

type HttpClient = RunningService<RoleClient, ()>;

async fn http_client(srv: &Http, token: &str) -> HttpClient {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.base.clone()).auth_header(token),
    );
    ().serve(transport).await.expect("initialize")
}

async fn http_call(client: &HttpClient, tool: &str, args: Value) -> Value {
    let r = client.call_tool(params(tool, args)).await.unwrap();
    serde_json::to_value(&r).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn over_http_another_token_cannot_see_the_job() {
    let tag = tag();
    let svc = service(unlimited(), jobs(), &tag);
    let srv = serve_http(svc.clone()).await;
    let a = http_client(&srv, TOKEN_A).await;
    let b = http_client(&srv, TOKEN_B).await;

    let r = http_call(
        &a,
        "start_render",
        json!({ "source": FOREVER, "width": 20, "height": 10 }),
    )
    .await;
    assert_eq!(structured(&r)["ok"], true, "{r}");
    let id = structured(&r)["job_id"].as_str().unwrap().to_string();
    // Another request (a new handler) of the same token finds it.
    let r = http_call(&a, "render_status", json!({ "job_id": id })).await;
    assert_eq!(structured(&r)["ok"], true, "{r}");

    // The other token gets exactly what an unknown id gets.
    let unknown = http_call(&b, "render_status", json!({ "job_id": "x" })).await;
    let unknown_msg = structured(&unknown)["error"]["message"]
        .as_str()
        .unwrap()
        .replace("\"x\"", "ID");
    for tool in ["render_status", "render_result", "cancel_render"] {
        let r = http_call(&b, tool, json!({ "job_id": id })).await;
        assert_eq!(r["isError"], true, "{tool}: {r}");
        assert_eq!(error_code(&r), "unknown_job", "{tool}: {r}");
        assert_eq!(
            structured(&r)["error"]["message"]
                .as_str()
                .unwrap()
                .replace(&format!("{id:?}"), "ID"),
            unknown_msg,
            "{tool}"
        );
    }
    // B's cancel did nothing; A's does.
    let r = http_call(&a, "render_status", json!({ "job_id": id })).await;
    assert_ne!(state(&r), "cancelled", "{r}");
    let r = http_call(&a, "cancel_render", json!({ "job_id": id })).await;
    assert_eq!(state(&r), "cancelled", "{r}");
    wait_for("the worker is gone", Duration::from_secs(5), async || {
        processes_with(&tag) == 0
    })
    .await;
    a.cancel().await.ok();
    b.cancel().await.ok();
}

/// Two bearer tokens over HTTP, a small results budget and a share of it
/// that holds one reservation: A fills its share and is refused with the
/// limit named, while B can still start; neither sees the other's jobs; and
/// cancelling frees A's share at once (a cancelled job's bookkeeping does
/// not hold it).
#[tokio::test(flavor = "multi_thread")]
async fn over_http_one_token_filling_its_share_leaves_the_other_able_to_start() {
    let small = LimitsConfig {
        max_output_bytes: 30_000,
        ..unlimited()
    };
    let largest = JobsConfig::max_result_bytes(&small);
    let tag = tag();
    let svc = service(
        small,
        JobsConfig {
            // Room for three reservations; each token may hold one.
            job_results_budget_bytes: 3 * largest,
            job_results_per_principal_bytes: Some(largest),
            ..jobs()
        },
        &tag,
    );
    let srv = serve_http(svc.clone()).await;
    let a = http_client(&srv, TOKEN_A).await;
    let b = http_client(&srv, TOKEN_B).await;
    let start = json!({ "source": FOREVER, "width": 20, "height": 10 });

    let r = http_call(&a, "start_render", start.clone()).await;
    assert_eq!(structured(&r)["ok"], true, "{r}");
    let a1 = structured(&r)["job_id"].as_str().unwrap().to_string();

    // A is at its share; the budget has room.
    let r = http_call(&a, "start_render", start.clone()).await;
    assert_eq!(r["isError"], true, "{r}");
    assert_eq!(error_code(&r), "results_budget_exceeded", "{r}");
    let input = &structured(&r)["error"]["input"];
    assert_eq!(input["code"], "results_per_principal_bytes", "{r}");
    assert_eq!(input["limit_bytes"], largest, "{r}");
    assert_eq!(input["reached_bytes"], 2 * largest, "{r}");
    assert_eq!(svc.jobs().unfinished(), 1, "the refused job was not made");

    // B is not held back by A's share.
    let r = http_call(&b, "start_render", start.clone()).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(structured(&r)["ok"], true, "{r}");
    let b1 = structured(&r)["job_id"].as_str().unwrap().to_string();
    assert_eq!(svc.jobs().charged_bytes(), 2 * largest);

    // Neither sees the other's job, and B's refusal comes from its own
    // share too.
    for (who, job) in [(&a, &b1), (&b, &a1)] {
        for tool in ["render_status", "render_result", "cancel_render"] {
            let r = http_call(who, tool, json!({ "job_id": job })).await;
            assert_eq!(r["isError"], true, "{tool}: {r}");
            assert_eq!(error_code(&r), "unknown_job", "{tool}: {r}");
        }
    }
    let r = http_call(&b, "start_render", start.clone()).await;
    assert_eq!(
        structured(&r)["error"]["input"]["code"],
        "results_per_principal_bytes",
        "{r}"
    );
    for (who, job) in [(&a, &a1), (&b, &b1)] {
        let r = http_call(who, "render_status", json!({ "job_id": job })).await;
        assert_ne!(state(&r), "cancelled", "{r}");
    }

    // Cancelling A's job frees A's share; B's job is untouched.
    let r = http_call(&a, "cancel_render", json!({ "job_id": a1 })).await;
    assert_eq!(state(&r), "cancelled", "{r}");
    let r = http_call(&a, "start_render", start.clone()).await;
    assert_eq!(structured(&r)["ok"], true, "{r}");
    let a2 = structured(&r)["job_id"].as_str().unwrap().to_string();
    let r = http_call(&b, "render_status", json!({ "job_id": b1 })).await;
    assert_ne!(state(&r), "cancelled", "{r}");

    for (who, job) in [(&a, &a2), (&b, &b1)] {
        http_call(who, "cancel_render", json!({ "job_id": job })).await;
    }
    wait_for(
        "the workers are gone",
        Duration::from_secs(10),
        async || processes_with(&tag) == 0,
    )
    .await;
    a.cancel().await.ok();
    b.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn over_http_job_tools_are_absent_without_async_jobs() {
    let svc = service(limits(), JobsConfig::default(), &tag());
    let srv = serve_http(svc).await;
    let a = http_client(&srv, TOKEN_A).await;
    let names: Vec<String> = a
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"render_postscript".to_string()));
    assert!(!names.iter().any(|n| n == "start_render"), "{names:?}");
    // A call to it is an unknown tool.
    assert!(
        a.call_tool(params("start_render", json!({ "source": "showpage" })))
            .await
            .is_err()
    );
    a.cancel().await.ok();
}

/// A Tasks-capable rmcp client over HTTP, as Claude Code connects.
async fn http_tasks_client(srv: &Http, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.base.clone()).auth_header(token),
    );
    // The Tasks extension needs the 2026-07-28 lifecycle (no `initialize`;
    // the capabilities travel in every request's `_meta`), which is how
    // Claude Code connects. rmcp's default `serve` starts a legacy session.
    ClientConfig::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::from_build_env(),
    )
    .serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .expect("discover")
}

/// The Tasks capability of an rmcp client reaches the server over HTTP: a
/// slow call becomes a task, found by a later request, and a plain client
/// gets the result.
#[tokio::test(flavor = "multi_thread")]
async fn over_http_an_rmcp_tasks_client_gets_a_task() {
    let svc = service(relaxed(), tasks_on(), &tag());
    let srv = serve_http(svc).await;
    let client = http_tasks_client(&srv, TOKEN_A).await;
    let response = client
        .call_tool_once(params(
            "render_postscript",
            json!({ "source": SLOW_RED, "width": 20, "height": 10 }),
        ))
        .await
        .unwrap();
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task, got {response:?}")
    };
    let task = poll_task(&client, &created.task.task_id).await;
    assert_eq!(task.status(), TaskStatus::Completed);
    client.cancel().await.ok();
}

/// Over HTTP a call dropped before the task threshold ends with the
/// request, as over stdio: the worker is killed and no job is made. The
/// threshold is far above the program's start-up, so the call is always
/// still waiting when it is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn over_http_a_call_dropped_before_the_threshold_kills_its_worker() {
    let tag = tag();
    let limits = LimitsConfig {
        max_steps: 0,
        ..relaxed()
    };
    let svc = service(limits, tasks_after(LONG_THRESHOLD_MS), &tag);
    let srv = serve_http(svc.clone()).await;
    let client = http_tasks_client(&srv, TOKEN_A).await;
    // (That this client's capability reaches the server over HTTP, so the
    // call is task-eligible, is the next test.)
    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params(
                "render_postscript",
                json!({ "source": FOREVER, "width": 20, "height": 20 }),
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("send");
    wait_for("the worker starts", Duration::from_secs(10), async || {
        processes_with(&tag) == 1
    })
    .await;
    assert_eq!(svc.counters.executed.load(Ordering::SeqCst), 1);
    handle.cancel(Some("test".into())).await.expect("cancel");
    wait_for("the worker is gone", Duration::from_secs(10), async || {
        processes_with(&tag) == 0
    })
    .await;
    assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 0);
    assert_eq!(svc.jobs().unfinished(), 0);
    assert_eq!(svc.jobs().charged_bytes(), 0);
    // The server is fine, and the same client still works.
    let r = client
        .call_tool(params("render_postscript", json!({ "source": QUICK_BLUE })))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    client.cancel().await.ok();
}

/// The body and `Mcp-Name` of a 2026-07-28 request (no session; client
/// capabilities in `_meta`), as Claude Code sends them.
fn current_request(method: &str, params: Value, tasks: bool) -> (String, String) {
    let mut params = params;
    let caps = if tasks {
        json!({ "extensions": { "io.modelcontextprotocol/tasks": {} } })
    } else {
        json!({})
    };
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "jobs-test", "version": "1.0" },
        "io.modelcontextprotocol/clientCapabilities": caps
    });
    // SEP-2243: Mcp-Name is the tool's name, or the task's id.
    let name = params["name"]
        .as_str()
        .or(params["taskId"].as_str())
        .unwrap_or_default()
        .to_string();
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    (body.to_string(), name)
}

async fn post_current(srv: &Http, token: &str, method: &str, params: Value, tasks: bool) -> Value {
    let (body, name) = current_request(method, params, tasks);
    let mut req = reqwest::Client::new()
        .post(&srv.base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method);
    if !name.is_empty() {
        req = req.header("Mcp-Name", name);
    }
    let resp = req.body(body).send().await.unwrap();
    serde_json::from_str(&resp.text().await.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn over_http_a_task_is_found_by_a_later_request_of_its_owner_only() {
    let svc = service(limits(), tasks_on(), &tag());
    let srv = serve_http(svc).await;
    let call = json!({
        "name": "render_postscript",
        "arguments": { "source": SLOW_RED, "width": 20, "height": 10 }
    });
    // Without the capability: the synchronous result (here, the sync
    // deadline).
    let sync = post_current(&srv, TOKEN_A, "tools/call", call.clone(), false).await;
    assert_eq!(
        sync["result"]["structuredContent"]["error"]["code"], "timeout",
        "{sync}"
    );
    let created = post_current(&srv, TOKEN_A, "tools/call", call, true).await;
    assert_eq!(created["result"]["resultType"], "task", "{created}");
    let id = created["result"]["taskId"].as_str().unwrap().to_string();

    // The other token: unknown task.
    let other = post_current(&srv, TOKEN_B, "tasks/get", json!({ "taskId": id }), true).await;
    assert!(
        other["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown task")),
        "{other}"
    );
    let other = post_current(&srv, TOKEN_B, "tasks/cancel", json!({ "taskId": id }), true).await;
    assert!(other["error"].is_object(), "{other}");

    // Each tasks/get is a new request and a new handler; the store is the
    // service's.
    let deadline = Instant::now() + Duration::from_secs(30);
    let done = loop {
        let got = post_current(&srv, TOKEN_A, "tasks/get", json!({ "taskId": id }), true).await;
        let status = got["result"]["status"].as_str().unwrap_or("").to_string();
        if status == "completed" {
            break got;
        }
        assert_eq!(status, "working", "{got}");
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let result = &done["result"]["result"];
    assert_ne!(result["isError"], true, "{done}");
    let (w, h, px) = png_of(result);
    assert_eq!((w, h), (20, 10));
    assert!(px.iter().all(|p| *p == [255, 0, 0, 255]));
}

// ------------------------------------------------- raw TCP clients (no rmcp)

/// Write a `tools/call` for a program that never ends to the server as a
/// hand-written HTTP/1.1 request on a raw socket: no rmcp, no HTTP client
/// library. The response is never read.
async fn raw_slow_call(srv: &Http, tasks: bool) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let (body, name) = current_request(
        "tools/call",
        json!({
            "name": "render_postscript",
            "arguments": { "source": FOREVER, "width": 20, "height": 20 }
        }),
        tasks,
    );
    let addr = srv
        .base
        .trim_start_matches("http://")
        .trim_end_matches("/mcp");
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Authorization: Bearer {TOKEN_A}\r\n\
         MCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\nMcp-Name: {name}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");
    stream
}

/// Drop a raw connection: a plain close (FIN), or a reset (RST, as when the
/// client process dies or a middlebox drops the flow). tokio deprecates
/// `set_linger` because a non-zero linger blocks on drop; zero does not.
#[allow(deprecated)]
fn drop_connection(stream: tokio::net::TcpStream, reset: bool) {
    if reset {
        stream.set_linger(Some(Duration::ZERO)).expect("linger 0");
        drop(stream);
    } else {
        drop(stream);
    }
}

/// A raw client sends the slow call, waits until its worker is running,
/// and drops the connection, once closed and once reset. Each time the
/// worker must be killed, and no job made. `tasks` sends the Tasks
/// capability, as Claude Code would on `tools/call`.
async fn raw_drop_kills_the_worker(svc: Arc<RenderService>, tag: &str, tasks: bool) {
    let srv = serve_http(svc.clone()).await;
    for (round, reset) in [false, true].into_iter().enumerate() {
        let stream = raw_slow_call(&srv, tasks).await;
        wait_for("the worker starts", Duration::from_secs(30), async || {
            processes_with(tag) == 1
        })
        .await;
        assert_eq!(
            svc.counters.executed.load(Ordering::SeqCst),
            round as u64 + 1
        );
        drop_connection(stream, reset);
        wait_for("the worker is gone", Duration::from_secs(10), async || {
            processes_with(tag) == 0
        })
        .await;
        assert_eq!(svc.counters.jobs_started.load(Ordering::SeqCst), 0);
        assert_eq!(svc.jobs().unfinished(), 0);
        assert_eq!(svc.jobs().charged_bytes(), 0);
    }
    // The slot is free and the server serves the next call.
    let r = post_current(
        &srv,
        TOKEN_A,
        "tools/call",
        json!({ "name": "render_postscript", "arguments": { "source": QUICK_BLUE } }),
        false,
    )
    .await;
    assert_ne!(r["result"]["isError"], true, "{r}");
}

/// The realistic failure of #86's cancelled-request case: the client
/// vanishes at the TCP level, not through a protocol message. A plain
/// `render_postscript` leaves no worker.
#[tokio::test(flavor = "multi_thread")]
async fn over_http_a_raw_tcp_drop_kills_the_worker() {
    let tag = tag();
    let limits = LimitsConfig {
        max_steps: 0,
        // Only the drop can end the loop within the test's time frame.
        render_timeout_ms: 60_000,
        ..relaxed()
    };
    let jobs = JobsConfig {
        job_timeout_ms: 70_000,
        ..jobs()
    };
    let svc = service(limits, jobs, &tag);
    raw_drop_kills_the_worker(svc, &tag, false).await;
}

/// The same with the Tasks capability on the request and `--tasks` on: the
/// threshold is far above the program's start-up, so the call is dropped
/// before it would become a task, and no job is made. (The 2026-07-28 shape
/// is what carries the capability to this stateless server; the request is
/// the one `over_http_a_task_is_found_by_a_later_request_of_its_owner_only`
/// turns into a task past a short threshold.)
#[tokio::test(flavor = "multi_thread")]
async fn over_http_a_raw_tcp_drop_before_the_threshold_kills_its_worker() {
    let tag = tag();
    let limits = LimitsConfig {
        max_steps: 0,
        ..relaxed()
    };
    let svc = service(limits, tasks_after(LONG_THRESHOLD_MS), &tag);
    raw_drop_kills_the_worker(svc, &tag, true).await;
}

/// `render_result` returns what `render_postscript` would: SVG as text up
/// to `PSCAT_MCP_MAX_SVG_BYTES`, as a blob over it, with `returned_as` to
/// match.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_returns_svg_as_text_or_blob_by_the_svg_limit() {
    const MARKS: &str = "0 1 300 { dup 7 mul 200 mod exch 13 mul 100 mod 3 3 rectfill } for";
    for (limit, want) in [(1_000_000, "text"), (1_000, "blob")] {
        let tag = tag();
        let svc = service(
            LimitsConfig {
                max_svg_bytes: Some(limit),
                ..relaxed()
            },
            jobs(),
            &tag,
        );
        let client = connect(svc, false).await;
        let r = call(
            &client,
            "start_render",
            json!({ "source": MARKS, "width": 200, "height": 100, "format": "svg" }),
        )
        .await;
        assert_eq!(structured(&r)["ok"], true, "{r}");
        let id = structured(&r)["job_id"].as_str().unwrap().to_string();
        let (seen, last) = poll_until_finished(&client, &id).await;
        assert_eq!(seen.last().map(String::as_str), Some("done"), "{last}");
        let result = call(&client, "render_result", json!({ "job_id": id })).await;
        assert_ne!(result["isError"], true, "{result}");
        let s = structured(&result);
        assert_eq!(s["format"], "svg", "{s}");
        assert_eq!(s["artifacts"][0]["returned_as"], want, "{s}");
        let bytes = s["artifacts"][0]["bytes"].as_u64().unwrap();
        assert!(bytes > 1_000 && bytes < 1_000_000, "{s}");
        let res = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "resource")
            .expect("a resource block");
        let res = &res["resource"];
        assert_eq!(res["mimeType"], "image/svg+xml", "{res}");
        match want {
            "text" => assert!(res["text"].as_str().unwrap().contains("<svg"), "{res}"),
            _ => {
                let svg = base64::engine::general_purpose::STANDARD
                    .decode(res["blob"].as_str().unwrap())
                    .unwrap();
                assert_eq!(svg.len() as u64, bytes);
                assert!(String::from_utf8(svg).unwrap().contains("<svg"));
            }
        }
        client.cancel().await.ok();
        assert_eq!(processes_with(&tag), 0);
    }
}

/// Many small marks: an SVG well over the 20 000-byte text limit set below.
const MANY_MARKS: &str =
    "1 srand 0 1 2000 { pop rand 500 mod rand 500 mod 3 3 rectfill } for showpage";
const SVG_CAP: u64 = 20_000;

/// Over stdio (jobs are always on there) with the SVG text limit set
/// through the environment: a `start_render` job whose SVG is over the
/// limit ends `done`, and `render_result` returns it as a blob resource
/// holding the whole SVG (before #63 it failed with `output_too_large`).
/// Polls with a generous deadline.
#[tokio::test]
async fn a_job_whose_svg_is_over_the_cap_returns_a_blob() {
    let cap = SVG_CAP.to_string();
    let client = common::start_stdio(
        env!("CARGO_BIN_EXE_pscat-mcp-server"),
        &[("PSCAT_MCP_MAX_SVG_BYTES", &cap)],
    )
    .await;
    let start = client
        .call_tool(
            CallToolRequestParams::new("start_render").with_arguments(
                json!({"source": MANY_MARKS, "width": 500, "height": 500, "format": "svg"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("start_render");
    assert_ne!(start.is_error, Some(true), "{start:?}");
    let id = start.structured_content.as_ref().unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(60);
    let state = loop {
        let r = client
            .call_tool(
                CallToolRequestParams::new("render_status")
                    .with_arguments(json!({ "job_id": id }).as_object().unwrap().clone()),
            )
            .await
            .expect("render_status");
        let s = r.structured_content.as_ref().unwrap()["state"]
            .as_str()
            .unwrap()
            .to_string();
        if matches!(s.as_str(), "done" | "failed" | "cancelled") {
            break s;
        }
        assert!(Instant::now() < deadline, "job never finished: {s}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(state, "done");

    let r = client
        .call_tool(
            CallToolRequestParams::new("render_result")
                .with_arguments(json!({ "job_id": id }).as_object().unwrap().clone()),
        )
        .await
        .expect("render_result");
    assert_eq!(r.is_error, Some(false), "{r:?}");
    let s = r.structured_content.clone().expect("structured content");
    assert_eq!(s["ok"], true, "{s}");
    assert_eq!(s["format"], "svg", "{s}");
    assert_eq!(s["artifacts"][0]["returned_as"], "blob", "{s}");
    let bytes = s["artifacts"][0]["bytes"].as_u64().unwrap();
    assert!(bytes > SVG_CAP, "{s}");
    let blobs: Vec<_> = r
        .content
        .iter()
        .filter_map(|c| match c.as_resource().map(|e| &e.resource) {
            Some(ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            }) => {
                assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
                Some(blob.clone())
            }
            Some(other) => panic!("expected a blob resource, got {other:?}"),
            None => None,
        })
        .collect();
    assert_eq!(blobs.len(), 1);
    let svg = base64::engine::general_purpose::STANDARD
        .decode(&blobs[0])
        .unwrap();
    assert_eq!(svg.len() as u64, bytes);
    assert!(String::from_utf8(svg).unwrap().contains("<svg"));
    client.cancel().await.ok();
}
