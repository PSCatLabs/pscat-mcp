//! The Cloud Storage artifact store (#140): `PSCAT_MCP_ARTIFACT_LINKS=gcs`.
//!
//! [`GcsArtifactStore::put`] uploads one call's artifacts to
//! `gs://<bucket>/<prefix><render_id>/<n>.<ext>` and returns a V4 signed URL
//! per object, signed without a key:
//!
//! 1. **Credentials** come from the metadata server only
//!    (`/computeMetadata/v1/instance/service-accounts/default/token`, with
//!    `Metadata-Flavor: Google`). The access token is cached until shortly
//!    before it expires and refreshed by one caller at a time. No key, key
//!    file or credential in the environment is ever read.
//! 2. **Upload**: one `multipart` upload per object to the JSON API
//!    (`upload/storage/v1/b/<bucket>/o?uploadType=multipart&ifGenerationMatch=0`),
//!    whose metadata part sets `name`, `contentType` and
//!    `contentDisposition` (`inline; filename="pscat-<n>.<ext>"`). Multipart
//!    because the runtime account may only create and get objects: a media
//!    upload followed by a metadata `PATCH` would need `objects.update`.
//!    `ifGenerationMatch=0` makes an existing object a `412`, so nothing is
//!    ever overwritten; the `render_id` is a random UUID besides. The
//!    server never lists, reads or deletes objects; the bucket's lifecycle
//!    rule removes them.
//! 3. **Signing**: the V4 canonical request and string-to-sign
//!    (`GOOG4-RSA-SHA256`, path-style `https://storage.googleapis.com/<bucket>/<object>`,
//!    signed header `host`, `UNSIGNED-PAYLOAD`) are built here
//!    ([`v4_string_to_sign`]); the IAM Credentials API's `signBlob` on the
//!    signer account (`PSCAT_MCP_ARTIFACT_SIGNER`, by default the account the
//!    metadata server reports) signs them, and the signature is hex-encoded
//!    into `X-Goog-Signature`.
//!
//! The HTTP client is the hyper + rustls (ring) stack the OAuth resource
//! server uses; no Google SDK. Every call has its own timeout
//! (`PSCAT_MCP_ARTIFACT_GCS_TIMEOUT_MS`), a response size cap
//! ([`MAX_RESPONSE_BYTES`]) and follows no redirect.
//!
//! **Secrets.** A signed URL is a bearer capability until it expires, and the
//! access token is a credential. Neither, nor an upload URL (its object name
//! holds the `render_id`), nor a response body, ever reaches a log line, an
//! error message or a `Debug` string: a failure is logged as a fixed code,
//! the stage that failed and at most an HTTP status
//! ([`ErrorCode::StorageUnavailable`]).
//!
//! **Quota.** Each principal may upload `PSCAT_MCP_ARTIFACT_QUOTA_BYTES`
//! (charged as the memory store charges: the bytes plus
//! [`RENDER_CHARGE_BYTES`] a call) within one TTL. The ledger is in this
//! process's memory, so the quota is per server instance. A whole call is
//! reserved before its first upload; a call that fails before any upload is
//! refunded, one that failed after an upload keeps its charge (its objects
//! may exist, and only the lifecycle rule deletes them).
//!
//! **The worker** gets nothing from this module: only the server process
//! talks to Google.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use axum::body::Bytes;
use base64::Engine as _;
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::artifacts::{
    ArtifactLink, ArtifactObject, ArtifactStore, LinkScheme, RENDER_CHARGE_BYTES, StoreFuture,
    StoredRender, store_refusal,
};
use crate::config::ArtifactConfig;
use crate::render::{ErrorCode, RenderError};

/// The metadata server, as seen from a Google Cloud instance.
pub const DEFAULT_METADATA_ENDPOINT: &str = "http://metadata.google.internal";
/// The IAM Service Account Credentials API.
pub const DEFAULT_IAM_ENDPOINT: &str = "https://iamcredentials.googleapis.com";
/// Cloud Storage's JSON API, where uploads go.
pub const DEFAULT_STORAGE_ENDPOINT: &str = "https://storage.googleapis.com";
/// The host signed URLs name (path style).
pub const SIGNED_URL_HOST: &str = "storage.googleapis.com";

/// The most bytes read of any response; a longer one is a failure.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// A token is refreshed this long before it expires (or at half its life,
/// if that is sooner), so an upload and its signing never run on one that
/// lapses in between.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// A wall clock, injectable for tests: the signing time, token expiry and
/// the quota window.
pub type WallClock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

// ------------------------------------------------------------- settings

/// Check a bucket name: 3 to 63 characters (222 with dots) of lowercase
/// letters, digits, `-`, `_` and `.`, starting and ending with a letter or
/// digit.
pub fn check_bucket(bucket: &str) -> Result<(), String> {
    let ok_chars = bucket
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
    let max = if bucket.contains('.') { 222 } else { 63 };
    let edges = bucket
        .bytes()
        .next()
        .zip(bucket.bytes().last())
        .is_some_and(|(a, z)| a.is_ascii_alphanumeric() && z.is_ascii_alphanumeric());
    if ok_chars && edges && (3..=max).contains(&bucket.len()) && !bucket.contains("..") {
        Ok(())
    } else {
        Err(
            "PSCAT_MCP_ARTIFACT_BUCKET is not a valid bucket name (3 to 63 lowercase letters, \
             digits, '-', '_' or '.', starting and ending with a letter or digit)"
                .into(),
        )
    }
}

/// Check an object name prefix: letters, digits, `.`, `_`, `-` and `/`,
/// ending in `/`, with no leading `/`, no empty segment and no `.` or `..`
/// segment; at most 512 bytes.
pub fn check_prefix(prefix: &str) -> Result<(), String> {
    let chars = prefix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
    let segments_ok = prefix
        .strip_suffix('/')
        .is_some_and(|p| p.split('/').all(|s| !s.is_empty() && s != "." && s != ".."));
    if chars && segments_ok && !prefix.starts_with('/') && prefix.len() <= 512 {
        Ok(())
    } else {
        Err(
            "PSCAT_MCP_ARTIFACT_PREFIX must be letters, digits, '.', '_', '-' and '/', end in \
             '/', and have no leading '/', empty segment or '..' (for example renders/)"
                .into(),
        )
    }
}

/// Check a service account email: one `@`, and only letters, digits, `.`,
/// `_`, `-` and `@` (it goes into the IAM Credentials URL path).
pub fn check_signer(email: &str) -> Result<(), String> {
    let chars = email
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b));
    let parts: Vec<&str> = email.split('@').collect();
    if chars && parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) && email.len() <= 254 {
        Ok(())
    } else {
        Err("PSCAT_MCP_ARTIFACT_SIGNER is not a service account email".into())
    }
}

/// Check a test-only endpoint override: `https`, or `http` on a loopback
/// host; no path, query or fragment.
pub fn check_endpoint(name: &str, value: &str) -> Result<(), String> {
    let uri = crate::oauth::checked_url(name, value, false).map_err(|e| e.to_string())?;
    if !matches!(uri.path(), "" | "/") {
        return Err(format!("{name} must be a base URL with no path"));
    }
    Ok(())
}

/// `value` with the trailing `/` taken off.
fn base(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

/// What the store needs from [`ArtifactConfig`], checked.
#[derive(Clone)]
pub struct GcsSettings {
    /// The bucket.
    pub bucket: String,
    /// The object name prefix (ends in `/`).
    pub prefix: String,
    /// The signer account; `None`: the metadata server's default account.
    pub signer: Option<String>,
    /// How long signed URLs work.
    pub ttl: Duration,
    /// Bytes one principal may upload within one TTL.
    pub quota: usize,
    /// The timeout of each call.
    pub timeout: Duration,
    /// The metadata server's base URL.
    pub metadata_endpoint: String,
    /// The IAM Credentials base URL.
    pub iam_endpoint: String,
    /// The Cloud Storage base URL for uploads.
    pub storage_endpoint: String,
}

impl std::fmt::Debug for GcsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsSettings")
            .field("ttl", &self.ttl)
            .field("quota", &self.quota)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl GcsSettings {
    /// The settings `config` gives (already checked by
    /// [`ArtifactConfig::check`]).
    pub fn from_config(config: &ArtifactConfig) -> anyhow::Result<Self> {
        let bucket = config
            .artifact_bucket
            .clone()
            .context("PSCAT_MCP_ARTIFACT_LINKS=gcs needs PSCAT_MCP_ARTIFACT_BUCKET")?;
        Ok(GcsSettings {
            bucket,
            prefix: config.artifact_prefix.clone(),
            signer: config.artifact_signer.clone(),
            ttl: config.ttl(),
            quota: config.artifact_quota_bytes,
            timeout: Duration::from_millis(config.artifact_gcs_timeout_ms),
            metadata_endpoint: base(
                config
                    .artifact_gcs_metadata_endpoint
                    .as_deref()
                    .unwrap_or(DEFAULT_METADATA_ENDPOINT),
            ),
            iam_endpoint: base(
                config
                    .artifact_gcs_iam_endpoint
                    .as_deref()
                    .unwrap_or(DEFAULT_IAM_ENDPOINT),
            ),
            storage_endpoint: base(
                config
                    .artifact_gcs_storage_endpoint
                    .as_deref()
                    .unwrap_or(DEFAULT_STORAGE_ENDPOINT),
            ),
        })
    }
}

// -------------------------------------------------------------- signing

/// `s` percent-encoded as RFC 3986 requires (uppercase hex), keeping only
/// the unreserved characters `A-Z a-z 0-9 - . _ ~`, and `/` too when
/// `keep_slash` (an object path).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What a V4 signed URL is made of before its signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4Unsigned {
    /// The path, `/<bucket>/<object>`, encoded.
    pub path: String,
    /// The canonical query string (`X-Goog-*` parameters, sorted).
    pub query: String,
    /// The canonical request.
    pub canonical_request: String,
    /// The string to sign.
    pub string_to_sign: String,
}

impl V4Unsigned {
    /// The signed URL, with `signature` (the raw signature bytes)
    /// hex-encoded into `X-Goog-Signature`.
    pub fn url(&self, signature: &[u8]) -> String {
        let hex: String = signature.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "https://{SIGNED_URL_HOST}{}?{}&X-Goog-Signature={hex}",
            self.path, self.query
        )
    }
}

/// The canonical request and string-to-sign of a V4 signed `GET` URL for
/// `bucket`/`object`, signed by `signer` at `at` and working for
/// `expires_secs`: path style on [`SIGNED_URL_HOST`], `GOOG4-RSA-SHA256`,
/// the one signed header `host`, `UNSIGNED-PAYLOAD`, credential scope
/// `<date>/auto/storage/goog4_request`.
///
/// The rules are Google's: canonical requests,
/// <https://docs.cloud.google.com/storage/docs/authentication/canonical-requests>,
/// and signatures,
/// <https://docs.cloud.google.com/storage/docs/authentication/signatures>.
/// The unit tests check the output against Google's conformance vectors,
/// <https://github.com/googleapis/conformance-tests/blob/main/storage/v1/v4_signatures.json>.
pub fn v4_string_to_sign(
    bucket: &str,
    object: &str,
    signer: &str,
    at: SystemTime,
    expires_secs: u64,
) -> V4Unsigned {
    let t = chrono::DateTime::<chrono::Utc>::from(at);
    let datetime = t.format("%Y%m%dT%H%M%SZ").to_string();
    let date = t.format("%Y%m%d").to_string();
    let scope = format!("{date}/auto/storage/goog4_request");
    let credential = format!("{signer}/{scope}");
    // Already in code-point order.
    let query = format!(
        "X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential={}&X-Goog-Date={datetime}\
         &X-Goog-Expires={expires_secs}&X-Goog-SignedHeaders=host",
        uri_encode(&credential, false)
    );
    let path = format!(
        "/{}/{}",
        uri_encode(bucket, false),
        uri_encode(object, true)
    );
    let canonical_request =
        format!("GET\n{path}\n{query}\nhost:{SIGNED_URL_HOST}\n\nhost\nUNSIGNED-PAYLOAD");
    let hashed: String = Sha256::digest(canonical_request.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let string_to_sign = format!("GOOG4-RSA-SHA256\n{datetime}\n{scope}\n{hashed}");
    V4Unsigned {
        path,
        query,
        canonical_request,
        string_to_sign,
    }
}

// ---------------------------------------------------------------- HTTP

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Which step of a `put` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The access token or the signer's email (metadata server).
    Credentials,
    /// An object upload (Cloud Storage).
    Upload,
    /// A `signBlob` call (IAM Credentials).
    Signing,
}

impl Stage {
    /// Its name, as logs and messages give it.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Credentials => "credentials",
            Stage::Upload => "upload",
            Stage::Signing => "signing",
        }
    }
}

/// How a call failed. Never holds a URL, a token or a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// Not the expected status.
    Status(u16),
    /// No answer within the timeout.
    Timeout,
    /// The connection or request failed.
    Transport,
    /// The answer was too large or not what was expected.
    BadResponse,
}

impl Failure {
    fn as_str(self) -> &'static str {
        match self {
            Failure::Status(_) => "status",
            Failure::Timeout => "timeout",
            Failure::Transport => "transport",
            Failure::BadResponse => "bad_response",
        }
    }
}

/// A failed call at `stage`.
#[derive(Debug, Clone, Copy)]
struct GcsError {
    stage: Stage,
    failure: Failure,
}

impl GcsError {
    fn at(stage: Stage) -> impl Fn(Failure) -> GcsError {
        move |failure| GcsError { stage, failure }
    }

    /// The tool error, logged once as a WARN with codes only.
    fn into_render_error(self) -> RenderError {
        let status = match self.failure {
            Failure::Status(s) => Some(s),
            _ => None,
        };
        tracing::warn!(
            code = ErrorCode::StorageUnavailable.as_str(),
            stage = self.stage.as_str(),
            failure = self.failure.as_str(),
            status,
            "artifact links failed"
        );
        RenderError::new(
            ErrorCode::StorageUnavailable,
            format!(
                "the output could not be stored for links (Cloud Storage {} failed: {}{}); \
                 retry, or use delivery: \"inline\" to get it in the result",
                self.stage.as_str(),
                self.failure.as_str(),
                status.map_or(String::new(), |s| format!(" {s}")),
            ),
        )
    }
}

/// A response: status, headers and at most [`MAX_RESPONSE_BYTES`] of body.
struct Response {
    status: u16,
    headers: http::HeaderMap,
    body: Bytes,
}

/// An access token. Its `Debug` never shows it.
#[derive(Clone)]
struct Token(Arc<str>);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token([redacted])")
    }
}

struct CachedToken {
    token: Token,
    refresh_at: SystemTime,
}

/// Bytes one principal uploaded and when.
type Ledger = HashMap<String, Vec<(SystemTime, usize)>>;

/// The `gcs` backend. See the module docs.
pub struct GcsArtifactStore {
    settings: GcsSettings,
    client: HttpClient,
    clock: WallClock,
    token: tokio::sync::Mutex<Option<CachedToken>>,
    signer: tokio::sync::Mutex<Option<String>>,
    ledger: Mutex<Ledger>,
}

impl std::fmt::Debug for GcsArtifactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsArtifactStore")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl GcsArtifactStore {
    /// A store as `config` says.
    pub fn from_config(config: &ArtifactConfig) -> anyhow::Result<Self> {
        Self::new(GcsSettings::from_config(config)?, Arc::new(SystemTime::now))
    }

    /// A store with `settings`, on `clock` (tests inject one).
    pub fn new(settings: GcsSettings, clock: WallClock) -> anyhow::Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider)
            .context("building the TLS client for Cloud Storage")?
            // The metadata server is plain http; overrides are checked
            // (`check_endpoint`) to be https or loopback.
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);
        Ok(GcsArtifactStore {
            settings,
            client,
            clock,
            token: tokio::sync::Mutex::new(None),
            signer: tokio::sync::Mutex::new(None),
            ledger: Mutex::new(HashMap::new()),
        })
    }

    /// Send `req`, within the timeout, reading at most
    /// [`MAX_RESPONSE_BYTES`] of the body. Redirects are not followed.
    async fn send(&self, req: http::Request<Full<Bytes>>) -> Result<Response, Failure> {
        let call = async {
            let resp = self
                .client
                .request(req)
                .await
                .map_err(|_| Failure::Transport)?;
            let status = resp.status().as_u16();
            let (parts, mut body) = resp.into_parts();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| Failure::Transport)?;
                if let Some(data) = frame.data_ref() {
                    if buf.len() + data.len() > MAX_RESPONSE_BYTES {
                        return Err(Failure::BadResponse);
                    }
                    buf.extend_from_slice(data);
                }
            }
            Ok(Response {
                status,
                headers: parts.headers,
                body: buf.into(),
            })
        };
        tokio::time::timeout(self.settings.timeout, call)
            .await
            .map_err(|_| Failure::Timeout)?
    }

    /// GET a metadata server path: `200` with `Metadata-Flavor: Google`.
    async fn metadata(&self, path: &str) -> Result<Bytes, Failure> {
        let req = http::Request::get(format!("{}{path}", self.settings.metadata_endpoint))
            .header("Metadata-Flavor", "Google")
            .body(Full::new(Bytes::new()))
            .map_err(|_| Failure::Transport)?;
        let resp = self.send(req).await?;
        if resp.status != 200 {
            return Err(Failure::Status(resp.status));
        }
        let flavor = resp
            .headers
            .get("metadata-flavor")
            .and_then(|v| v.to_str().ok());
        if flavor != Some("Google") {
            return Err(Failure::BadResponse);
        }
        Ok(resp.body)
    }

    /// The access token: cached until shortly before it expires; one
    /// caller refreshes while the others wait for it.
    async fn token(&self) -> Result<Token, GcsError> {
        let mut cached = self.token.lock().await;
        let now = (self.clock)();
        if let Some(c) = cached.as_ref()
            && now < c.refresh_at
        {
            return Ok(c.token.clone());
        }
        let err = GcsError::at(Stage::Credentials);
        let body = self
            .metadata("/computeMetadata/v1/instance/service-accounts/default/token")
            .await
            .map_err(&err)?;
        let v: Value = serde_json::from_slice(&body).map_err(|_| err(Failure::BadResponse))?;
        let token = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 8192 && t.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| err(Failure::BadResponse))?;
        let life = Duration::from_secs(
            v["expires_in"]
                .as_u64()
                .ok_or_else(|| err(Failure::BadResponse))?,
        );
        let margin = TOKEN_REFRESH_MARGIN.min(life / 2);
        let token = Token(token.into());
        *cached = Some(CachedToken {
            token: token.clone(),
            refresh_at: now + life - margin,
        });
        Ok(token)
    }

    /// The signer's email: the configured one, or the metadata server's
    /// default account, fetched once.
    async fn signer(&self) -> Result<String, GcsError> {
        if let Some(s) = &self.settings.signer {
            return Ok(s.clone());
        }
        let mut cached = self.signer.lock().await;
        if let Some(s) = cached.as_ref() {
            return Ok(s.clone());
        }
        let err = GcsError::at(Stage::Credentials);
        let body = self
            .metadata("/computeMetadata/v1/instance/service-accounts/default/email")
            .await
            .map_err(&err)?;
        let email = std::str::from_utf8(&body)
            .map(str::trim)
            .map_err(|_| err(Failure::BadResponse))?;
        check_signer(email).map_err(|_| err(Failure::BadResponse))?;
        *cached = Some(email.to_string());
        Ok(email.to_string())
    }

    /// Upload `object` as `name`, never over an existing object.
    async fn upload(
        &self,
        token: &Token,
        name: &str,
        n: usize,
        object: &ArtifactObject,
    ) -> Result<(), GcsError> {
        let err = GcsError::at(Stage::Upload);
        let ext = extension(&object.mime_type);
        let metadata = serde_json::json!({
            "name": name,
            "contentType": object.mime_type,
            "contentDisposition": format!("inline; filename=\"pscat-{n}.{ext}\""),
        })
        .to_string();
        // A random boundary; one that occurs in the bytes is drawn again.
        let boundary = loop {
            let b = format!("pscat-mcp-{}", uuid::Uuid::new_v4().simple());
            if !contains(&object.bytes, b.as_bytes()) {
                break b;
            }
        };
        let mut body = Vec::with_capacity(object.bytes.len() + metadata.len() + 256);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n\
                 --{boundary}\r\nContent-Type: {}\r\n\r\n",
                object.mime_type
            )
            .as_bytes(),
        );
        body.extend_from_slice(&object.bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=multipart&ifGenerationMatch=0",
            self.settings.storage_endpoint,
            uri_encode(&self.settings.bucket, false)
        );
        let req = http::Request::post(url)
            .header(http::header::AUTHORIZATION, format!("Bearer {}", token.0))
            .header(
                http::header::CONTENT_TYPE,
                format!("multipart/related; boundary={boundary}"),
            )
            .header(http::header::USER_AGENT, "pscat-mcp")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| err(Failure::Transport))?;
        let resp = self.send(req).await.map_err(&err)?;
        if resp.status != 200 {
            return Err(err(Failure::Status(resp.status)));
        }
        Ok(())
    }

    /// Sign `payload` with the signer's key through `signBlob`.
    async fn sign_blob(
        &self,
        token: &Token,
        signer: &str,
        payload: &str,
    ) -> Result<Vec<u8>, GcsError> {
        let err = GcsError::at(Stage::Signing);
        let b64 = base64::engine::general_purpose::STANDARD;
        let body = serde_json::json!({ "payload": b64.encode(payload.as_bytes()) }).to_string();
        let url = format!(
            "{}/v1/projects/-/serviceAccounts/{signer}:signBlob",
            self.settings.iam_endpoint
        );
        let req = http::Request::post(url)
            .header(http::header::AUTHORIZATION, format!("Bearer {}", token.0))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::USER_AGENT, "pscat-mcp")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| err(Failure::Transport))?;
        let resp = self.send(req).await.map_err(&err)?;
        if resp.status != 200 {
            return Err(err(Failure::Status(resp.status)));
        }
        let v: Value = serde_json::from_slice(&resp.body).map_err(|_| err(Failure::BadResponse))?;
        v["signedBlob"]
            .as_str()
            .and_then(|s| b64.decode(s).ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err(Failure::BadResponse))
    }

    /// Reserve `charge` bytes for `owner` at `now`, or refuse.
    fn reserve(&self, owner: &str, charge: usize, now: SystemTime) -> Result<(), RenderError> {
        let ttl = self.settings.ttl;
        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        ledger.retain(|_, entries| {
            entries.retain(|(at, _)| *at + ttl > now);
            !entries.is_empty()
        });
        let entries = ledger.entry(owner.to_string()).or_default();
        let held: usize = entries.iter().map(|(_, n)| *n).sum();
        let reached = held.saturating_add(charge);
        if reached > self.settings.quota {
            if entries.is_empty() {
                ledger.remove(owner);
            }
            return Err(store_refusal(
                format!(
                    "this client has uploaded {held} bytes of output within the last {} s and \
                     this call needs {charge} more, over the quota of {} bytes per client \
                     (PSCAT_MCP_ARTIFACT_QUOTA_BYTES); retry when earlier links expire, or use \
                     delivery: \"inline\"",
                    ttl.as_secs(),
                    self.settings.quota
                ),
                "artifact_quota_bytes",
                self.settings.quota,
                reached,
            ));
        }
        entries.push((now, charge));
        Ok(())
    }

    /// Give back a reservation made at `at` (nothing was uploaded).
    fn refund(&self, owner: &str, charge: usize, at: SystemTime) {
        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = ledger.get_mut(owner) {
            if let Some(i) = entries.iter().position(|e| *e == (at, charge)) {
                entries.remove(i);
            }
            if entries.is_empty() {
                ledger.remove(owner);
            }
        }
    }

    /// Bytes `owner` has charged within the window (tests).
    pub fn charged_bytes(&self, owner: &str) -> usize {
        let now = (self.clock)();
        let ttl = self.settings.ttl;
        let ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        ledger.get(owner).map_or(0, |entries| {
            entries
                .iter()
                .filter(|(at, _)| *at + ttl > now)
                .map(|(_, n)| *n)
                .sum()
        })
    }

    async fn put_inner(
        &self,
        id: &str,
        objects: &[ArtifactObject],
        at: SystemTime,
        uploaded: &mut bool,
    ) -> Result<Vec<ArtifactLink>, GcsError> {
        let token = self.token().await?;
        let signer = self.signer().await?;
        let names: Vec<String> = objects
            .iter()
            .enumerate()
            .map(|(i, o)| {
                format!(
                    "{}{id}/{}.{}",
                    self.settings.prefix,
                    i + 1,
                    extension(&o.mime_type)
                )
            })
            .collect();
        for (i, (name, object)) in names.iter().zip(objects).enumerate() {
            *uploaded = true;
            self.upload(&token, name, i + 1, object).await?;
        }
        let mut links = Vec::with_capacity(names.len());
        for name in &names {
            let unsigned = v4_string_to_sign(
                &self.settings.bucket,
                name,
                &signer,
                at,
                self.settings.ttl.as_secs(),
            );
            let signature = self
                .sign_blob(&token, &signer, &unsigned.string_to_sign)
                .await?;
            let url = unsigned.url(&signature);
            links.push(ArtifactLink {
                uri: url.clone(),
                download_url: url,
                scheme: LinkScheme::Https,
            });
        }
        Ok(links)
    }
}

/// Whether `needle` occurs in `hay`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    let Some((&first, _)) = needle.split_first() else {
        return true;
    };
    let mut start = 0;
    while let Some(i) = hay[start..].iter().position(|&b| b == first) {
        let at = start + i;
        if hay[at..].starts_with(needle) {
            return true;
        }
        start = at + 1;
    }
    false
}

/// The file extension for an artifact's MIME type.
fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// `t` to the whole second below.
fn whole_seconds(t: SystemTime) -> SystemTime {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => SystemTime::UNIX_EPOCH + Duration::from_secs(d.as_secs()),
        Err(_) => t,
    }
}

impl ArtifactStore for GcsArtifactStore {
    fn name(&self) -> &'static str {
        "gcs"
    }

    fn ttl(&self) -> Duration {
        self.settings.ttl
    }

    fn quota_bytes(&self) -> usize {
        self.settings.quota
    }

    fn put<'a>(
        &'a self,
        owner: &'a str,
        objects: Vec<ArtifactObject>,
    ) -> StoreFuture<'a, Result<StoredRender, RenderError>> {
        Box::pin(async move {
            let at = whole_seconds((self.clock)());
            let charge = objects
                .iter()
                .fold(RENDER_CHARGE_BYTES, |n, o| n.saturating_add(o.bytes.len()));
            self.reserve(owner, charge, at)?;
            let id = uuid::Uuid::new_v4().to_string();
            let mut uploaded = false;
            match self.put_inner(&id, &objects, at, &mut uploaded).await {
                Ok(links) => Ok(StoredRender {
                    render_id: id,
                    expires_at: at + self.settings.ttl,
                    links,
                }),
                Err(e) => {
                    if !uploaded {
                        self.refund(owner, charge, at);
                    }
                    Err(e.into_render_error())
                }
            }
        })
    }

    // `read` keeps the default `None`: the signed URL is the only way to
    // fetch an object.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> SystemTime {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .into()
    }

    const SIGNER: &str = "test-iam-credentials@dummy-project-id.iam.gserviceaccount.com";

    /// Google's conformance vectors for V4 signed URLs
    /// (<https://github.com/googleapis/conformance-tests/blob/main/storage/v1/v4_signatures.json>,
    /// `signingV4Tests`): the cases this store's requests fit (GET,
    /// path style, no extra headers or query parameters). The expected URL
    /// is up to `&X-Goog-Signature=`, which needs the vectors' RSA key.
    struct Vector {
        name: &'static str,
        bucket: &'static str,
        object: &'static str,
        timestamp: &'static str,
        expiration: u64,
        canonical_request: &'static str,
        string_to_sign: &'static str,
        url_unsigned: &'static str,
    }

    const VECTORS: &[Vector] = &[
        Vector {
            name: "Simple GET",
            bucket: "test-bucket",
            object: "test-object",
            timestamp: "2019-02-01T09:00:00Z",
            expiration: 10,
            canonical_request: "GET\n/test-bucket/test-object\nX-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD",
            string_to_sign: "GOOG4-RSA-SHA256\n20190201T090000Z\n20190201/auto/storage/goog4_request\n00e2fb794ea93d7adb703edaebdd509821fcc7d4f1a79ac5c8d2b394df109320",
            url_unsigned: "https://storage.googleapis.com/test-bucket/test-object?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host",
        },
        Vector {
            name: "Vary expiration and timestamp",
            bucket: "test-bucket",
            object: "test-object",
            timestamp: "2019-03-01T09:00:00Z",
            expiration: 20,
            canonical_request: "GET\n/test-bucket/test-object\nX-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190301%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190301T090000Z&X-Goog-Expires=20&X-Goog-SignedHeaders=host\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD",
            string_to_sign: "GOOG4-RSA-SHA256\n20190301T090000Z\n20190301/auto/storage/goog4_request\n779f19fdb6fd381390e2d5af04947cf21750277ee3c20e0c97b7e46a1dff8907",
            url_unsigned: "https://storage.googleapis.com/test-bucket/test-object?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190301%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190301T090000Z&X-Goog-Expires=20&X-Goog-SignedHeaders=host",
        },
        Vector {
            name: "Vary bucket and object",
            bucket: "test-bucket2",
            object: "test-object2",
            timestamp: "2019-02-01T09:00:00Z",
            expiration: 10,
            canonical_request: "GET\n/test-bucket2/test-object2\nX-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD",
            string_to_sign: "GOOG4-RSA-SHA256\n20190201T090000Z\n20190201/auto/storage/goog4_request\na139afbf35ac30e9864f63197f79609731ab1b0ca166e2a456dba156fcd3f9ce",
            url_unsigned: "https://storage.googleapis.com/test-bucket2/test-object2?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host",
        },
        Vector {
            name: "Forward Slashes should not be stripped",
            bucket: "test-bucket",
            object: "/path/with/slashes/under_score/amper&sand/file.ext",
            timestamp: "2019-02-01T09:00:00Z",
            expiration: 10,
            canonical_request: "GET\n/test-bucket//path/with/slashes/under_score/amper%26sand/file.ext\nX-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host\nhost:storage.googleapis.com\n\nhost\nUNSIGNED-PAYLOAD",
            string_to_sign: "GOOG4-RSA-SHA256\n20190201T090000Z\n20190201/auto/storage/goog4_request\n63c601ecd6ccfec84f1113fc906609cbdf7651395f4300cecd96ddd2c35164f8",
            url_unsigned: "https://storage.googleapis.com/test-bucket//path/with/slashes/under_score/amper%26sand/file.ext?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Credential=test-iam-credentials%40dummy-project-id.iam.gserviceaccount.com%2F20190201%2Fauto%2Fstorage%2Fgoog4_request&X-Goog-Date=20190201T090000Z&X-Goog-Expires=10&X-Goog-SignedHeaders=host",
        },
    ];

    #[test]
    fn signing_matches_googles_conformance_vectors() {
        for v in VECTORS {
            let u = v4_string_to_sign(v.bucket, v.object, SIGNER, at(v.timestamp), v.expiration);
            assert_eq!(u.canonical_request, v.canonical_request, "{}", v.name);
            assert_eq!(u.string_to_sign, v.string_to_sign, "{}", v.name);
            let hashed: String = Sha256::digest(v.canonical_request.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(v.string_to_sign.lines().nth(3), Some(hashed.as_str()));
            let url = u.url(&[0xab, 0x01]);
            assert_eq!(
                url,
                format!("{}&X-Goog-Signature=ab01", v.url_unsigned),
                "{}",
                v.name
            );
        }
    }

    #[test]
    fn uri_encoding_keeps_only_unreserved_characters() {
        assert_eq!(uri_encode("aZ09-._~", false), "aZ09-._~");
        assert_eq!(uri_encode("a/b c@d&é", false), "a%2Fb%20c%40d%26%C3%A9");
        assert_eq!(uri_encode("a/b", true), "a/b");
    }

    #[test]
    fn bucket_prefix_signer_and_endpoints_are_checked() {
        for ok in ["my-bucket", "a.b.c", "abc", "a_b-9"] {
            assert!(check_bucket(ok).is_ok(), "{ok}");
        }
        for bad in ["", "ab", "Upper", "-a-", "a/b", "a..b", &"x".repeat(64)] {
            assert!(check_bucket(bad).is_err(), "{bad}");
        }
        for ok in ["renders/", "a/b/", "x.y_z-1/"] {
            assert!(check_prefix(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "renders",
            "/renders/",
            "a/../b/",
            "../",
            "a//",
            "./",
            "a b/",
            "a?b/",
            "é/",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
        assert!(check_signer("sa@p.iam.gserviceaccount.com").is_ok());
        for bad in ["", "no-at", "a@b@c", "a/b@c", "@b", "a@", "a b@c"] {
            assert!(check_signer(bad).is_err(), "{bad}");
        }
        for ok in [
            "https://example.test",
            "https://example.test/",
            "http://127.0.0.1:9",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            assert!(check_endpoint("E", ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.test",
            "ftp://127.0.0.1",
            "https://example.test/path",
            "https://example.test?q=1",
            "https://example.test#f",
            "not a url",
        ] {
            assert!(check_endpoint("E", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_boundary_search_finds_and_misses() {
        assert!(contains(b"xxabcx", b"abc"));
        assert!(contains(b"abc", b"abc"));
        assert!(!contains(b"ababx", b"abc"));
        assert!(!contains(b"", b"a"));
    }

    #[test]
    fn debug_strings_hide_the_token_and_the_bucket() {
        let t = Token("ya29.secret".into());
        assert!(!format!("{t:?}").contains("secret"));
        let s = GcsSettings {
            bucket: "hidden-bucket".into(),
            prefix: "renders/".into(),
            signer: Some("sa@p.iam.gserviceaccount.com".into()),
            ttl: Duration::from_secs(60),
            quota: 1,
            timeout: Duration::from_secs(1),
            metadata_endpoint: DEFAULT_METADATA_ENDPOINT.into(),
            iam_endpoint: DEFAULT_IAM_ENDPOINT.into(),
            storage_endpoint: DEFAULT_STORAGE_ENDPOINT.into(),
        };
        let d = format!("{s:?}");
        assert!(!d.contains("hidden-bucket") && !d.contains("sa@"), "{d}");
    }
}
