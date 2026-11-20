//! The overall deadline of one `put` to the Cloud Storage backend (#149),
//! against a local fake of the metadata server, IAM Credentials and Cloud
//! Storage whose every answer is slow. No Google endpoint is called.
//!
//! Without the deadline a slow Google makes a call as long as the sum of its
//! per-call timeouts' worth of waiting (every page makes two calls); with
//! it the call ends at the deadline with `storage_unavailable`.

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use pscat_mcp::artifacts::{ArtifactObject, ArtifactStore, RENDER_CHARGE_BYTES};
use pscat_mcp::config::{ArtifactConfig, JobsConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::gcs::{GcsArtifactStore, GcsSettings};
use pscat_mcp::mcp::PscatMcp;
use pscat_mcp::render::{ErrorCode, RenderService};
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const BUCKET: &str = "example-bucket";
const SA: &str = "renderer@example-project.iam.gserviceaccount.com";

// -------------------------------------------------------------- the fake

/// How long each endpoint takes to answer, in milliseconds (`HANG` never
/// answers), and how many requests of each kind arrived.
#[derive(Default)]
struct Fake {
    metadata_ms: AtomicU64,
    storage_ms: AtomicU64,
    iam_ms: AtomicU64,
    tokens: AtomicUsize,
    uploads: AtomicUsize,
    signs: AtomicUsize,
    /// Names of the uploaded objects, in arrival order.
    names: Mutex<Vec<String>>,
}

const HANG: u64 = u64::MAX;

async fn answer_after(ms: u64) {
    if ms == HANG {
        std::future::pending::<()>().await;
    } else if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

async fn handle(State(f): State<Arc<Fake>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    if path.starts_with("/computeMetadata/") {
        answer_after(f.metadata_ms.load(Ordering::SeqCst)).await;
        let flavor = [("Metadata-Flavor", "Google")];
        if path.ends_with("/default/token") {
            f.tokens.fetch_add(1, Ordering::SeqCst);
            return (
                flavor,
                axum::Json(json!({"access_token": "ya29.fake-token", "expires_in": 3600})),
            )
                .into_response();
        }
        if path.ends_with("/default/email") {
            return (flavor, SA).into_response();
        }
    }
    if path.starts_with("/upload/storage/v1/b/") {
        f.uploads.fetch_add(1, Ordering::SeqCst);
        // The object name is in the multipart metadata part.
        let body = axum::body::to_bytes(req.into_body(), 64 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        if let Some(i) = text.find("\"name\":\"") {
            let name = text[i + 8..].split('"').next().unwrap_or("").to_string();
            f.names.lock().unwrap().push(name);
        }
        answer_after(f.storage_ms.load(Ordering::SeqCst)).await;
        return axum::Json(json!({"kind": "storage#object"})).into_response();
    }
    if path.ends_with(":signBlob") {
        f.signs.fetch_add(1, Ordering::SeqCst);
        answer_after(f.iam_ms.load(Ordering::SeqCst)).await;
        let blob = base64::engine::general_purpose::STANDARD.encode(b"signature");
        return axum::Json(json!({"keyId": "k1", "signedBlob": blob})).into_response();
    }
    http::StatusCode::NOT_FOUND.into_response()
}

struct FakeServer {
    fake: Arc<Fake>,
    base: String,
    _stop: tokio_util::sync::DropGuard,
}

impl FakeServer {
    /// Every endpoint answers after `ms`.
    fn slow(&self, ms: u64) {
        for a in [
            &self.fake.metadata_ms,
            &self.fake.storage_ms,
            &self.fake.iam_ms,
        ] {
            a.store(ms, Ordering::SeqCst);
        }
    }
    fn requests(&self) -> usize {
        self.fake.tokens.load(Ordering::SeqCst)
            + self.fake.uploads.load(Ordering::SeqCst)
            + self.fake.signs.load(Ordering::SeqCst)
    }
}

async fn fake_google() -> FakeServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let fake = Arc::new(Fake::default());
    let app = axum::Router::new()
        .fallback(handle)
        .with_state(fake.clone());
    let stop = CancellationToken::new();
    let s = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(s.cancelled_owned())
            .await;
    });
    FakeServer {
        fake,
        base: format!("http://{addr}"),
        _stop: stop.drop_guard(),
    }
}

// ---------------------------------------------------------------- helpers

fn settings(fake: &FakeServer, timeout: Duration, deadline: Duration) -> GcsSettings {
    GcsSettings {
        bucket: BUCKET.into(),
        prefix: "renders/".into(),
        signer: None,
        ttl: Duration::from_secs(1800),
        quota: 1 << 20,
        timeout,
        deadline,
        metadata_endpoint: fake.base.clone(),
        iam_endpoint: fake.base.clone(),
        storage_endpoint: fake.base.clone(),
        short_links: None,
    }
}

fn store(fake: &FakeServer, timeout: Duration, deadline: Duration) -> GcsArtifactStore {
    GcsArtifactStore::new(settings(fake, timeout, deadline), Arc::new(SystemTime::now)).unwrap()
}

fn objects(n: usize) -> Vec<ArtifactObject> {
    (0..n)
        .map(|i| ArtifactObject {
            mime_type: "image/png".into(),
            bytes: vec![i as u8; 100].into(),
        })
        .collect()
}

/// The charge of a call that stores `n` objects of 100 bytes.
fn charge(n: usize) -> usize {
    n * 100 + RENDER_CHARGE_BYTES
}

/// What a failed message must not hold, and must say.
fn assert_clean_deadline(e: &pscat_mcp::render::RenderError, stage: &str) {
    assert_eq!(e.code, ErrorCode::StorageUnavailable, "{e:?}");
    assert!(e.message.contains("deadline"), "{}", e.message);
    assert!(e.message.contains(stage), "{}", e.message);
    assert!(e.message.contains("retry"), "{}", e.message);
    assert!(e.message.contains("inline"), "{}", e.message);
    for secret in ["http", "ya29", "renders/", BUCKET, "@"] {
        assert!(!e.message.contains(secret), "{secret} in {}", e.message);
    }
}

// -------------------------------------------------- the store, directly

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_multi_page_put_stops_at_the_deadline_not_after_the_sum_of_the_calls() {
    let fake = fake_google().await;
    // Every call takes 500 ms and is well inside its 5 s timeout: eight
    // pages are 2 + 8 + 8 calls, 9 s in all. The deadline is 3 s: the token
    // and the signer take 1 s, so the uploads start at 1 s and a few fit
    // before the deadline, with 2 s to spare on a stalled machine; the
    // signatures could only start at 5 s.
    fake.slow(500);
    let s = store(&fake, Duration::from_secs(5), Duration::from_secs(3));
    let started = Instant::now();
    let e = s
        .put("a", objects(8))
        .await
        .expect_err("the put must hit its deadline");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(2900), "{took:?}");
    // Bounded by what a broken deadline would take (the 9 s sum), not by the
    // deadline plus a margin: a stalled runner must not fail the test.
    assert!(took < Duration::from_secs(7), "{took:?}");
    assert_clean_deadline(&e, "upload");
    // Some uploads started, and the rest never did: the call stopped.
    let uploads = fake.fake.uploads.load(Ordering::SeqCst);
    assert!((1..8).contains(&uploads), "{uploads} uploads");
    assert_eq!(fake.fake.signs.load(Ordering::SeqCst), 0, "no signing yet");
    // The fake counts a request when it arrives, before it answers slowly.
    // Settle first, so a request sent just before the deadline and still
    // on its way is not read as work that went on, then watch for longer
    // than a call takes: nothing new may arrive.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = fake.requests();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.requests(), seen, "work went on after the deadline");
    // Objects may exist, so the charge stays (as for any failed upload).
    assert_eq!(s.charged_bytes("a"), charge(8));
    // No page of the failed render is named twice or out of order.
    let names = fake.fake.names.lock().unwrap().clone();
    for (i, name) in names.iter().enumerate() {
        assert!(name.ends_with(&format!("/{}.png", i + 1)), "{names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_deadline_can_fall_in_the_signing_stage() {
    let fake = fake_google().await;
    fake.slow(0);
    // Token, signer and uploads answer at once (2 s of slack for them);
    // each of the eight signatures takes 500 ms, 4 s in all, and the
    // deadline is 2 s: it falls in the signing stage with signatures left.
    fake.fake.iam_ms.store(500, Ordering::SeqCst);
    let s = store(&fake, Duration::from_secs(5), Duration::from_secs(2));
    let e = s.put("a", objects(8)).await.expect_err("deadline");
    assert_clean_deadline(&e, "signing");
    assert_eq!(fake.fake.uploads.load(Ordering::SeqCst), 8);
    assert!(fake.fake.signs.load(Ordering::SeqCst) < 8);
    assert_eq!(s.charged_bytes("a"), charge(8));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_before_any_upload_refunds_the_charge() {
    let fake = fake_google().await;
    fake.slow(0);
    fake.fake.metadata_ms.store(HANG, Ordering::SeqCst);
    let s = store(&fake, Duration::from_secs(5), Duration::from_millis(400));
    let started = Instant::now();
    let e = s.put("a", objects(3)).await.expect_err("deadline");
    // A broken deadline waits for the 5 s per-call timeout.
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_clean_deadline(&e, "credentials");
    assert_eq!(fake.fake.uploads.load(Ordering::SeqCst), 0);
    assert_eq!(s.charged_bytes("a"), 0, "nothing was uploaded");
}

#[tokio::test(flavor = "multi_thread")]
async fn callers_waiting_for_the_token_are_bound_by_the_deadline_too() {
    let fake = fake_google().await;
    fake.slow(0);
    // The metadata server never answers and each call may take 8 s. The
    // first caller holds the token lock for that long, and each caller
    // after it would wait for the lock and then try for its own 8 s.
    fake.fake.metadata_ms.store(HANG, Ordering::SeqCst);
    let s = Arc::new(store(&fake, Duration::from_secs(8), Duration::from_secs(2)));
    let started = Instant::now();
    let calls: Vec<_> = (0..4)
        .map(|i| {
            let s = s.clone();
            tokio::spawn(async move { s.put(&format!("p{i}"), objects(2)).await })
        })
        .collect();
    for c in calls {
        let e = c.await.unwrap().expect_err("deadline");
        assert_clean_deadline(&e, "credentials");
    }
    // A broken deadline takes the 8 s per-call timeout for the first caller
    // and more for each caller behind it.
    assert!(started.elapsed() < Duration::from_secs(6));
    // The lock was released: a healthy metadata server serves the next call.
    fake.fake.metadata_ms.store(0, Ordering::SeqCst);
    let s2 = store(&fake, Duration::from_secs(5), Duration::from_secs(5));
    s2.put("a", objects(1)).await.expect("healthy again");
    s.put("a", objects(1)).await.expect("the lock is free");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_put_inside_the_deadline_is_unchanged() {
    let fake = fake_google().await;
    fake.slow(30);
    let s = store(&fake, Duration::from_secs(5), Duration::from_secs(10));
    let stored = s.put("a", objects(3)).await.expect("a healthy put");
    assert_eq!(stored.links.len(), 3);
    assert_eq!(fake.fake.uploads.load(Ordering::SeqCst), 3);
    assert_eq!(fake.fake.signs.load(Ordering::SeqCst), 3);
}

// ------------------------------------------------------ through the tools

fn worker() -> WorkerConfig {
    WorkerConfig {
        worker_exe: Some(SERVER.into()),
        ..WorkerConfig::default()
    }
}

fn config(fake: &FakeServer, deadline_ms: u64) -> ArtifactConfig {
    ArtifactConfig {
        artifact_gcs_metadata_endpoint: Some(fake.base.clone()),
        artifact_gcs_iam_endpoint: Some(fake.base.clone()),
        artifact_gcs_storage_endpoint: Some(fake.base.clone()),
        artifact_gcs_timeout_ms: 5000,
        artifact_gcs_deadline_ms: deadline_ms,
        artifact_threshold_bytes: 0,
        ..ArtifactConfig::default().gcs(BUCKET)
    }
}

async fn connect(artifacts: ArtifactConfig) -> Client {
    let service: Arc<RenderService> = pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig::default().enabled(),
        artifacts,
        &worker(),
    )
    .expect("service");
    let (server_io, client_io) = tokio::io::duplex(1 << 22);
    let server = PscatMcp::new(service);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::from_build_env(),
    )
    .serve(client_io)
    .await
    .expect("client initialize")
}

async fn call(client: &Client, args: Value) -> Value {
    let Value::Object(map) = args else {
        unreachable!()
    };
    let r = client
        .call_tool(CallToolRequestParams::new("render_postscript".to_string()).with_arguments(map))
        .await
        .expect("tool call");
    serde_json::to_value(&r).unwrap()
}

fn pages(n: usize) -> String {
    (0..n)
        .map(|i| format!("{} setgray 0 0 30 20 rectfill showpage ", i as f64 / 10.0))
        .collect()
}

fn images(r: &Value) -> usize {
    r["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "image")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn url_is_storage_unavailable_and_both_and_auto_fall_back_inline_within_the_deadline() {
    let fake = fake_google().await;
    // Six pages: 2 + 6 + 6 calls of 800 ms, 11 s in all, each within its
    // 5 s timeout. The deadline is 1.2 s.
    fake.slow(800);
    let client = connect(config(&fake, 1200)).await;
    let args = |delivery: &str| json!({"source": pages(6), "width": 30, "height": 20, "delivery": delivery});
    // What the render alone takes here (the first call also starts the
    // worker), so a loaded machine's render time is not blamed on Google.
    let inline = json!({"source": pages(6), "width": 30, "height": 20});
    call(&client, inline.clone()).await;
    let started = Instant::now();
    call(&client, inline).await;
    let render = started.elapsed();
    for delivery in ["url", "both", "auto"] {
        let started = Instant::now();
        let r = call(&client, args(delivery)).await;
        let took = started.elapsed();
        // The deadline is 1.2 s over the render; a broken one would take the
        // 11 s the calls add up to. Bounded by that, not by the deadline
        // plus a margin, so a stalled runner does not fail the test.
        assert!(
            took < render + Duration::from_secs(8),
            "{delivery}: {took:?} (a render alone: {render:?})"
        );
        if delivery == "url" {
            assert_eq!(r["isError"], true, "{r}");
            let err = &r["structuredContent"]["error"];
            assert_eq!(err["code"], "storage_unavailable", "{r}");
            let msg = err["message"].as_str().unwrap();
            assert!(msg.contains("retry") && msg.contains("inline"), "{msg}");
            assert_eq!(images(&r), 0);
        } else {
            assert_eq!(r["isError"], false, "{delivery}: {r}");
            assert_eq!(images(&r), 6, "{delivery}: every page inline");
            let sc = &r["structuredContent"];
            assert!(sc.get("render_id").is_none(), "{delivery}");
            assert_eq!(sc["artifacts"][0]["returned_as"], "image");
            assert!(sc["artifacts"][0].get("download_url").is_none());
            let note = sc["delivery_note"].as_str().unwrap();
            assert!(note.contains("storage_unavailable"), "{note}");
        }
    }
}

// ------------------------------------------------------------ settings

fn check(c: ArtifactConfig) -> Result<(), String> {
    c.check(&LimitsConfig::default(), &JobsConfig::default())
}

#[test]
fn the_deadline_setting_is_checked_and_its_default_fits_the_documented_request_timeout() {
    assert!(check(ArtifactConfig::default().gcs(BUCKET)).is_ok());
    let e = check(ArtifactConfig {
        artifact_gcs_deadline_ms: 0,
        ..ArtifactConfig::default().gcs(BUCKET)
    })
    .unwrap_err();
    assert!(e.contains("artifact-gcs-deadline-ms"), "{e}");
    // Not checked without gcs, like the other gcs settings.
    assert!(
        check(ArtifactConfig {
            artifact_gcs_deadline_ms: 0,
            ..ArtifactConfig::default()
        })
        .is_ok()
    );
    // docs/CLOUD_RUN.md: the request timeout (60 s is the advice) must
    // exceed the render, the queue wait and this deadline together.
    let limits = LimitsConfig::default();
    let total = limits.render_timeout_ms.max(
        limits
            .max_sheet_timeout_ms
            .unwrap_or(limits.render_timeout_ms),
    ) + limits.queue_timeout_ms
        + ArtifactConfig::default().artifact_gcs_deadline_ms;
    assert!(total < 60_000, "{total} ms");
}

#[test]
fn the_deadline_can_be_set_by_the_environment_and_zero_refuses_to_start() {
    let run = |value: &str| {
        let mut cmd = std::process::Command::new(SERVER);
        for (k, _) in std::env::vars() {
            if k.starts_with("PSCAT_MCP_") || k == "PORT" || k == "RUST_LOG" {
                cmd.env_remove(k);
            }
        }
        let out = cmd
            .arg("serve-stdio")
            .env("PSCAT_MCP_LOG_FORMAT", "pretty")
            .env("PSCAT_MCP_ARTIFACT_LINKS", "gcs")
            .env("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET)
            .env("PSCAT_MCP_ARTIFACT_GCS_DEADLINE_MS", value)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("run the server");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (ok, err) = run("0");
    assert!(!ok && err.contains("artifact-gcs-deadline-ms"), "{err}");
    let (ok, err) = run("soon");
    assert!(!ok && err.contains("artifact-gcs-deadline-ms"), "{err}");
}
