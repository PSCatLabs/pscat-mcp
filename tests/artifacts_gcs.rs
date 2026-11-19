//! The Cloud Storage artifact store (#140), against a local fake standing
//! in for the metadata server, the IAM Credentials API and Cloud Storage.
//! Nothing here reaches a Google endpoint.
//!
//! The fake IAM "signs" a blob as its SHA-256 digest, so a test recomputes
//! the expected `X-Goog-Signature` from its own canonical request.

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use pscat_mcp::artifacts::{ArtifactObject, ArtifactStore, LinkScheme, LinksBackend};
use pscat_mcp::auth::BearerTokenAuth;
use pscat_mcp::config::{ArtifactConfig, JobsConfig, LimitsConfig, WorkerConfig};
use pscat_mcp::gcs::{GcsArtifactStore, GcsSettings, WallClock};
use pscat_mcp::mcp::PscatMcp;
use pscat_mcp::render::{ErrorCode, RenderService};
use pscat_mcp::short_link::ShortLinks;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation,
    ReadResourceRequestParams,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

type Client = RunningService<RoleClient, ClientConfig>;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const BUCKET: &str = "example-bucket";
/// What the fake metadata server reports as the default account.
const DEFAULT_SA: &str = "renderer@example-project.iam.gserviceaccount.com";
const TOKEN: &str = "test-token-for-gcs-links";

// -------------------------------------------------------------- the fake

/// How a fake endpoint answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ok,
    Status(u16),
    Hang,
}

/// One upload the fake received.
#[derive(Debug, Clone)]
struct Upload {
    path: String,
    query: String,
    auth: String,
    /// The metadata part, parsed.
    metadata: Value,
    /// The media part's `Content-Type`.
    media_type: String,
    bytes: Vec<u8>,
}

/// One `signBlob` call.
#[derive(Debug, Clone)]
struct SignCall {
    path: String,
    auth: String,
    payload: String,
}

struct Fake {
    metadata: Mutex<Mode>,
    storage: Mutex<Mode>,
    iam: Mutex<Mode>,
    expires_in: Mutex<u64>,
    tokens: AtomicUsize,
    emails: AtomicUsize,
    uploads: Mutex<Vec<Upload>>,
    signs: Mutex<Vec<SignCall>>,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            metadata: Mutex::new(Mode::Ok),
            storage: Mutex::new(Mode::Ok),
            iam: Mutex::new(Mode::Ok),
            expires_in: Mutex::new(3600),
            tokens: AtomicUsize::new(0),
            emails: AtomicUsize::new(0),
            uploads: Mutex::new(Vec::new()),
            signs: Mutex::new(Vec::new()),
        }
    }
}

/// The access token the fake's `n`th token fetch (from 1) hands out.
fn fake_token(n: usize) -> String {
    format!("ya29.fake-access-token-{n}")
}

struct FakeServer {
    fake: Arc<Fake>,
    base: String,
    _stop: tokio_util::sync::DropGuard,
}

impl FakeServer {
    fn set(&self, which: &str, mode: Mode) {
        let m = match which {
            "metadata" => &self.fake.metadata,
            "storage" => &self.fake.storage,
            "iam" => &self.fake.iam,
            _ => unreachable!(),
        };
        *m.lock().unwrap() = mode;
    }
    fn uploads(&self) -> Vec<Upload> {
        self.fake.uploads.lock().unwrap().clone()
    }
    fn signs(&self) -> Vec<SignCall> {
        self.fake.signs.lock().unwrap().clone()
    }
}

fn header(req: &http::request::Parts, name: &str) -> String {
    req.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn gate(mode: Mode) -> Option<Response> {
    match mode {
        Mode::Ok => None,
        Mode::Status(s) => Some(
            (
                http::StatusCode::from_u16(s).unwrap(),
                "{\"error\":\"fake failure with a body the server must not echo\"}",
            )
                .into_response(),
        ),
        Mode::Hang => {
            std::future::pending::<()>().await;
            None
        }
    }
}

/// Split a `multipart/related` body into (headers, content) parts.
fn multipart(content_type: &str, body: &[u8]) -> Vec<(String, Vec<u8>)> {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .expect("a boundary")
        .to_string();
    let delim = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut rest = body;
    // Skip to the first delimiter.
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let start = find(rest, delim.as_bytes()).expect("first delimiter");
    rest = &rest[start + delim.len()..];
    loop {
        if rest.starts_with(b"--") {
            break;
        }
        rest = rest.strip_prefix(b"\r\n").expect("CRLF after delimiter");
        let head_end = find(rest, b"\r\n\r\n").expect("part headers");
        let head = String::from_utf8(rest[..head_end].to_vec()).unwrap();
        rest = &rest[head_end + 4..];
        let next = find(rest, format!("\r\n{delim}").as_bytes()).expect("next delimiter");
        parts.push((head, rest[..next].to_vec()));
        rest = &rest[next + 2 + delim.len()..];
    }
    parts
}

async fn handle(State(f): State<Arc<Fake>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body: Bytes = axum::body::to_bytes(body, 64 << 20).await.unwrap();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    if path.starts_with("/computeMetadata/") {
        if header(&parts, "metadata-flavor") != "Google" {
            return http::StatusCode::FORBIDDEN.into_response();
        }
        let mode = *f.metadata.lock().unwrap();
        if let Some(r) = gate(mode).await {
            return r;
        }
        let flavor = [("Metadata-Flavor", "Google")];
        if path.ends_with("/default/token") {
            let n = f.tokens.fetch_add(1, Ordering::SeqCst) + 1;
            let expires_in = *f.expires_in.lock().unwrap();
            return (
                flavor,
                axum::Json(
                    json!({"access_token": fake_token(n), "expires_in": expires_in,
                                  "token_type": "Bearer"}),
                ),
            )
                .into_response();
        }
        if path.ends_with("/default/email") {
            f.emails.fetch_add(1, Ordering::SeqCst);
            return (flavor, DEFAULT_SA).into_response();
        }
        return http::StatusCode::NOT_FOUND.into_response();
    }
    if parts.method == http::Method::POST && path.starts_with("/upload/storage/v1/b/") {
        let mode = *f.storage.lock().unwrap();
        // Record the attempt before failing, so a test sees it was made.
        let ct = header(&parts, "content-type");
        let p = multipart(&ct, &body);
        assert_eq!(p.len(), 2, "metadata and media");
        let metadata: Value = serde_json::from_slice(&p[0].1).unwrap();
        assert!(p[0].0.contains("application/json"), "{}", p[0].0);
        f.uploads.lock().unwrap().push(Upload {
            path: path.clone(),
            query,
            auth: header(&parts, "authorization"),
            metadata,
            media_type: p[1]
                .0
                .trim()
                .strip_prefix("Content-Type: ")
                .unwrap_or("")
                .to_string(),
            bytes: p[1].1.clone(),
        });
        if let Some(r) = gate(mode).await {
            return r;
        }
        return axum::Json(json!({"kind": "storage#object"})).into_response();
    }
    if parts.method == http::Method::POST && path.ends_with(":signBlob") {
        let mode = *f.iam.lock().unwrap();
        if let Some(r) = gate(mode).await {
            return r;
        }
        let v: Value = serde_json::from_slice(&body).unwrap();
        let payload = b64().decode(v["payload"].as_str().unwrap()).unwrap();
        let digest = Sha256::digest(&payload);
        f.signs.lock().unwrap().push(SignCall {
            path,
            auth: header(&parts, "authorization"),
            payload: String::from_utf8(payload).unwrap(),
        });
        return axum::Json(json!({"keyId": "k1", "signedBlob": b64().encode(digest)}))
            .into_response();
    }
    http::StatusCode::NOT_FOUND.into_response()
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

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn at(rfc3339: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .into()
}

fn manual_clock(t: SystemTime) -> (WallClock, Arc<Mutex<SystemTime>>) {
    let t = Arc::new(Mutex::new(t));
    let read = t.clone();
    (Arc::new(move || *read.lock().unwrap()), t)
}

fn settings(fake: &FakeServer) -> GcsSettings {
    GcsSettings {
        bucket: BUCKET.into(),
        prefix: "renders/".into(),
        signer: None,
        ttl: Duration::from_secs(1800),
        quota: 1 << 20,
        timeout: Duration::from_secs(5),
        metadata_endpoint: fake.base.clone(),
        iam_endpoint: fake.base.clone(),
        storage_endpoint: fake.base.clone(),
        short_links: None,
    }
}

fn store(settings: GcsSettings, clock: WallClock) -> GcsArtifactStore {
    GcsArtifactStore::new(settings, clock).unwrap()
}

fn object(mime: &str, len: usize) -> ArtifactObject {
    ArtifactObject {
        mime_type: mime.into(),
        bytes: (0..len).map(|i| (i * 7) as u8).collect::<Vec<u8>>().into(),
    }
}

/// The signed URL the server must produce for `object` at `datetime`, built
/// here independently of the server's code, with the fake's signature.
fn expected_url(object: &str, signer: &str, datetime: &str, expires: u64) -> (String, String) {
    let date = &datetime[..8];
    let credential = format!("{signer}/{date}/auto/storage/goog4_request")
        .replace('@', "%40")
        .replace('/', "%2F");
    let query = format!(
        "X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential={credential}\
         &X-Goog-Date={datetime}&X-Goog-Expires={expires}&X-Goog-SignedHeaders=host"
    );
    let path = format!("/{BUCKET}/{object}");
    let canonical =
        format!("GET\n{path}\n{query}\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD");
    let sts = format!(
        "GOOG4-RSA-SHA256\n{datetime}\n{date}/auto/storage/goog4_request\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let sig = hex(&Sha256::digest(sts.as_bytes()));
    (
        format!("https://storage.googleapis.com{path}?{query}&X-Goog-Signature={sig}"),
        sts,
    )
}

// -------------------------------------------------- the store, directly

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_and_its_signed_url_are_exactly_as_specified() {
    let fake = fake_google().await;
    let (clock, _) = manual_clock(at("2026-01-02T03:04:05.678Z"));
    let s = store(settings(&fake), clock);
    let png = object("image/png", 300);
    let stored = s.put("alice", vec![png.clone()]).await.unwrap();

    let id = &stored.render_id;
    assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 4);
    let name = format!("renders/{id}/1.png");
    // The upload: one multipart POST, never over an existing object.
    let uploads = fake.uploads();
    assert_eq!(uploads.len(), 1);
    let u = &uploads[0];
    assert_eq!(u.path, format!("/upload/storage/v1/b/{BUCKET}/o"));
    assert_eq!(u.query, "uploadType=multipart&ifGenerationMatch=0");
    assert_eq!(u.auth, format!("Bearer {}", fake_token(1)));
    assert_eq!(u.metadata["name"], name);
    assert_eq!(u.metadata["contentType"], "image/png");
    assert_eq!(
        u.metadata["contentDisposition"],
        "inline; filename=\"pscat-1.png\""
    );
    assert_eq!(u.media_type, "image/png");
    assert_eq!(u.bytes, &png.bytes[..]);

    // The signed URL, at the clock's whole second.
    let (want, sts) = expected_url(&name, DEFAULT_SA, "20260102T030405Z", 1800);
    assert_eq!(stored.links.len(), 1);
    assert_eq!(stored.links[0].download_url, want);
    assert_eq!(stored.links[0].uri, want);
    assert_eq!(stored.expires_at_rfc3339(), "2026-01-02T03:34:05Z");
    let signs = fake.signs();
    assert_eq!(signs.len(), 1);
    assert_eq!(signs[0].payload, sts);
    assert_eq!(
        signs[0].path,
        format!("/v1/projects/-/serviceAccounts/{DEFAULT_SA}:signBlob")
    );
    assert_eq!(signs[0].auth, format!("Bearer {}", fake_token(1)));
    // Nothing to read back: the signed URL is the only way to the bytes.
    assert!(s.read("alice", id, 1).await.is_none());
    assert_eq!(s.name(), "gcs");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_page_render_is_one_render_id_with_one_object_per_page() {
    let fake = fake_google().await;
    let (clock, _) = manual_clock(at("2026-03-04T05:06:07Z"));
    let s = store(settings(&fake), clock);
    let stored = s
        .put(
            "alice",
            vec![
                object("image/svg+xml", 10),
                object("image/svg+xml", 20),
                object("image/svg+xml", 30),
            ],
        )
        .await
        .unwrap();
    let names: Vec<String> = fake
        .uploads()
        .iter()
        .map(|u| u.metadata["name"].as_str().unwrap().to_string())
        .collect();
    let id = &stored.render_id;
    assert_eq!(
        names,
        (1..=3)
            .map(|n| format!("renders/{id}/{n}.svg"))
            .collect::<Vec<_>>()
    );
    for (n, (link, u)) in stored.links.iter().zip(fake.uploads()).enumerate() {
        assert_eq!(u.bytes.len(), 10 * (n + 1));
        assert_eq!(
            u.metadata["contentDisposition"],
            format!("inline; filename=\"pscat-{}.svg\"", n + 1)
        );
        let (want, _) = expected_url(&names[n], DEFAULT_SA, "20260304T050607Z", 1800);
        assert_eq!(link.download_url, want);
    }
    // A PDF is `.pdf` and `application/pdf`.
    let pdf = s
        .put("alice", vec![object("application/pdf", 5)])
        .await
        .unwrap();
    let last = fake.uploads().pop().unwrap();
    assert_eq!(
        last.metadata["name"],
        format!("renders/{}/1.pdf", pdf.render_id)
    );
    assert_eq!(last.media_type, "application/pdf");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_is_cached_refreshed_before_expiry_and_fetched_once_at_a_time() {
    let fake = fake_google().await;
    let (clock, t) = manual_clock(at("2026-01-01T00:00:00Z"));
    let s = Arc::new(store(settings(&fake), clock));
    // Concurrent first calls: one token fetch, one email fetch.
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.put("alice", vec![object("image/png", 10)]).await.unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fake.fake.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(fake.fake.emails.load(Ordering::SeqCst), 1);
    // Still valid well before expiry (3600 s, refreshed 300 s early).
    *t.lock().unwrap() += Duration::from_secs(3299);
    s.put("alice", vec![object("image/png", 10)]).await.unwrap();
    assert_eq!(fake.fake.tokens.load(Ordering::SeqCst), 1);
    *t.lock().unwrap() += Duration::from_secs(1);
    s.put("alice", vec![object("image/png", 10)]).await.unwrap();
    assert_eq!(fake.fake.tokens.load(Ordering::SeqCst), 2);
    let last = fake.uploads().pop().unwrap();
    assert_eq!(last.auth, format!("Bearer {}", fake_token(2)));
    assert_eq!(fake.signs().pop().unwrap().auth, last.auth);
    // The signer's email is fetched once for the life of the store.
    assert_eq!(fake.fake.emails.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_lived_token_is_refreshed_at_half_its_life() {
    let fake = fake_google().await;
    *fake.fake.expires_in.lock().unwrap() = 100;
    let (clock, t) = manual_clock(at("2026-01-01T00:00:00Z"));
    let s = store(settings(&fake), clock);
    s.put("a", vec![object("image/png", 1)]).await.unwrap();
    *t.lock().unwrap() += Duration::from_secs(49);
    s.put("a", vec![object("image/png", 1)]).await.unwrap();
    assert_eq!(fake.fake.tokens.load(Ordering::SeqCst), 1);
    *t.lock().unwrap() += Duration::from_secs(1);
    s.put("a", vec![object("image/png", 1)]).await.unwrap();
    assert_eq!(fake.fake.tokens.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_signer_is_used_and_no_email_is_fetched() {
    let fake = fake_google().await;
    let signer = "url-signer@example-project.iam.gserviceaccount.com";
    let (clock, _) = manual_clock(at("2026-05-06T07:08:09Z"));
    let s = store(
        GcsSettings {
            signer: Some(signer.into()),
            prefix: "staging/pscat/".into(),
            ttl: Duration::from_secs(600),
            ..settings(&fake)
        },
        clock,
    );
    let stored = s.put("a", vec![object("image/png", 3)]).await.unwrap();
    assert_eq!(fake.fake.emails.load(Ordering::SeqCst), 0);
    assert_eq!(
        fake.signs()[0].path,
        format!("/v1/projects/-/serviceAccounts/{signer}:signBlob")
    );
    let name = format!("staging/pscat/{}/1.png", stored.render_id);
    let (want, _) = expected_url(&name, signer, "20260506T070809Z", 600);
    assert_eq!(stored.links[0].download_url, want);
    assert_eq!(stored.expires_at_rfc3339(), "2026-05-06T07:18:09Z");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quota_counts_uploads_within_the_ttl_per_principal() {
    let fake = fake_google().await;
    let (clock, t) = manual_clock(at("2026-01-01T00:00:00Z"));
    let one = 100 + pscat_mcp::artifacts::RENDER_CHARGE_BYTES;
    let s = store(
        GcsSettings {
            quota: 2 * one,
            ttl: Duration::from_secs(60),
            ..settings(&fake)
        },
        clock,
    );
    s.put("a", vec![object("image/png", 100)]).await.unwrap();
    s.put("a", vec![object("image/png", 100)]).await.unwrap();
    let e = s
        .put("a", vec![object("image/png", 100)])
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ResultsBudgetExceeded);
    let input = e.input.as_deref().unwrap();
    assert_eq!(input.code, "artifact_quota_bytes");
    assert_eq!(input.limit_bytes, Some(2 * one as u64));
    assert_eq!(input.reached_bytes, Some(3 * one as u64));
    // Refused before any upload.
    assert_eq!(fake.uploads().len(), 2);
    // Another principal has its own quota.
    s.put("b", vec![object("image/png", 100)]).await.unwrap();
    // Once the window passes, `a` may upload again.
    *t.lock().unwrap() += Duration::from_secs(60);
    s.put("a", vec![object("image/png", 100)]).await.unwrap();
    assert_eq!(s.charged_bytes("a"), one);
}

/// A failure, as the store reports it.
async fn failure(s: &GcsArtifactStore) -> pscat_mcp::render::RenderError {
    s.put("a", vec![object("image/png", 50)])
        .await
        .expect_err("the put must fail")
}

fn assert_clean(e: &pscat_mcp::render::RenderError, stage: &str) {
    assert_eq!(e.code, ErrorCode::StorageUnavailable, "{e:?}");
    assert!(e.message.contains(stage), "{}", e.message);
    assert!(e.message.contains("retry"), "{}", e.message);
    assert!(e.message.contains("inline"), "{}", e.message);
    for secret in ["http", "ya29", "renders/", "fake failure", BUCKET, "@"] {
        assert!(!e.message.contains(secret), "{secret} in {}", e.message);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_storage_unavailable_without_secrets() {
    let fake = fake_google().await;
    let (clock, _) = manual_clock(at("2026-01-01T00:00:00Z"));
    let s = store(settings(&fake), clock.clone());

    // The metadata server down: nothing uploaded, the reservation refunded.
    fake.set("metadata", Mode::Status(500));
    let e = failure(&s).await;
    assert_clean(&e, "credentials");
    assert!(e.message.contains("500"), "{}", e.message);
    assert!(fake.uploads().is_empty());
    assert_eq!(s.charged_bytes("a"), 0);
    fake.set("metadata", Mode::Ok);

    // Storage refuses (412: the object exists; 500): no signing.
    for status in [412, 500] {
        fake.set("storage", Mode::Status(status));
        let e = failure(&s).await;
        assert_clean(&e, "upload");
        assert!(e.message.contains(&status.to_string()), "{}", e.message);
    }
    assert!(fake.signs().is_empty());
    // An upload was attempted: its charge stays (the object may exist).
    assert_eq!(
        s.charged_bytes("a"),
        2 * (50 + pscat_mcp::artifacts::RENDER_CHARGE_BYTES)
    );
    fake.set("storage", Mode::Ok);

    // IAM refuses to sign.
    fake.set("iam", Mode::Status(403));
    let e = failure(&s).await;
    assert_clean(&e, "signing");
    assert!(e.message.contains("403"), "{}", e.message);
    fake.set("iam", Mode::Ok);

    // Timeouts, each call on its own.
    let quick = store(
        GcsSettings {
            timeout: Duration::from_millis(300),
            ..settings(&fake)
        },
        clock,
    );
    for which in ["metadata", "storage", "iam"] {
        fake.set(which, Mode::Hang);
        let started = Instant::now();
        let e = failure(&quick).await;
        assert!(e.message.contains("timeout"), "{which}: {}", e.message);
        assert_eq!(e.code, ErrorCode::StorageUnavailable);
        assert!(started.elapsed() < Duration::from_secs(5), "{which}");
        fake.set(which, Mode::Ok);
    }
    // Healthy again.
    quick.put("a", vec![object("image/png", 1)]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_listens_is_storage_unavailable() {
    // A port nothing listens on.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let s = store(
        GcsSettings {
            bucket: BUCKET.into(),
            prefix: "renders/".into(),
            signer: None,
            ttl: Duration::from_secs(60),
            quota: 1 << 20,
            timeout: Duration::from_secs(2),
            metadata_endpoint: base.clone(),
            iam_endpoint: base.clone(),
            storage_endpoint: base,
            short_links: None,
        },
        Arc::new(SystemTime::now),
    );
    let e = failure(&s).await;
    assert_clean(&e, "credentials");
    assert!(e.message.contains("transport"), "{}", e.message);
}

// ------------------------------------------------------------ settings

fn check(c: ArtifactConfig) -> Result<(), String> {
    c.check(&LimitsConfig::default(), &JobsConfig::default())
}

#[test]
fn gcs_settings_are_checked_at_startup() {
    assert!(check(ArtifactConfig::default().gcs(BUCKET)).is_ok());
    // The bucket is required.
    let e = check(ArtifactConfig {
        artifact_links: LinksBackend::Gcs,
        ..ArtifactConfig::default()
    })
    .unwrap_err();
    assert!(e.contains("PSCAT_MCP_ARTIFACT_BUCKET"), "{e}");
    assert!(check(ArtifactConfig::default().gcs("Not A Bucket")).is_err());
    // The prefix.
    for bad in ["renders", "/renders/", "a/../b/", "../", "a//", "", "a b/"] {
        let e = check(ArtifactConfig {
            artifact_prefix: bad.into(),
            ..ArtifactConfig::default().gcs(BUCKET)
        })
        .unwrap_err();
        assert!(e.contains("PSCAT_MCP_ARTIFACT_PREFIX"), "{bad}: {e}");
    }
    for ok in ["renders/", "a/b-c_d.e/"] {
        assert!(
            check(ArtifactConfig {
                artifact_prefix: ok.into(),
                ..ArtifactConfig::default().gcs(BUCKET)
            })
            .is_ok(),
            "{ok}"
        );
    }
    // V4 signed URLs last at most 7 days.
    let e = check(ArtifactConfig {
        artifact_url_ttl_secs: 604_801,
        ..ArtifactConfig::default().gcs(BUCKET)
    })
    .unwrap_err();
    assert!(e.contains("604800"), "{e}");
    assert!(
        check(ArtifactConfig {
            artifact_url_ttl_secs: 604_800,
            ..ArtifactConfig::default().gcs(BUCKET)
        })
        .is_ok()
    );
    // The memory store has no such ceiling.
    assert!(
        check(ArtifactConfig {
            artifact_url_ttl_secs: 604_801,
            ..ArtifactConfig::default().memory()
        })
        .is_ok()
    );
    // The signer.
    let e = check(ArtifactConfig {
        artifact_signer: Some("not/an-email".into()),
        ..ArtifactConfig::default().gcs(BUCKET)
    })
    .unwrap_err();
    assert!(e.contains("PSCAT_MCP_ARTIFACT_SIGNER"), "{e}");
    // A quota above the results budget is fine: nothing is held in memory.
    assert!(
        check(ArtifactConfig {
            artifact_quota_bytes: JobsConfig::default().job_results_budget_bytes + 1,
            ..ArtifactConfig::default().gcs(BUCKET)
        })
        .is_ok()
    );
    // The test-only endpoint overrides: https, or http on loopback only.
    for (field, bad) in [
        ("metadata", "http://metadata.example"),
        ("iam", "http://iam.example"),
        ("storage", "http://10.0.0.1:80"),
        ("storage", "https://storage.example/some/path"),
        ("iam", "https://iam.example?x=1"),
    ] {
        let mut c = ArtifactConfig::default().gcs(BUCKET);
        let v = Some(bad.to_string());
        match field {
            "metadata" => c.artifact_gcs_metadata_endpoint = v,
            "iam" => c.artifact_gcs_iam_endpoint = v,
            _ => c.artifact_gcs_storage_endpoint = v,
        }
        let e = check(c).unwrap_err();
        assert!(e.contains("_ENDPOINT"), "{bad}: {e}");
    }
    let mut c = ArtifactConfig::default().gcs(BUCKET);
    c.artifact_gcs_metadata_endpoint = Some("http://127.0.0.1:1".into());
    c.artifact_gcs_iam_endpoint = Some("https://iam.example".into());
    c.artifact_gcs_storage_endpoint = Some("http://localhost:2/".into());
    assert!(check(c).is_ok());
}

/// Run the binary with `args` and `env` and no client; return whether it
/// exited successfully and its stderr.
fn run_server(args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = std::process::Command::new(SERVER);
    for (k, _) in std::env::vars() {
        if k.starts_with("PSCAT_MCP_") || k == "PORT" || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .env("PSCAT_MCP_LOG_FORMAT", "pretty")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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
fn gcs_refuses_to_start_without_a_bucket_or_with_bad_settings() {
    let stdio = ["serve-stdio"];
    let (ok, err) = run_server(&stdio, &[("PSCAT_MCP_ARTIFACT_LINKS", "gcs")]);
    assert!(!ok && err.contains("PSCAT_MCP_ARTIFACT_BUCKET"), "{err}");
    let gcs = [
        ("PSCAT_MCP_ARTIFACT_LINKS", "gcs"),
        ("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET),
    ];
    for (k, v, needle) in [
        (
            "PSCAT_MCP_ARTIFACT_PREFIX",
            "../x/",
            "PSCAT_MCP_ARTIFACT_PREFIX",
        ),
        ("PSCAT_MCP_ARTIFACT_URL_TTL_SECS", "700000", "604800"),
        (
            "PSCAT_MCP_ARTIFACT_GCS_STORAGE_ENDPOINT",
            "http://storage.example",
            "PSCAT_MCP_ARTIFACT_GCS_STORAGE_ENDPOINT",
        ),
    ] {
        let mut env = gcs.to_vec();
        env.push((k, v));
        let (ok, err) = run_server(&stdio, &env);
        assert!(!ok && err.contains(needle), "{k}={v}: {err}");
    }
}

#[test]
fn gcs_with_the_none_auth_method_is_refused() {
    let http = ["serve-http", "--bind", "127.0.0.1", "--port", "0"];
    for auth in [
        vec![("PSCAT_MCP_AUTH", "none")],
        vec![
            ("PSCAT_MCP_AUTH", "bearer,none"),
            ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
        ],
    ] {
        let mut env = auth.clone();
        env.extend([
            ("PSCAT_MCP_ARTIFACT_LINKS", "gcs"),
            ("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET),
        ]);
        let (ok, err) = run_server(&http, &env);
        assert!(!ok, "{auth:?}: {err}");
        assert!(
            err.contains("cannot be combined")
                && err.contains("PSCAT_MCP_ARTIFACT_LINKS=gcs")
                && err.contains("signed URLs"),
            "{auth:?}: {err}"
        );
        assert!(!err.contains("listening"), "{err}");
    }
}

// ------------------------------------------- through the tools, in-process

fn worker() -> WorkerConfig {
    WorkerConfig {
        worker_exe: Some(SERVER.into()),
        ..WorkerConfig::default()
    }
}

fn gcs_config(fake: &FakeServer) -> ArtifactConfig {
    ArtifactConfig {
        artifact_gcs_metadata_endpoint: Some(fake.base.clone()),
        artifact_gcs_iam_endpoint: Some(fake.base.clone()),
        artifact_gcs_storage_endpoint: Some(fake.base.clone()),
        artifact_gcs_timeout_ms: 5000,
        ..ArtifactConfig::default().gcs(BUCKET)
    }
}

fn service(artifacts: ArtifactConfig) -> Arc<RenderService> {
    pscat_mcp::build_service_with(
        LimitsConfig::default(),
        JobsConfig::default().enabled(),
        artifacts,
        &worker(),
    )
    .expect("service")
}

async fn connect(service: Arc<RenderService>) -> Client {
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

fn pages(n: usize) -> String {
    (0..n)
        .map(|i| format!("{} setgray 0 0 30 20 rectfill showpage ", i as f64 / 10.0))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn url_delivery_returns_https_signed_urls_in_every_place() {
    let fake = fake_google().await;
    let svc = service(gcs_config(&fake));
    assert_eq!(svc.artifacts().unwrap().name(), "gcs");
    let client = connect(svc).await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(3), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    assert_eq!(r["isError"], false, "{r}");
    let sc = &r["structuredContent"];
    let id = sc["render_id"].as_str().unwrap();
    let artifacts = sc["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 3);
    let links = blocks_of(&r, "resource_link");
    assert_eq!(links.len(), 3);
    assert!(blocks_of(&r, "image").is_empty());
    let uploads = fake.uploads();
    assert_eq!(uploads.len(), 3);
    let text = text_of(&r);
    for (n, (a, link)) in artifacts.iter().zip(&links).enumerate() {
        let url = a["download_url"].as_str().unwrap();
        assert!(
            url.starts_with(&format!(
                "https://storage.googleapis.com/{BUCKET}/renders/{id}/{}.png?X-Goog-Algorithm=GOOG4-RSA-SHA256&",
                n + 1
            )),
            "{url}"
        );
        assert!(url.contains("&X-Goog-Expires=1800&"), "{url}");
        assert_eq!(link["uri"], url);
        assert_eq!(link["mimeType"], "image/png");
        assert_eq!(a["returned_as"], "link");
        assert!(a["expires_at"].as_str().unwrap().ends_with('Z'));
        assert!(text.contains(url), "{text}");
        // The uploaded bytes are the page's PNG.
        assert!(uploads[n].bytes.starts_with(b"\x89PNG"));
        assert_eq!(uploads[n].bytes.len() as u64, a["bytes"].as_u64().unwrap());
    }
    assert!(text.contains("signed URLs"), "{text}");
    assert!(!text.contains("pscat://"), "{text}");
    // `resources/read` serves nothing for a signed URL.
    let url = artifacts[0]["download_url"].as_str().unwrap();
    let read = client
        .read_resource(ReadResourceRequestParams::new(url))
        .await;
    let err = read.expect_err("no resource for a signed URL").to_string();
    assert!(!err.contains("X-Goog"), "the error echoes the link: {err}");

    // `both`: the bytes inline and the links; `inline`: no upload at all.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(1), "width": 30, "height": 20, "delivery": "both"}),
    )
    .await;
    assert_eq!(blocks_of(&r, "image").len(), 1);
    assert_eq!(blocks_of(&r, "resource_link").len(), 1);
    let before = fake.uploads().len();
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(1), "width": 30, "height": 20}),
    )
    .await;
    assert!(r["structuredContent"].get("render_id").is_none());
    assert_eq!(fake.uploads().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_sheet_and_handwriting_get_signed_urls_too() {
    let fake = fake_google().await;
    let client = connect(service(gcs_config(&fake))).await;
    let r = call(
        &client,
        "handwrite",
        json!({"text": "hi", "width": 120, "delivery": "url"}),
    )
    .await;
    let url = r["structuredContent"]["artifacts"][0]["download_url"]
        .as_str()
        .unwrap();
    assert!(url.starts_with("https://storage.googleapis.com/"), "{url}");
    let r = call(
        &client,
        "review_sheet",
        json!({"entries": [{"source": "0 0 10 10 rectfill", "width": 20, "height": 20}],
               "delivery": "url"}),
    )
    .await;
    assert_eq!(r["isError"], false, "{r}");
    let url = r["structuredContent"]["download_url"].as_str().unwrap();
    assert!(url.starts_with("https://storage.googleapis.com/"), "{url}");
    assert_eq!(r["structuredContent"]["returned_as"], "link");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upload_is_an_error_for_url_and_inline_for_both_and_auto() {
    let fake = fake_google().await;
    let client = connect(service(ArtifactConfig {
        artifact_threshold_bytes: 0,
        ..gcs_config(&fake)
    }))
    .await;
    fake.set("storage", Mode::Status(503));
    let args = |delivery: &str| json!({"source": pages(1), "width": 30, "height": 20, "delivery": delivery});
    // `url`: the structured, retryable error.
    let r = call(&client, "render_postscript", args("url")).await;
    assert_eq!(r["isError"], true, "{r}");
    let err = &r["structuredContent"]["error"];
    assert_eq!(err["code"], "storage_unavailable");
    let msg = err["message"].as_str().unwrap();
    assert!(msg.contains("retry") && msg.contains("inline"), "{msg}");
    assert!(blocks_of(&r, "image").is_empty());
    // `both` and `auto` (over a zero threshold): inline, with a note.
    for delivery in ["both", "auto"] {
        let r = call(&client, "render_postscript", args(delivery)).await;
        assert_eq!(r["isError"], false, "{delivery}: {r}");
        assert_eq!(blocks_of(&r, "image").len(), 1, "{delivery}");
        assert!(blocks_of(&r, "resource_link").is_empty(), "{delivery}");
        let sc = &r["structuredContent"];
        assert!(sc.get("render_id").is_none(), "{delivery}");
        assert_eq!(sc["artifacts"][0]["returned_as"], "image");
        assert!(sc["artifacts"][0].get("download_url").is_none());
        let note = sc["delivery_note"].as_str().unwrap();
        assert!(note.contains("storage_unavailable"), "{note}");
        assert!(text_of(&r).contains(note), "{delivery}");
    }
    // The sheet too.
    let r = call(
        &client,
        "review_sheet",
        json!({"entries": [{"source": "0 0 10 10 rectfill", "width": 20, "height": 20}],
               "delivery": "both"}),
    )
    .await;
    assert_eq!(r["isError"], false, "{r}");
    assert!(r["structuredContent"]["delivery_note"].is_string(), "{r}");
    assert_eq!(blocks_of(&r, "image").len(), 1);
    // Signing failures too.
    fake.set("storage", Mode::Ok);
    fake.set("iam", Mode::Status(403));
    let r = call(&client, "render_postscript", args("url")).await;
    assert_eq!(
        r["structuredContent"]["error"]["code"], "storage_unavailable",
        "{r}"
    );
    let r = call(&client, "render_postscript", args("both")).await;
    assert!(r["structuredContent"]["delivery_note"].is_string(), "{r}");
    // A quota refusal is never a fallback.
    fake.set("iam", Mode::Ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quota_refuses_url_and_both_alike() {
    let fake = fake_google().await;
    let config = gcs_config(&fake);
    // A store with a small quota (the config check would want room for one
    // largest output): room for one small render.
    let settings = GcsSettings {
        quota: 1500,
        ..pscat_mcp::gcs::GcsSettings::from_config(&config).unwrap()
    };
    let store = Arc::new(GcsArtifactStore::new(settings, Arc::new(SystemTime::now)).unwrap());
    let svc = Arc::try_unwrap(service(config.clone()))
        .ok()
        .expect("one owner")
        .with_artifact_store(config, store);
    let client = connect(Arc::new(svc)).await;
    let args = |delivery: &str| json!({"source": "0 0 1 1 rectfill", "width": 20, "height": 20, "delivery": delivery});
    let r = call(&client, "render_postscript", args("url")).await;
    assert_eq!(r["isError"], false, "{r}");
    for delivery in ["url", "both"] {
        let r = call(&client, "render_postscript", args(delivery)).await;
        assert_eq!(r["isError"], true, "{delivery}: {r}");
        let err = &r["structuredContent"]["error"];
        assert_eq!(err["code"], "results_budget_exceeded");
        assert_eq!(err["input"]["code"], "artifact_quota_bytes");
        assert_eq!(err["input"]["limit_bytes"], 1500);
    }
    assert_eq!(fake.uploads().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_result_falls_back_inline_when_both_fails() {
    let fake = fake_google().await;
    let client = connect(service(gcs_config(&fake))).await;
    fake.set("storage", Mode::Status(500));
    let r = call(
        &client,
        "start_render",
        json!({"source": pages(1), "width": 30, "height": 20, "delivery": "both"}),
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
    let r = call(&client, "render_result", json!({"job_id": id})).await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(blocks_of(&r, "image").len(), 1);
    assert!(r["structuredContent"]["delivery_note"].is_string(), "{r}");
    // Storage back: a `url` job gets its links.
    fake.set("storage", Mode::Ok);
    let r = call(
        &client,
        "start_render",
        json!({"source": pages(1), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    let id = r["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    loop {
        let s = call(&client, "render_status", json!({"job_id": id})).await;
        if s["structuredContent"]["state"] == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "{s}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = call(&client, "render_result", json!({"job_id": id})).await;
    let url = r["structuredContent"]["artifacts"][0]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"));
    assert!(url.starts_with("https://storage.googleapis.com/"), "{url}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tool_descriptions_describe_signed_urls() {
    let fake = fake_google().await;
    let client = connect(service(gcs_config(&fake))).await;
    let tools = client.list_all_tools().await.unwrap();
    let render = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    let d = render.description.as_deref().unwrap_or("");
    assert!(d.contains("signed URLs"), "{d}");
    assert!(d.contains("storage_unavailable"), "{d}");
    assert!(!d.contains("pscat://render"), "{d}");
}

// ------------------------------------------ logs and capture, the binary

struct Server {
    child: tokio::process::Child,
    base: String,
    logs: tokio::task::JoinHandle<String>,
}

async fn start_http(fake: &FakeServer, rust_log: Option<&str>, capture: &str) -> Server {
    start_http_with(fake, rust_log, capture, &[]).await
}

async fn start_http_with(
    fake: &FakeServer,
    rust_log: Option<&str>,
    capture: &str,
    env: &[(&str, &str)],
) -> Server {
    let mut cmd = tokio::process::Command::new(SERVER);
    for (k, _) in std::env::vars() {
        if k.starts_with("PSCAT_MCP_") || k == "PORT" || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    cmd.args(["serve-http", "--bind", "127.0.0.1", "--port", "0"])
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env("PSCAT_MCP_ARTIFACT_LINKS", "gcs")
        .env("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET)
        .env("PSCAT_MCP_ARTIFACT_GCS_METADATA_ENDPOINT", &fake.base)
        .env("PSCAT_MCP_ARTIFACT_GCS_IAM_ENDPOINT", &fake.base)
        .env("PSCAT_MCP_ARTIFACT_GCS_STORAGE_ENDPOINT", &fake.base)
        .env("PSCAT_MCP_CAPTURE_FILE", capture)
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
    assert!(!addr.is_empty(), "serve-http did not start: {collected}");
    assert!(
        collected.contains("\"artifact_links\":\"gcs\""),
        "{collected}"
    );
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

async fn post(http: &reqwest::Client, url: &str, body: &Value, name: Option<&str>) -> Value {
    let mut req = http
        .post(url)
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18");
    if let Some(n) = name {
        req = req
            .header("mcp-method", "resources/read")
            .header("mcp-name", n);
    }
    let resp = req.body(body.to_string()).send().await.expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    serde_json::from_str(&resp.text().await.unwrap()).expect("json")
}

fn render(id: u64, source: &str, delivery: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
        "name": "render_postscript",
        "arguments": {"source": source, "width": 40, "height": 40, "delivery": delivery},
    }})
}

/// Render with links, send the signed URL back inside a request, then make
/// the upload fail; return the URL, the logs and the capture file.
async fn signed_url_round_trip(rust_log: Option<&str>) -> (String, String, String) {
    let fake = fake_google().await;
    let capture = std::env::temp_dir().join(format!(
        "pscat-mcp-gcs-capture-{}.jsonl",
        uuid::Uuid::new_v4().simple()
    ));
    let capture_path = capture.to_string_lossy().into_owned();
    let mut server = start_http(&fake, rust_log, &capture_path).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let v = post(
        &http,
        &server.base,
        &render(1, "0.3 setgray 0 0 40 40 rectfill", "url"),
        None,
    )
    .await;
    let url = v["result"]["structuredContent"]["artifacts"][0]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no link: {v}"))
        .to_string();
    assert!(url.contains("X-Goog-Signature="), "{url}");
    // A client sends the link back: in a tool argument and in a header.
    let source = format!("% {url}\n0 0 1 1 rectfill");
    post(&http, &server.base, &render(2, &source, "inline"), None).await;
    let read = json!({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
                      "params": {"uri": url}});
    let v = post(&http, &server.base, &read, Some(&url)).await;
    assert!(v.get("error").is_some(), "{v}");
    assert!(!v.to_string().contains("X-Goog-Signature"), "{v}");
    // The upload fails: `both` falls back, and the WARN line has codes only.
    fake.set("storage", Mode::Status(500));
    let v = post(
        &http,
        &server.base,
        &render(3, "0 0 1 1 rectfill", "both"),
        None,
    )
    .await;
    assert!(
        v["result"]["structuredContent"]["delivery_note"].is_string(),
        "{v}"
    );
    server.child.kill().await.unwrap();
    let logs = tokio::time::timeout(Duration::from_secs(30), server.logs)
        .await
        .expect("stderr closed")
        .unwrap();
    let captured = std::fs::read_to_string(&capture).unwrap_or_default();
    let _ = std::fs::remove_file(&capture);
    (url, logs, captured)
}

fn no_secrets(url: &str, logs: &str, captured: &str) {
    let signature = url.split("X-Goog-Signature=").nth(1).unwrap();
    let render_id = url
        .split("/renders/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    for (what, text) in [("logs", logs), ("capture file", captured)] {
        assert!(!text.contains(signature), "the signature is in the {what}");
        assert!(!text.contains("X-Goog-Signature"), "{what}: {text}");
        assert!(!text.contains("X-Goog-Credential"), "{what}: {text}");
        assert!(!text.contains("ya29."), "a token is in the {what}");
        assert!(
            !text.contains("fake failure"),
            "a response body is in the {what}"
        );
    }
    assert!(!logs.contains(render_id), "the render id is in the logs");
    assert!(!logs.contains("storage.googleapis.com"), "{logs}");
    // The failure is one WARN line with codes only.
    let warn: Vec<Value> = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["message"] == "artifact links failed")
        .collect();
    assert_eq!(warn.len(), 1, "{logs}");
    let f = &warn[0];
    assert_eq!(f["level"], "WARN");
    assert_eq!(f["code"], "storage_unavailable");
    assert_eq!(f["stage"], "upload");
    assert_eq!(f["status"], 500);
    // The capture recorded the requests, with the URL's query redacted
    // (in the body and in `Mcp-Name`).
    assert!(captured.contains("?[redacted]"), "{captured}");
    assert!(
        captured.contains("\"mcp-name\":\"https://storage.googleapis.com/"),
        "{captured}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_urls_never_reach_the_logs_or_the_capture_file() {
    let (url, logs, captured) = signed_url_round_trip(None).await;
    no_secrets(&url, &logs, &captured);
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_urls_stay_out_of_the_logs_at_trace() {
    let (url, logs, captured) = signed_url_round_trip(Some("info,pscat_mcp=trace")).await;
    no_secrets(&url, &logs, &captured);
}

// ---------------------------------------------- short links (#163)

const ORIGIN: &str = "https://mcp.example.test";
const SECRET: &[u8] = b"a link secret for the tests, 32 bytes or more";
const OLD_SECRET: &[u8] = b"the previous link secret, also long enough";

fn short_settings(fake: &FakeServer, current: &[u8], previous: Option<&[u8]>) -> GcsSettings {
    GcsSettings {
        short_links: Some(ShortLinks::from_secrets(ORIGIN, current, previous).unwrap()),
        ..settings(fake)
    }
}

/// The router of a service whose store is `store`.
fn app_with(fake: &FakeServer, store: Arc<GcsArtifactStore>) -> axum::Router {
    let config = gcs_config(fake);
    let svc = Arc::try_unwrap(service(config.clone()))
        .ok()
        .expect("one owner")
        .with_artifact_store(config, store);
    pscat_mcp::http::router(
        Arc::new(svc),
        Arc::new(BearerTokenAuth::new([TOKEN])),
        StreamableHttpServerConfig::default(),
    )
}

struct Answer {
    status: u16,
    headers: http::HeaderMap,
    body: String,
}

/// `method` `target` (a path and query) on `app`, with no `Authorization`.
async fn open(app: &axum::Router, method: &str, target: &str) -> Answer {
    let req = http::Request::builder()
        .method(method)
        .uri(target)
        .header("host", "mcp.example.test")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// The path and query of a short link.
fn target_of(link: &str) -> &str {
    link.strip_prefix(ORIGIN)
        .unwrap_or_else(|| panic!("not on {ORIGIN}: {link}"))
}

fn stamp(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

fn header_of<'a>(a: &'a Answer, name: &str) -> &'a str {
    a.headers
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_link_redirects_to_a_v4_url_minted_when_opened() {
    let fake = fake_google().await;
    let t0 = at("2026-01-02T03:04:05.678Z");
    let (clock, t) = manual_clock(t0);
    let s = Arc::new(store(short_settings(&fake, SECRET, None), clock));
    assert!(s.serves_short_links());
    let stored = s
        .put(
            "alice",
            vec![object("image/png", 30), object("image/svg+xml", 20)],
        )
        .await
        .unwrap();
    let id = stored.render_id.clone();
    // Uploaded as before, and nothing signed yet.
    assert_eq!(fake.uploads().len(), 2);
    assert!(fake.signs().is_empty());
    assert_eq!(fake.fake.emails.load(Ordering::SeqCst), 0);
    // The links: short, on the origin, expiring when the result says.
    let e = unix(at("2026-01-02T03:34:05Z"));
    assert_eq!(stored.expires_at_rfc3339(), "2026-01-02T03:34:05Z");
    for (n, (link, ext)) in stored.links.iter().zip(["png", "svg"]).enumerate() {
        let prefix = format!("{ORIGIN}/a/{id}/{}.{ext}?e={e}&s=", n + 1);
        let token = link.download_url.strip_prefix(&prefix).unwrap_or_else(|| {
            panic!("{}", link.download_url);
        });
        assert_eq!(token.len(), 22, "{token}");
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        );
        assert_eq!(link.uri, link.download_url);
        assert_eq!(link.scheme, LinkScheme::Short);
        assert!(link.uri.len() < 110, "{}", link.uri);
    }

    // Opened 100 s later: a V4 URL for that object, minted now, for 60 s.
    *t.lock().unwrap() += Duration::from_secs(100);
    let click = whole(*t.lock().unwrap());
    let app = app_with(&fake, s.clone());
    let link = &stored.links[0].download_url;
    let (want, sts) = expected_url(
        &format!("renders/{id}/1.png"),
        DEFAULT_SA,
        &stamp(click),
        60,
    );
    for (method, target) in [
        ("GET", target_of(link).to_string()),
        ("HEAD", target_of(link).to_string()),
        ("GET", format!("{}&utm_source=chatgpt.com", target_of(link))),
        ("GET", format!("{}?utm_source=chatgpt.com", target_of(link))),
        ("GET", {
            let (p, q) = target_of(link).split_once('?').unwrap();
            format!("{p}?utm_source=chatgpt.com&utm_medium=x&{q}")
        }),
    ] {
        let a = open(&app, method, &target).await;
        assert_eq!(a.status, 302, "{method} {target}: {}", a.body);
        assert_eq!(header_of(&a, "location"), want, "{method} {target}");
        assert_eq!(header_of(&a, "referrer-policy"), "no-referrer");
        assert_eq!(header_of(&a, "cache-control"), "no-store");
        assert!(a.body.is_empty(), "{}", a.body);
    }
    let signs = fake.signs();
    assert_eq!(signs.len(), 5);
    assert!(signs.iter().all(|c| c.payload == sts));
    // The second object, the same way.
    let (want2, _) = expected_url(
        &format!("renders/{id}/2.svg"),
        DEFAULT_SA,
        &stamp(click),
        60,
    );
    let a = open(&app, "GET", target_of(&stored.links[1].download_url)).await;
    assert_eq!(header_of(&a, "location"), want2);

    // Ten seconds before the link expires, the URL works for those ten.
    *t.lock().unwrap() = SystemTime::UNIX_EPOCH + Duration::from_secs(e - 10);
    let (want, _) = expected_url(
        &format!("renders/{id}/1.png"),
        DEFAULT_SA,
        &stamp(SystemTime::UNIX_EPOCH + Duration::from_secs(e - 10)),
        10,
    );
    let a = open(&app, "GET", target_of(link)).await;
    assert_eq!(header_of(&a, "location"), want);
    // At `e`, gone.
    *t.lock().unwrap() = SystemTime::UNIX_EPOCH + Duration::from_secs(e);
    let before = fake.signs().len();
    let a = open(&app, "GET", target_of(link)).await;
    assert_eq!(a.status, 410, "{}", a.body);
    assert_eq!(a.body, "link expired\n");
    assert!(a.headers.get("location").is_none());
    assert_eq!(fake.signs().len(), before);
}

fn whole(t: SystemTime) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(unix(t))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_or_malformed_short_link_gives_no_redirect() {
    let fake = fake_google().await;
    let (clock, t) = manual_clock(at("2026-02-03T04:05:06Z"));
    let s = Arc::new(store(short_settings(&fake, SECRET, None), clock));
    let stored = s.put("alice", vec![object("image/png", 30)]).await.unwrap();
    let id = &stored.render_id;
    // Another real render: its id is valid, but not under this token.
    let other = s.put("alice", vec![object("image/png", 30)]).await.unwrap();
    let app = app_with(&fake, s.clone());
    let link = target_of(&stored.links[0].download_url).to_string();
    let (path, query) = link.split_once('?').unwrap();
    let token = query.split("s=").nth(1).unwrap().to_string();
    let e: u64 = query
        .strip_prefix("e=")
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let flip = |i: usize| {
        let mut b: Vec<u8> = token.bytes().collect();
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        String::from_utf8(b).unwrap()
    };
    let mut cases: Vec<(String, u16)> = Vec::new();
    for i in [0, 10, 21] {
        cases.push((format!("{path}?e={e}&s={}", flip(i)), 403));
    }
    cases.extend([
        // Another render id, page, format.
        (format!("/a/{}/1.png?{query}", other.render_id), 403),
        (format!("/a/{id}/2.png?{query}"), 403),
        (format!("/a/{id}/1.svg?{query}"), 403),
        (format!("/a/{id}/1.pdf?{query}"), 403),
        // A changed expiry, later or earlier.
        (format!("{path}?e={}&s={token}", e + 3600), 403),
        (format!("{path}?e={}&s={token}", e - 1), 403),
        // No token, no expiry, nothing.
        (format!("{path}?e={e}"), 403),
        (format!("{path}?s={token}"), 403),
        (path.to_string(), 403),
        (format!("{path}?e={e}&s="), 403),
        (format!("{path}?e={e}&s={}", &token[..21]), 403),
        // Paths that are not a link: never another object.
        (format!("/a/{}/1.png?{query}", id.to_uppercase()), 404),
        (format!("/a/{}/1.png?{query}", id.replace('-', "")), 404),
        (format!("/a/{id}/01.png?{query}"), 404),
        (format!("/a/{id}/1.png/../../x/1.png?{query}"), 404),
        (format!("/a/{id}/..%2F1.png?{query}"), 404),
        (format!("/a/{id}%2F1.png?{query}"), 404),
        (format!("/a/{id}/1.png.exe?{query}"), 404),
        (format!("/a/{id}/1?{query}"), 404),
        (format!("/a/{id}?{query}"), 404),
        (format!("/a/x/{id}/1.png?{query}"), 404),
    ]);
    for (target, status) in &cases {
        let a = open(&app, "GET", target).await;
        assert_eq!(a.status, *status, "{target}: {}", a.body);
        assert!(a.headers.get("location").is_none(), "{target}");
        let want = if *status == 403 { "forbidden\n" } else { "" };
        if *status == 403 {
            assert_eq!(a.body, want, "{target}");
        } else {
            assert!(
                a.body.is_empty() || a.body == "not found\n",
                "{target}: {}",
                a.body
            );
        }
        assert!(!a.body.contains(id.as_str()) && !a.body.contains(&token));
    }
    assert!(fake.signs().is_empty(), "a refused link reached IAM");
    // POST and the like: 405, nothing signed.
    for method in ["POST", "PUT", "DELETE"] {
        let a = open(&app, method, &link).await;
        assert_eq!(a.status, 405, "{method}");
        assert_eq!(header_of(&a, "allow"), "GET, HEAD");
        assert!(a.headers.get("location").is_none());
    }
    assert!(fake.signs().is_empty());
    // The link itself still works; IAM down is a 503 with no detail.
    *t.lock().unwrap() += Duration::from_secs(1);
    assert_eq!(open(&app, "GET", &link).await.status, 302);
    fake.set("iam", Mode::Status(500));
    let a = open(&app, "GET", &link).await;
    assert_eq!(a.status, 503);
    assert_eq!(a.body, "unavailable, retry\n");
    assert!(a.headers.get("location").is_none());
    fake.set("iam", Mode::Ok);
    fake.set("metadata", Mode::Status(500));
    // The token is cached; a fresh store must fetch it and fails.
    let fresh = Arc::new(store(
        short_settings(&fake, SECRET, None),
        Arc::new(move || at("2026-02-03T04:05:07Z")),
    ));
    let a = open(&app_with(&fake, fresh), "GET", &link).await;
    assert_eq!(a.status, 503);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_previous_secret_still_works_after_a_rotation() {
    let fake = fake_google().await;
    let (clock, _) = manual_clock(at("2026-03-01T00:00:00Z"));
    // Links made before the rotation, under the old secret.
    let before = store(short_settings(&fake, OLD_SECRET, None), clock.clone());
    let stored = before.put("a", vec![object("image/png", 5)]).await.unwrap();
    let link = target_of(&stored.links[0].download_url).to_string();
    // After: the new secret is current, the old one previous.
    let rotated = Arc::new(store(
        short_settings(&fake, SECRET, Some(OLD_SECRET)),
        clock.clone(),
    ));
    let a = open(&app_with(&fake, rotated.clone()), "GET", &link).await;
    assert_eq!(a.status, 302, "{}", a.body);
    assert!(header_of(&a, "location").contains(&format!("/renders/{}/1.png?", stored.render_id)));
    // New links are under the new secret, which the old store refuses.
    let fresh = rotated
        .put("a", vec![object("image/png", 5)])
        .await
        .unwrap();
    assert_ne!(
        fresh.links[0].uri.split("s=").nth(1),
        stored.links[0].uri.split("s=").nth(1)
    );
    // Once the previous secret is dropped, the old links stop.
    let dropped = Arc::new(store(short_settings(&fake, SECRET, None), clock));
    let app = app_with(&fake, dropped);
    assert_eq!(open(&app, "GET", &link).await.status, 403);
    assert_eq!(
        open(&app, "GET", target_of(&fresh.links[0].uri))
            .await
            .status,
        302
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_short_links_there_is_no_a_route() {
    let fake = fake_google().await;
    let s = Arc::new(store(settings(&fake), Arc::new(SystemTime::now)));
    assert!(!s.serves_short_links());
    let stored = s.put("a", vec![object("image/png", 5)]).await.unwrap();
    assert!(
        stored.links[0]
            .uri
            .starts_with("https://storage.googleapis.com/")
    );
    // A token minted with any secret leads nowhere here.
    let other = ShortLinks::from_secrets(ORIGIN, SECRET, None).unwrap();
    let link = other.link(
        &pscat_mcp::short_link::LinkTarget {
            render_id: stored.render_id.clone(),
            n: 1,
            ext: "png",
        },
        unix(SystemTime::now()) + 600,
    );
    let a = open(&app_with(&fake, s), "GET", target_of(&link)).await;
    assert_eq!(a.status, 404);
    assert_eq!(fake.signs().len(), 1);
}

/// A secret file in the temp directory, removed on drop.
struct SecretFile(std::path::PathBuf);

impl SecretFile {
    fn new(bytes: &[u8]) -> Self {
        let p = std::env::temp_dir().join(format!(
            "pscat-mcp-link-secret-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&p, bytes).unwrap();
        SecretFile(p)
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn url_delivery_carries_the_short_link_in_every_place() {
    let fake = fake_google().await;
    let secret = SecretFile::new(b"0123456789abcdef0123456789abcdef\n");
    let config = ArtifactConfig {
        artifact_public_origin: Some(format!("{ORIGIN}/")),
        artifact_link_secret_file: Some(secret.0.clone()),
        ..gcs_config(&fake)
    };
    let svc = service(config);
    assert!(svc.artifacts().unwrap().serves_short_links());
    let client = connect(svc.clone()).await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(2), "width": 30, "height": 20, "delivery": "url"}),
    )
    .await;
    assert_eq!(r["isError"], false, "{r}");
    let sc = &r["structuredContent"];
    let id = sc["render_id"].as_str().unwrap();
    let links = blocks_of(&r, "resource_link");
    let text = text_of(&r);
    for (n, (a, link)) in sc["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&links)
        .enumerate()
    {
        let url = a["download_url"].as_str().unwrap();
        let expires = chrono::DateTime::parse_from_rfc3339(a["expires_at"].as_str().unwrap())
            .unwrap()
            .timestamp();
        assert!(
            url.starts_with(&format!("{ORIGIN}/a/{id}/{}.png?e={expires}&s=", n + 1)),
            "{url}"
        );
        assert_eq!(link["uri"], url);
        assert!(text.contains(url), "{text}");
    }
    assert!(text.contains("redirects to a"), "{text}");
    assert!(!text.contains("storage.googleapis.com"), "{text}");
    assert!(fake.signs().is_empty());
    // The tool description says what the links are.
    let tools = client.list_all_tools().await.unwrap();
    let d = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .and_then(|t| t.description.as_deref())
        .unwrap_or("");
    assert!(d.contains("short https links"), "{d}");
    // And the router redeems it.
    let url = sc["artifacts"][0]["download_url"].as_str().unwrap();
    let app = pscat_mcp::http::router(
        svc,
        Arc::new(BearerTokenAuth::new([TOKEN])),
        StreamableHttpServerConfig::default(),
    );
    let a = open(&app, "GET", target_of(url)).await;
    assert_eq!(a.status, 302, "{}", a.body);
    assert!(
        header_of(&a, "location").starts_with(&format!(
            "https://storage.googleapis.com/{BUCKET}/renders/{id}/1.png?"
        )),
        "{}",
        header_of(&a, "location")
    );
    assert!(header_of(&a, "location").contains("&X-Goog-Expires="));
    // Inline output is untouched.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": pages(1), "width": 30, "height": 20}),
    )
    .await;
    assert!(r["structuredContent"].get("render_id").is_none());
    assert_eq!(blocks_of(&r, "image").len(), 1);
}

#[test]
fn short_link_settings_are_checked_at_startup() {
    let http = ["serve-http", "--bind", "127.0.0.1", "--port", "0"];
    let gcs = [
        ("PSCAT_MCP_ARTIFACT_LINKS", "gcs"),
        ("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET),
        ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
    ];
    let with = |extra: &[(&str, &str)]| {
        let mut env = gcs.to_vec();
        env.extend_from_slice(extra);
        run_server(&http, &env)
    };
    // A public origin needs a secret.
    let (ok, err) = with(&[("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN)]);
    assert!(!ok, "{err}");
    assert!(
        err.contains("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE")
            && err.contains("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN"),
        "{err}"
    );
    assert!(!err.contains("listening"), "{err}");
    // A short secret is refused, and never echoed.
    let short = SecretFile::new(b"too-short-secret-value\n");
    let (ok, err) = with(&[
        ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN),
        ("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", short.path()),
    ]);
    assert!(!ok && err.contains("32"), "{err}");
    assert!(!err.contains("too-short-secret-value"), "{err}");
    // A missing file.
    let (ok, err) = with(&[
        ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN),
        (
            "PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE",
            "/nonexistent/pscat-secret",
        ),
    ]);
    assert!(
        !ok && err.contains("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE"),
        "{err}"
    );
    // A previous secret alone.
    let good = SecretFile::new(&[b'k'; 40]);
    let (ok, err) = with(&[("PSCAT_MCP_ARTIFACT_LINK_SECRET_PREVIOUS_FILE", good.path())]);
    assert!(
        !ok && err.contains("PSCAT_MCP_ARTIFACT_LINK_SECRET_PREVIOUS_FILE"),
        "{err}"
    );
    // A bad origin.
    for bad in [
        "http://mcp.example.test",
        "https://mcp.example.test/mcp",
        "https://mcp.example.test?x=1",
    ] {
        let (ok, err) = with(&[
            ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", bad),
            ("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", good.path()),
        ]);
        assert!(
            !ok && err.contains("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN"),
            "{bad}: {err}"
        );
    }
    // serve-stdio serves no /a/ route.
    let (ok, err) = run_server(
        &["serve-stdio"],
        &[
            ("PSCAT_MCP_ARTIFACT_LINKS", "gcs"),
            ("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET),
            ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN),
            ("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", good.path()),
        ],
    );
    assert!(!ok && err.contains("serve-stdio"), "{err}");
    // `none` stays refused with gcs links, short or not.
    let (ok, err) = run_server(
        &http,
        &[
            ("PSCAT_MCP_AUTH", "none"),
            ("PSCAT_MCP_ARTIFACT_LINKS", "gcs"),
            ("PSCAT_MCP_ARTIFACT_BUCKET", BUCKET),
            ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN),
            ("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", good.path()),
        ],
    );
    assert!(!ok && err.contains("cannot be combined"), "{err}");
}

#[test]
fn short_links_are_opt_in() {
    // Unset or `direct`: direct links, no secret needed.
    let c = ArtifactConfig::default().gcs(BUCKET);
    assert_eq!(c.public_origin(), Ok(None));
    assert!(check(c.clone()).is_ok());
    let direct = ArtifactConfig {
        artifact_public_origin: Some("direct".into()),
        ..c.clone()
    };
    assert_eq!(direct.public_origin(), Ok(None));
    assert!(check(direct).is_ok());
    // Given: short links, which need the secret.
    let given = ArtifactConfig {
        artifact_public_origin: Some("https://links.example.test/".into()),
        ..c
    };
    assert_eq!(
        given.public_origin(),
        Ok(Some("https://links.example.test".into()))
    );
    assert!(check(given).unwrap_err().contains("LINK_SECRET_FILE"));
}

/// A deployed server with `gcs` links and an https OAuth resource, and no
/// public origin, keeps starting without a secret and keeps direct links.
#[tokio::test(flavor = "multi_thread")]
async fn an_oauth_resource_does_not_turn_short_links_on() {
    let fake = fake_google().await;
    let capture = std::env::temp_dir().join(format!(
        "pscat-mcp-optin-capture-{}.jsonl",
        uuid::Uuid::new_v4().simple()
    ));
    let mut server = start_http_with(
        &fake,
        None,
        &capture.to_string_lossy(),
        &[("PSCAT_MCP_OAUTH_RESOURCE", "https://mcp.example.test/mcp")],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let v = post(
        &http,
        &server.base,
        &render(1, "0 0 1 1 rectfill", "url"),
        None,
    )
    .await;
    let url = v["result"]["structuredContent"]["artifacts"][0]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no link: {v}"));
    assert!(
        url.starts_with("https://storage.googleapis.com/") && url.contains("X-Goog-Signature="),
        "{url}"
    );
    server.child.kill().await.unwrap();
    let _ = std::fs::remove_file(&capture);
}

// ---------------- short links: logs and capture, through the binary

/// Render with short links, open the link (with `utm_source`, and with a
/// bad token), send it back inside requests; return the link, the logs and
/// the capture file.
async fn short_link_round_trip(rust_log: Option<&str>) -> (String, String, String) {
    let fake = fake_google().await;
    let secret = SecretFile::new(b"a secret only the server and this test know, 48b");
    let capture = std::env::temp_dir().join(format!(
        "pscat-mcp-short-capture-{}.jsonl",
        uuid::Uuid::new_v4().simple()
    ));
    let capture_path = capture.to_string_lossy().into_owned();
    let mut server = start_http_with(
        &fake,
        rust_log,
        &capture_path,
        &[
            ("PSCAT_MCP_ARTIFACT_PUBLIC_ORIGIN", ORIGIN),
            ("PSCAT_MCP_ARTIFACT_LINK_SECRET_FILE", secret.path()),
        ],
    )
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let v = post(
        &http,
        &server.base,
        &render(1, "0.3 setgray 0 0 40 40 rectfill", "url"),
        None,
    )
    .await;
    let link = v["result"]["structuredContent"]["artifacts"][0]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no link: {v}"))
        .to_string();
    let root = server.base.strip_suffix("/mcp").unwrap().to_string();
    let target = target_of(&link).to_string();
    // Opened as a model would: with a parameter appended. No Authorization.
    let resp = http
        .get(format!("{root}{target}&utm_source=chatgpt.com"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(
        location.starts_with("https://storage.googleapis.com/"),
        "{location}"
    );
    // A bad token, a HEAD and a POST.
    let bad = target.replace("&s=", "&s=A");
    let resp = http.get(format!("{root}{bad}")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(resp.text().await.unwrap(), "forbidden\n");
    let resp = http.head(format!("{root}{target}")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let resp = http.post(format!("{root}{target}")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 405);
    // A client sends the link back: in a tool argument and in `Mcp-Name`.
    let source = format!("% {link}\n0 0 1 1 rectfill");
    post(&http, &server.base, &render(2, &source, "inline"), None).await;
    let read = json!({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
                      "params": {"uri": link}});
    let v = post(&http, &server.base, &read, Some(&link)).await;
    assert!(v.get("error").is_some(), "{v}");
    server.child.kill().await.unwrap();
    let logs = tokio::time::timeout(Duration::from_secs(30), server.logs)
        .await
        .expect("stderr closed")
        .unwrap();
    let captured = std::fs::read_to_string(&capture).unwrap_or_default();
    let _ = std::fs::remove_file(&capture);
    (link, logs, captured)
}

fn no_short_link_secrets(link: &str, logs: &str, captured: &str) {
    let target = target_of(link);
    let (path, query) = target.split_once('?').unwrap();
    let id = path.split('/').nth(2).unwrap();
    let token = query.split("s=").nth(1).unwrap();
    assert_eq!(id.len(), 36, "{id}");
    for (what, text) in [("logs", logs), ("capture file", captured)] {
        assert!(!text.contains(id), "the render id is in the {what}");
        assert!(!text.contains(token), "the token is in the {what}");
        assert!(!text.contains(query), "the query is in the {what}");
        assert!(!text.contains("utm_source"), "{what}: a query is logged");
        assert!(!text.contains("X-Goog-Signature"), "{what}: {text}");
        assert!(!text.contains("ya29."), "a token is in the {what}");
        assert!(
            !text.contains("secret only the server"),
            "the secret is in the {what}"
        );
    }
    assert!(!logs.contains("storage.googleapis.com"), "{logs}");
    assert!(!logs.contains("artifact link redirect failed"), "{logs}");
    // The request span names the route, not the path.
    assert!(logs.contains("/a/[redacted]"), "{logs}");
    // The capture recorded the link sent back, id and query redacted.
    assert!(
        captured.contains(&format!("{ORIGIN}/a/[redacted]/1.png?[redacted]")),
        "{captured}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn short_links_never_reach_the_logs_or_the_capture_file() {
    let (link, logs, captured) = short_link_round_trip(None).await;
    no_short_link_secrets(&link, &logs, &captured);
}

#[tokio::test(flavor = "multi_thread")]
async fn short_links_stay_out_of_the_logs_at_pscat_mcp_trace() {
    let (link, logs, captured) = short_link_round_trip(Some("info,pscat_mcp=trace")).await;
    no_short_link_secrets(&link, &logs, &captured);
    // The route's one line, at DEBUG, has an outcome only.
    let lines: Vec<Value> = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["message"] == "short link")
        .collect();
    let outcomes: Vec<&str> = lines
        .iter()
        .map(|v| v["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        ["redirect", "forbidden", "redirect", "method_not_allowed"],
        "{logs}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn short_links_stay_out_of_the_logs_at_trace() {
    let (link, logs, captured) = short_link_round_trip(Some("trace")).await;
    no_short_link_secrets(&link, &logs, &captured);
}
