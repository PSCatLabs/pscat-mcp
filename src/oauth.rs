//! The `oauth` auth method: an OAuth 2.1 resource server, per the MCP
//! authorization spec (issue #119).
//!
//! pscat-mcp holds no passwords and runs no login page. A separate
//! *authorization server* (an identity provider, deliberately not chosen
//! here: any standards-based issuer works) signs users in and issues JWT
//! access tokens; this module only validates them:
//!
//! 1. A request without a token gets `401` with
//!    `WWW-Authenticate: Bearer realm="pscat-mcp", resource_metadata="…"`.
//! 2. The client reads the Protected Resource Metadata (RFC 9728) at that
//!    URL, which names the authorization server, the resource and the
//!    scopes. The HTTP layer serves it unauthenticated (see
//!    [`OauthAuth::resource_metadata`]).
//! 3. The client registers with the authorization server and signs the user
//!    in (authorization code with PKCE), asking for a token whose audience is
//!    this server's resource URL (RFC 8707).
//! 4. Each `/mcp` request carries `Authorization: Bearer <JWT>`, checked here.
//!
//! # Token validation
//!
//! In order, each step rejecting with `401 invalid_token` unless noted:
//!
//! * at most [`MAX_TOKEN_BYTES`] bytes, three base64url segments, a JSON
//!   header and payload;
//! * the header's `alg` is one of the asymmetric algorithms of [`Alg`]
//!   (`none` and every `HS*` are refused before any key is looked at), it
//!   carries a `kid`, and no `crit`; `jwk`, `jku` and `x5u` are ignored, so
//!   keys only ever come from the configured issuer's JWKS;
//! * the `kid` names a key of the issuer's JWKS whose type (and curve, and
//!   `alg` if the key states one) fits the header's `alg`;
//! * the signature verifies over the received `header.payload` bytes (ring;
//!   RSA keys of 2048 to 8192 bits, ECDSA in the JOSE fixed `r‖s` form);
//! * `iss` equals `PSCAT_MCP_OAUTH_ISSUER` exactly, `aud` (a string or an
//!   array) contains `PSCAT_MCP_OAUTH_RESOURCE` (a trailing `/` aside),
//!   `exp` is present and not past, `nbf` (if present) is not in the future,
//!   both with `PSCAT_MCP_OAUTH_LEEWAY_SECS` of leeway, and `sub` is a
//!   non-empty string;
//! * every scope of `PSCAT_MCP_OAUTH_SCOPES` is granted (`scope`, space
//!   separated, or `scp`), else `403 insufficient_scope`;
//! * the user is on the allowlist, else `403 not_allowed`: `sub:<subject>`
//!   (the issuer is already fixed, so this is issuer plus subject), or
//!   `email:<address>` only when the token also says `email_verified: true`.
//!
//! The token is only parsed (bounded JSON) and checked here, in the server;
//! nothing in it is executed, and no PostScript runs before it is accepted.
//!
//! # JWKS
//!
//! Keys are fetched lazily, on the first token that needs them, never at
//! startup (an identity-provider outage must not stop the server). The JWKS
//! URL is `PSCAT_MCP_OAUTH_JWKS_URL`, or discovered from the issuer's
//! metadata in the MCP spec's order (RFC 8414 with path insertion, OpenID
//! Connect with path insertion, OpenID Connect appended), whose `issuer` must
//! match. Each fetch has its own timeout (`PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS`,
//! which also bounds the whole key lookup, waiting included), a size cap
//! ([`MAX_DOCUMENT_BYTES`]), no redirects, and needs `https` (plain `http`
//! only for a loopback host, for tests). Keys are cached for
//! `PSCAT_MCP_OAUTH_JWKS_TTL_SECS`; an unknown `kid`, an expired cache or a
//! failed fetch triggers at most one fetch per
//! `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS`, one at a time, so garbage `kid`s
//! cannot make the server hammer the issuer. When a refresh fails, the keys
//! already held stay in use. A request without a JWT never waits for any of
//! this.
//!
//! # Privacy
//!
//! The principal id is `oauth:` and 128 bits of SHA-256 over issuer and
//! subject, so neither the subject nor an email reaches a log line or a job
//! record. Rejections log only a fixed `reason` (at INFO), never the token or
//! a claim.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use axum::body::Bytes;
use base64::Engine as _;
use clap::Args;
use http_body_util::{BodyExt as _, Empty};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::{
    AuthError, AuthFuture, AuthMethod, Challenge, Outcome, Principal, ResourceMetadata,
    bearer_token,
};

/// Longest access token looked at, in bytes; longer ones are rejected before
/// any decoding.
pub const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Largest JWKS or authorization-server metadata document read, in bytes.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Most keys kept from one JWKS.
pub const MAX_KEYS: usize = 64;
/// The RFC 9728 well-known path prefix.
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// Settings of the `oauth` method (used when `PSCAT_MCP_AUTH` lists it).
#[derive(Debug, Clone, Args)]
pub struct OauthConfig {
    /// The authorization server's issuer identifier, e.g.
    /// `https://example.eu.auth0.com/`. Tokens' `iss` must equal it exactly;
    /// it is also the metadata's `authorization_servers` entry.
    #[arg(long = "oauth-issuer", env = "PSCAT_MCP_OAUTH_ISSUER")]
    pub issuer: Option<String>,

    /// This server's resource URL (RFC 8707 / RFC 9728), the URL clients
    /// connect to, e.g. `https://mcp.example.com/mcp`. Tokens' `aud` must
    /// contain it. Never taken from the request's Host header.
    #[arg(long = "oauth-resource", env = "PSCAT_MCP_OAUTH_RESOURCE")]
    pub resource: Option<String>,

    /// JWKS URL. Unset: discovered from the issuer's metadata.
    #[arg(long = "oauth-jwks-url", env = "PSCAT_MCP_OAUTH_JWKS_URL")]
    pub jwks_url: Option<String>,

    /// Scopes a token must carry, comma separated; also published as
    /// `scopes_supported` and in the challenge's `scope`. Empty: no scope
    /// check.
    #[arg(
        long = "oauth-scopes",
        env = "PSCAT_MCP_OAUTH_SCOPES",
        value_delimiter = ','
    )]
    pub scopes: Vec<String>,

    /// Users allowed in, comma separated: `sub:<subject>` or
    /// `email:<address>` (matched only when `email_verified` is true).
    #[arg(
        long = "oauth-allowed-users",
        env = "PSCAT_MCP_OAUTH_ALLOWED_USERS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    pub allowed_users: Vec<String>,

    /// File of allowed users, one entry per line (`#` comments allowed).
    #[arg(
        long = "oauth-allowed-users-file",
        env = "PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE"
    )]
    pub allowed_users_file: Option<PathBuf>,

    /// Timeout of one JWKS (or discovery) fetch, and of a whole key lookup,
    /// in milliseconds.
    #[arg(
        long = "oauth-jwks-timeout-ms",
        env = "PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS",
        default_value_t = 5000
    )]
    pub jwks_timeout_ms: u64,

    /// How long fetched keys are used before they are fetched again, in
    /// seconds.
    #[arg(
        long = "oauth-jwks-ttl-secs",
        env = "PSCAT_MCP_OAUTH_JWKS_TTL_SECS",
        default_value_t = 600
    )]
    pub jwks_ttl_secs: u64,

    /// Least time between two JWKS fetch attempts, in seconds: an unknown
    /// `kid` or a failing issuer triggers at most one fetch per interval.
    #[arg(
        long = "oauth-jwks-refetch-secs",
        env = "PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS",
        default_value_t = 30
    )]
    pub jwks_refetch_secs: u64,

    /// Clock-skew leeway for `exp` and `nbf`, in seconds.
    #[arg(
        long = "oauth-leeway-secs",
        env = "PSCAT_MCP_OAUTH_LEEWAY_SECS",
        default_value_t = 60
    )]
    pub leeway_secs: u64,
}

impl Default for OauthConfig {
    fn default() -> Self {
        OauthConfig {
            issuer: None,
            resource: None,
            jwks_url: None,
            scopes: Vec::new(),
            allowed_users: Vec::new(),
            allowed_users_file: None,
            jwks_timeout_ms: 5000,
            jwks_ttl_secs: 600,
            jwks_refetch_secs: 30,
            leeway_secs: 60,
        }
    }
}

impl OauthConfig {
    /// Whether any OAuth setting is given (to warn when `oauth` is not
    /// enabled).
    pub fn is_configured(&self) -> bool {
        self.issuer.is_some() || self.resource.is_some() || !self.allowed_users.is_empty()
    }

    /// The allowlist entries from the variable and the file, trimmed, empty
    /// lines and `#` comments skipped.
    fn load_allowlist(&self) -> anyhow::Result<Allowlist> {
        let mut entries: Vec<String> = self
            .allowed_users
            .iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        if let Some(path) = &self.allowed_users_file {
            let text = std::fs::read_to_string(path).with_context(|| {
                format!(
                    "reading PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE {}",
                    path.display()
                )
            })?;
            entries.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_string),
            );
        }
        let mut list = Allowlist::default();
        for entry in entries {
            if let Some(sub) = entry.strip_prefix("sub:") {
                let sub = sub.trim();
                anyhow::ensure!(!sub.is_empty(), "an allowlist entry `sub:` has no subject");
                list.subjects.insert(sub.to_string());
            } else if let Some(email) = entry.strip_prefix("email:") {
                let email = email.trim();
                anyhow::ensure!(
                    email.contains('@'),
                    "an allowlist entry `email:` has no address"
                );
                list.emails.insert(email.to_ascii_lowercase());
            } else {
                // Do not echo the entry: it may be a personal address.
                anyhow::bail!(
                    "an entry of PSCAT_MCP_OAUTH_ALLOWED_USERS(_FILE) is neither `sub:<subject>` \
                     nor `email:<address>`"
                );
            }
        }
        Ok(list)
    }
}

#[derive(Debug, Default)]
struct Allowlist {
    subjects: HashSet<String>,
    /// Lowercased.
    emails: HashSet<String>,
}

impl Allowlist {
    fn len(&self) -> usize {
        self.subjects.len() + self.emails.len()
    }

    fn allows(&self, claims: &Value, sub: &str) -> bool {
        if self.subjects.contains(sub) {
            return true;
        }
        // An email only counts when the issuer vouches for it.
        claims["email_verified"] == Value::Bool(true)
            && claims["email"]
                .as_str()
                .is_some_and(|e| self.emails.contains(&e.to_ascii_lowercase()))
    }
}

/// A URL from the configuration, checked: `https`, or `http` to a loopback
/// host (tests and local development); no fragment.
pub(crate) fn checked_url(name: &str, value: &str, allow_query: bool) -> anyhow::Result<http::Uri> {
    let uri: http::Uri = value
        .parse()
        .map_err(|_| anyhow::anyhow!("{name} is not a URL"))?;
    let scheme = uri.scheme_str().unwrap_or("");
    let host = uri.host().unwrap_or("");
    anyhow::ensure!(!host.is_empty(), "{name} has no host");
    anyhow::ensure!(!value.contains('#'), "{name} must not contain a fragment");
    anyhow::ensure!(
        allow_query || uri.query().is_none(),
        "{name} must not contain a query"
    );
    match scheme {
        "https" => Ok(uri),
        "http" if is_loopback_host(host) => Ok(uri),
        _ => anyhow::bail!("{name} must be an https URL (http only for a loopback host)"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The signature algorithms accepted. Only asymmetric ones: `none` and the
/// `HS*` family are refused, so a public key can never be used as an HMAC
/// secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alg {
    RS256,
    RS384,
    RS512,
    PS256,
    PS384,
    PS512,
    ES256,
    ES384,
    EdDSA,
}

impl Alg {
    fn parse(s: &str) -> Option<Alg> {
        Some(match s {
            "RS256" => Alg::RS256,
            "RS384" => Alg::RS384,
            "RS512" => Alg::RS512,
            "PS256" => Alg::PS256,
            "PS384" => Alg::PS384,
            "PS512" => Alg::PS512,
            "ES256" => Alg::ES256,
            "ES384" => Alg::ES384,
            "EdDSA" => Alg::EdDSA,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Alg::RS256 => "RS256",
            Alg::RS384 => "RS384",
            Alg::RS512 => "RS512",
            Alg::PS256 => "PS256",
            Alg::PS384 => "PS384",
            Alg::PS512 => "PS512",
            Alg::ES256 => "ES256",
            Alg::ES384 => "ES384",
            Alg::EdDSA => "EdDSA",
        }
    }
}

/// A public key from the JWKS.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyMaterial {
    /// Big-endian modulus (no leading zeros) and exponent.
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// Uncompressed point `04‖x‖y`.
    P256(Vec<u8>),
    P384(Vec<u8>),
    Ed25519(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Jwk {
    material: KeyMaterial,
    /// The key's own `alg`, if it states one.
    alg: Option<String>,
}

impl Jwk {
    /// Verify `sig` over `msg` with `alg`, if this key fits `alg`.
    fn verify(&self, alg: Alg, msg: &[u8], sig: &[u8]) -> bool {
        use ring::signature as s;
        if self.alg.as_deref().is_some_and(|a| a != alg.name()) {
            return false;
        }
        match (&self.material, alg) {
            (KeyMaterial::Rsa { n, e }, _) => {
                let params: &s::RsaParameters = match alg {
                    Alg::RS256 => &s::RSA_PKCS1_2048_8192_SHA256,
                    Alg::RS384 => &s::RSA_PKCS1_2048_8192_SHA384,
                    Alg::RS512 => &s::RSA_PKCS1_2048_8192_SHA512,
                    Alg::PS256 => &s::RSA_PSS_2048_8192_SHA256,
                    Alg::PS384 => &s::RSA_PSS_2048_8192_SHA384,
                    Alg::PS512 => &s::RSA_PSS_2048_8192_SHA512,
                    _ => return false,
                };
                s::RsaPublicKeyComponents { n, e }
                    .verify(params, msg, sig)
                    .is_ok()
            }
            (KeyMaterial::P256(point), Alg::ES256) => {
                s::UnparsedPublicKey::new(&s::ECDSA_P256_SHA256_FIXED, point)
                    .verify(msg, sig)
                    .is_ok()
            }
            (KeyMaterial::P384(point), Alg::ES384) => {
                s::UnparsedPublicKey::new(&s::ECDSA_P384_SHA384_FIXED, point)
                    .verify(msg, sig)
                    .is_ok()
            }
            (KeyMaterial::Ed25519(x), Alg::EdDSA) => s::UnparsedPublicKey::new(&s::ED25519, x)
                .verify(msg, sig)
                .is_ok(),
            _ => false,
        }
    }
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
}

/// The usable signing keys of a JWKS document, by `kid`. Keys without a
/// `kid`, for another use than `sig`, of an unknown type or malformed are
/// skipped; at most [`MAX_KEYS`] are kept.
fn parse_jwks(doc: &Value) -> HashMap<String, Arc<Jwk>> {
    let mut out = HashMap::new();
    let Some(keys) = doc["keys"].as_array() else {
        return out;
    };
    for k in keys {
        if out.len() >= MAX_KEYS {
            break;
        }
        let Some(kid) = k["kid"].as_str() else {
            continue;
        };
        if k.get("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let field = |name: &str| k[name].as_str().and_then(b64url);
        let material = match (k["kty"].as_str(), k["crv"].as_str()) {
            (Some("RSA"), _) => {
                let (Some(n), Some(e)) = (field("n"), field("e")) else {
                    continue;
                };
                // ring wants both without leading zeros.
                let strip = |v: Vec<u8>| {
                    let first = v.iter().position(|b| *b != 0).unwrap_or(v.len());
                    v[first..].to_vec()
                };
                KeyMaterial::Rsa {
                    n: strip(n),
                    e: strip(e),
                }
            }
            (Some("EC"), Some(crv @ ("P-256" | "P-384"))) => {
                let len = if crv == "P-256" { 32 } else { 48 };
                let (Some(x), Some(y)) = (field("x"), field("y")) else {
                    continue;
                };
                if x.len() != len || y.len() != len {
                    continue;
                }
                let mut point = Vec::with_capacity(1 + 2 * len);
                point.push(4);
                point.extend_from_slice(&x);
                point.extend_from_slice(&y);
                if crv == "P-256" {
                    KeyMaterial::P256(point)
                } else {
                    KeyMaterial::P384(point)
                }
            }
            (Some("OKP"), Some("Ed25519")) => match field("x") {
                Some(x) if x.len() == 32 => KeyMaterial::Ed25519(x),
                _ => continue,
            },
            _ => continue,
        };
        out.entry(kid.to_string()).or_insert_with(|| {
            Arc::new(Jwk {
                material,
                alg: k["alg"].as_str().map(str::to_string),
            })
        });
    }
    out
}

/// Why a token was refused: the log `reason` and the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    NotAJwt,
    TooLarge,
    UnsupportedAlg,
    MissingKid,
    UnknownKid,
    JwksUnavailable,
    BadSignature,
    WrongIssuer,
    WrongAudience,
    Expired,
    NotYetValid,
    MissingSubject,
    InsufficientScope,
    NotAllowed,
}

impl Reject {
    fn reason(self) -> &'static str {
        match self {
            Reject::NotAJwt => "not_a_jwt",
            Reject::TooLarge => "token_too_large",
            Reject::UnsupportedAlg => "unsupported_alg",
            Reject::MissingKid => "missing_kid",
            Reject::UnknownKid => "unknown_kid",
            Reject::JwksUnavailable => "jwks_unavailable",
            Reject::BadSignature => "bad_signature",
            Reject::WrongIssuer => "wrong_issuer",
            Reject::WrongAudience => "wrong_audience",
            Reject::Expired => "expired",
            Reject::NotYetValid => "not_yet_valid",
            Reject::MissingSubject => "missing_subject",
            Reject::InsufficientScope => "insufficient_scope",
            Reject::NotAllowed => "not_allowed",
        }
    }

    fn error(self) -> AuthError {
        match self {
            Reject::InsufficientScope => AuthError::InsufficientScope,
            Reject::NotAllowed => AuthError::NotAllowed,
            _ => AuthError::InvalidToken,
        }
    }
}

/// A token split and decoded, not yet verified.
struct Parsed<'a> {
    alg: Alg,
    kid: String,
    /// `header.payload`, the signed bytes.
    signed: &'a [u8],
    signature: Vec<u8>,
    claims: Value,
}

fn parse_token(token: &str) -> Result<Parsed<'_>, Reject> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Reject::TooLarge);
    }
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Reject::NotAJwt);
    };
    let header: Value = b64url(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(Reject::NotAJwt)?;
    if !header.is_object() {
        return Err(Reject::NotAJwt);
    }
    // Before anything else: only the asymmetric algorithms, no extensions.
    let alg = header["alg"]
        .as_str()
        .and_then(Alg::parse)
        .ok_or(Reject::UnsupportedAlg)?;
    if header.get("crit").is_some() {
        return Err(Reject::UnsupportedAlg);
    }
    let kid = header["kid"]
        .as_str()
        .filter(|k| !k.is_empty())
        .ok_or(Reject::MissingKid)?
        .to_string();
    let claims: Value = b64url(p)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .ok_or(Reject::NotAJwt)?;
    let signature = b64url(s).ok_or(Reject::NotAJwt)?;
    Ok(Parsed {
        alg,
        kid,
        signed: &token.as_bytes()[..h.len() + 1 + p.len()],
        signature,
        claims,
    })
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn trim_slash(s: &str) -> &str {
    s.strip_suffix('/').unwrap_or(s)
}

/// The scopes a token grants: `scope` (space separated) or `scp` (an array
/// or a string).
fn granted_scopes(claims: &Value) -> HashSet<&str> {
    let mut out = HashSet::new();
    for name in ["scope", "scp"] {
        match &claims[name] {
            Value::String(s) => out.extend(s.split_whitespace()),
            Value::Array(a) => out.extend(a.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    out
}

/// The keys and when they were fetched.
#[derive(Default)]
struct KeySet {
    keys: HashMap<String, Arc<Jwk>>,
    fetched: Option<Instant>,
}

/// What the (single) refresher remembers.
#[derive(Default)]
struct Refresh {
    last_attempt: Option<Instant>,
    jwks_url: Option<String>,
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Empty<Bytes>>;

/// Fetches bounded JSON documents.
struct Fetcher {
    client: HttpClient,
    timeout: Duration,
}

impl Fetcher {
    fn new(timeout: Duration) -> anyhow::Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider)
            .context("building the TLS client for the OAuth issuer")?
            // Plain http is refused for anything but a loopback host by
            // `checked_url` before a request is made.
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);
        Ok(Fetcher { client, timeout })
    }

    /// GET `url` and parse it as JSON: `200` only, at most
    /// [`MAX_DOCUMENT_BYTES`], within the timeout. Redirects are not
    /// followed. Errors never contain the body.
    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let uri = checked_url("the URL", url, true).map_err(|e| e.to_string())?;
        let fetch = async {
            let req = http::Request::get(uri)
                .header(http::header::ACCEPT, "application/json")
                .header(http::header::USER_AGENT, "pscat-mcp")
                .body(Empty::<Bytes>::new())
                .map_err(|e| e.to_string())?;
            let resp = self.client.request(req).await.map_err(|e| e.to_string())?;
            if resp.status() != http::StatusCode::OK {
                return Err(format!("status {}", resp.status().as_u16()));
            }
            let mut body = resp.into_body();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| e.to_string())?;
                if let Some(data) = frame.data_ref() {
                    if buf.len() + data.len() > MAX_DOCUMENT_BYTES {
                        return Err(format!("larger than {MAX_DOCUMENT_BYTES} bytes"));
                    }
                    buf.extend_from_slice(data);
                }
            }
            serde_json::from_slice(&buf).map_err(|_| "not JSON".to_string())
        };
        tokio::time::timeout(self.timeout, fetch)
            .await
            .map_err(|_| format!("timed out after {} ms", self.timeout.as_millis()))?
    }
}

/// The issuer's metadata URLs, in the MCP spec's order.
fn discovery_urls(issuer: &str) -> Vec<String> {
    let base = trim_slash(issuer);
    let (origin, path) = match base.find("://").and_then(|i| {
        base[i + 3..]
            .find('/')
            .map(|j| (&base[..i + 3 + j], &base[i + 3 + j..]))
    }) {
        Some((origin, path)) => (origin, path),
        None => (base, ""),
    };
    if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{base}/.well-known/openid-configuration"),
        ]
    }
}

/// The `oauth` auth method. See the module docs.
pub struct OauthAuth {
    issuer: String,
    resource: String,
    resource_metadata_url: String,
    metadata_paths: Vec<String>,
    scopes: Vec<String>,
    allow: Allowlist,
    leeway: f64,
    ttl: Duration,
    refetch: Duration,
    timeout: Duration,
    fetcher: Fetcher,
    keys: RwLock<KeySet>,
    refresh: tokio::sync::Mutex<Refresh>,
}

impl std::fmt::Debug for OauthAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthAuth")
            .field("issuer", &self.issuer)
            .field("resource", &self.resource)
            .field("allowed_users", &self.allow.len())
            .finish()
    }
}

impl OauthAuth {
    /// The method name, logged as `auth_method`.
    pub const NAME: &'static str = "oauth";

    /// Build the method from its settings. No network access: keys are
    /// fetched on the first token. Refuses a missing issuer or resource, a
    /// URL that is not `https` (except on a loopback host), and an empty or
    /// malformed allowlist.
    pub fn from_config(cfg: &OauthConfig) -> anyhow::Result<Self> {
        let issuer = cfg
            .issuer
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context(
                "auth method `oauth` needs PSCAT_MCP_OAUTH_ISSUER (--oauth-issuer), the \
                 authorization server's issuer URL",
            )?;
        checked_url("PSCAT_MCP_OAUTH_ISSUER", issuer, false)?;
        let resource = cfg
            .resource
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context(
                "auth method `oauth` needs PSCAT_MCP_OAUTH_RESOURCE (--oauth-resource), this \
                 server's public URL such as https://mcp.example.com/mcp",
            )?;
        let uri = checked_url("PSCAT_MCP_OAUTH_RESOURCE", resource, false)?;
        let path = trim_slash(uri.path());
        anyhow::ensure!(
            path.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b)),
            "PSCAT_MCP_OAUTH_RESOURCE's path may only contain letters, digits and -._~/"
        );
        if let Some(jwks) = &cfg.jwks_url {
            checked_url("PSCAT_MCP_OAUTH_JWKS_URL", jwks, true)?;
        }
        let allow = cfg.load_allowlist()?;
        anyhow::ensure!(
            allow.len() > 0,
            "auth method `oauth` needs an allowlist: set PSCAT_MCP_OAUTH_ALLOWED_USERS or \
             PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE (entries `sub:<subject>` or `email:<address>`); \
             being able to sign in at the issuer is not enough"
        );
        anyhow::ensure!(
            cfg.jwks_timeout_ms > 0,
            "PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS must be positive"
        );
        let scopes: Vec<String> = cfg
            .scopes
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        anyhow::ensure!(
            scopes.iter().all(|s| s
                .bytes()
                .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')),
            "PSCAT_MCP_OAUTH_SCOPES entries must be printable ASCII without quotes"
        );
        let origin = format!(
            "{}://{}",
            uri.scheme_str().unwrap_or("https"),
            uri.authority().map(|a| a.as_str()).unwrap_or("")
        );
        // RFC 9728 3.1: the well-known prefix, then the resource's path.
        let suffixed = format!("{METADATA_PATH}{path}");
        let mut metadata_paths = vec![suffixed.clone()];
        if suffixed != METADATA_PATH {
            metadata_paths.push(METADATA_PATH.to_string());
        }
        let timeout = Duration::from_millis(cfg.jwks_timeout_ms);
        Ok(OauthAuth {
            issuer: issuer.to_string(),
            resource: resource.to_string(),
            resource_metadata_url: format!("{origin}{suffixed}"),
            metadata_paths,
            scopes,
            allow,
            leeway: cfg.leeway_secs as f64,
            ttl: Duration::from_secs(cfg.jwks_ttl_secs),
            refetch: Duration::from_secs(cfg.jwks_refetch_secs),
            timeout,
            fetcher: Fetcher::new(timeout)?,
            keys: RwLock::new(KeySet::default()),
            refresh: tokio::sync::Mutex::new(Refresh {
                last_attempt: None,
                jwks_url: cfg.jwks_url.clone(),
            }),
        })
    }

    /// The URL of the Protected Resource Metadata named in the challenge.
    pub fn resource_metadata_url(&self) -> &str {
        &self.resource_metadata_url
    }

    /// The RFC 9728 document.
    fn metadata_document(&self) -> Value {
        let mut doc = json!({
            "resource": self.resource,
            "authorization_servers": [self.issuer],
            "bearer_methods_supported": ["header"],
            "resource_name": "pscat-mcp",
        });
        if !self.scopes.is_empty() {
            doc["scopes_supported"] = json!(self.scopes);
        }
        doc
    }

    /// The key `kid` if the cache holds it, and whether the cache is fresh.
    fn cached(&self, kid: &str) -> (Option<Arc<Jwk>>, bool) {
        let set = self.keys.read().unwrap_or_else(|e| e.into_inner());
        let fresh = set.fetched.is_some_and(|t| t.elapsed() < self.ttl);
        (set.keys.get(kid).cloned(), fresh)
    }

    /// The key named `kid`, fetching the JWKS when the cache is stale or does
    /// not know `kid`, at most once per refetch interval. Bounded by the
    /// timeout as a whole, waiting for another fetch included; a request
    /// holding a stale but known key never waits for another's refresh.
    async fn key(&self, kid: &str) -> Result<Arc<Jwk>, Reject> {
        let (found, fresh) = self.cached(kid);
        if let (Some(k), true) = (&found, fresh) {
            return Ok(k.clone());
        }
        let lookup = async {
            let refresh = match (&found, self.refresh.try_lock()) {
                (_, Ok(guard)) => guard,
                // A key already held, only past its TTL: never wait for
                // another request's refresh.
                (Some(k), Err(_)) => return Ok(k.clone()),
                (None, Err(_)) => self.refresh.lock().await,
            };
            self.refresh_for(refresh, kid).await
        };
        match tokio::time::timeout(self.timeout, lookup).await {
            Ok(r) => r,
            Err(_) => {
                // Still usable if an older fetch knew it.
                self.cached(kid).0.ok_or(Reject::JwksUnavailable)
            }
        }
    }

    /// With the refresh lock held: fetch the JWKS if allowed, then look up
    /// `kid`.
    async fn refresh_for(
        &self,
        mut refresh: tokio::sync::MutexGuard<'_, Refresh>,
        kid: &str,
    ) -> Result<Arc<Jwk>, Reject> {
        // Someone else may have fetched while this one waited.
        let (found, fresh) = self.cached(kid);
        if let (Some(k), true) = (&found, fresh) {
            return Ok(k.clone());
        }
        let may_fetch = refresh
            .last_attempt
            .is_none_or(|t| t.elapsed() >= self.refetch);
        if !may_fetch {
            return found.ok_or(Reject::UnknownKid);
        }
        // Stamped before the fetch, so a hung or failing issuer is asked at
        // most once per interval.
        refresh.last_attempt = Some(Instant::now());
        match self.fetch_keys(&mut refresh).await {
            Ok(keys) => {
                let n = keys.len();
                let mut set = self.keys.write().unwrap_or_else(|e| e.into_inner());
                set.keys = keys;
                set.fetched = Some(Instant::now());
                tracing::debug!(keys = n, "fetched the OAuth issuer's JWKS");
                set.keys.get(kid).cloned().ok_or(Reject::UnknownKid)
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetching the OAuth issuer's JWKS failed");
                found.ok_or(Reject::JwksUnavailable)
            }
        }
    }

    async fn fetch_keys(&self, refresh: &mut Refresh) -> Result<HashMap<String, Arc<Jwk>>, String> {
        let url = match &refresh.jwks_url {
            Some(u) => u.clone(),
            None => {
                let u = self.discover().await?;
                refresh.jwks_url = Some(u.clone());
                u
            }
        };
        let doc = self.fetcher.get_json(&url).await?;
        let keys = parse_jwks(&doc);
        if keys.is_empty() {
            return Err("the JWKS holds no usable signing key".into());
        }
        Ok(keys)
    }

    /// The `jwks_uri` from the issuer's metadata, whose `issuer` must match.
    async fn discover(&self) -> Result<String, String> {
        let mut last = String::from("no metadata URL");
        for url in discovery_urls(&self.issuer) {
            match self.fetcher.get_json(&url).await {
                Ok(doc) => {
                    if doc["issuer"].as_str() != Some(self.issuer.as_str()) {
                        return Err(format!("the metadata at {url} names another issuer"));
                    }
                    let jwks = doc["jwks_uri"]
                        .as_str()
                        .ok_or_else(|| format!("the metadata at {url} has no jwks_uri"))?;
                    checked_url("jwks_uri", jwks, true).map_err(|e| e.to_string())?;
                    return Ok(jwks.to_string());
                }
                Err(e) => last = format!("{url}: {e}"),
            }
        }
        Err(format!("discovering the issuer's metadata failed ({last})"))
    }

    /// Validate `token`; see the module docs for the order of the checks.
    async fn validate(&self, token: &str) -> Result<Principal, Reject> {
        let parsed = parse_token(token)?;
        let key = self.key(&parsed.kid).await?;
        if !key.verify(parsed.alg, parsed.signed, &parsed.signature) {
            return Err(Reject::BadSignature);
        }
        let c = &parsed.claims;
        if c["iss"].as_str() != Some(self.issuer.as_str()) {
            return Err(Reject::WrongIssuer);
        }
        let want = trim_slash(&self.resource);
        let aud_ok = match &c["aud"] {
            Value::String(a) => trim_slash(a) == want,
            Value::Array(a) => a
                .iter()
                .any(|v| v.as_str().is_some_and(|a| trim_slash(a) == want)),
            _ => false,
        };
        if !aud_ok {
            return Err(Reject::WrongAudience);
        }
        let now = now_secs();
        match c["exp"].as_f64() {
            Some(exp) if exp.is_finite() && now <= exp + self.leeway => {}
            _ => return Err(Reject::Expired),
        }
        match &c["nbf"] {
            Value::Null => {}
            v => match v.as_f64() {
                Some(nbf) if nbf.is_finite() && nbf <= now + self.leeway => {}
                _ => return Err(Reject::NotYetValid),
            },
        }
        let sub = c["sub"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(Reject::MissingSubject)?;
        if !self.scopes.is_empty() {
            let granted = granted_scopes(c);
            if !self.scopes.iter().all(|s| granted.contains(s.as_str())) {
                return Err(Reject::InsufficientScope);
            }
        }
        if !self.allow.allows(c, sub) {
            return Err(Reject::NotAllowed);
        }
        Ok(Principal::new(
            OauthAuth::NAME,
            principal_id(&self.issuer, sub),
        ))
    }
}

/// `oauth:` and 128 bits of SHA-256 over issuer and subject: stable per user,
/// disjoint from the other methods' ids, and free of personal data.
pub fn principal_id(issuer: &str, sub: &str) -> String {
    let mut h = Sha256::new();
    h.update(issuer.as_bytes());
    h.update([0u8]);
    h.update(sub.as_bytes());
    let digest = h.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("oauth:{hex}")
}

impl AuthMethod for OauthAuth {
    fn name(&self) -> &'static str {
        OauthAuth::NAME
    }

    /// No `Authorization` header is not applicable; another scheme is
    /// malformed (as for `bearer`); a bearer token is validated as a JWT.
    fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome> {
        Box::pin(async move {
            let token = match bearer_token(&parts.headers) {
                Ok(t) => t,
                Err(AuthError::Missing) => return Outcome::NotApplicable,
                Err(e) => return Outcome::Rejected(e),
            };
            match self.validate(token).await {
                Ok(p) => Outcome::Accepted(p),
                Err(r) => {
                    if r == Reject::NotAJwt {
                        // Likely a static token meant for `bearer`.
                        tracing::debug!(reason = r.reason(), "oauth: token rejected");
                    } else {
                        tracing::info!(reason = r.reason(), "oauth: token rejected");
                    }
                    Outcome::Rejected(r.error())
                }
            }
        })
    }

    /// `Bearer realm="pscat-mcp", resource_metadata="…"`, with `scope` when
    /// scopes are configured, and `error` naming what this method rejected
    /// (`invalid_token`, `invalid_request` or `insufficient_scope`). A user
    /// who is not on the allowlist gets no `error`: `insufficient_scope`
    /// would send the client into a re-consent loop that cannot help.
    fn challenges(&self, rejection: Option<&AuthError>) -> Vec<Challenge> {
        let mut c = Challenge::new("Bearer")
            .param("realm", "pscat-mcp")
            .param("resource_metadata", self.resource_metadata_url.clone());
        match rejection {
            Some(AuthError::InvalidToken) => c = c.param("error", "invalid_token"),
            Some(AuthError::Malformed) => c = c.param("error", "invalid_request"),
            Some(AuthError::InsufficientScope) => c = c.param("error", "insufficient_scope"),
            _ => {}
        }
        if !self.scopes.is_empty() {
            c = c.param("scope", self.scopes.join(" "));
        }
        vec![c]
    }

    fn resource_metadata(&self) -> Option<ResourceMetadata> {
        Some(ResourceMetadata {
            paths: self.metadata_paths.clone(),
            document: self.metadata_document(),
        })
    }

    fn summary(&self) -> String {
        format!(
            "oauth (issuer {}, resource {}, {} allowed user(s))",
            self.issuer,
            self.resource,
            self.allow.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: &Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    }

    #[test]
    fn discovery_urls_follow_the_mcp_order() {
        assert_eq!(
            discovery_urls("https://a.example"),
            [
                "https://a.example/.well-known/oauth-authorization-server",
                "https://a.example/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            discovery_urls("https://a.example/"),
            discovery_urls("https://a.example")
        );
        assert_eq!(
            discovery_urls("https://a.example/tenant1/"),
            [
                "https://a.example/.well-known/oauth-authorization-server/tenant1",
                "https://a.example/.well-known/openid-configuration/tenant1",
                "https://a.example/tenant1/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn urls_need_https_off_loopback() {
        for ok in [
            "https://a.example/x",
            "http://127.0.0.1:9/x",
            "http://localhost/",
            "http://[::1]:8/",
        ] {
            assert!(checked_url("u", ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "http://a.example/",
            "ftp://a.example/",
            "a.example",
            "https://a.example/#f",
            "https://a.example/?q=1",
            "/relative",
        ] {
            assert!(checked_url("u", bad, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_refuses_none_and_hmac_before_keys() {
        let body = enc(&json!({"sub": "x"}));
        for alg in ["none", "HS256", "HS384", "HS512", "", "rs256"] {
            let t = format!("{}.{body}.", enc(&json!({"alg": alg, "kid": "k"})));
            assert_eq!(parse_token(&t).err(), Some(Reject::UnsupportedAlg), "{alg}");
        }
        let t = format!("{}.{body}.", enc(&json!({"kid": "k"})));
        assert_eq!(parse_token(&t).err(), Some(Reject::UnsupportedAlg));
        let t = format!(
            "{}.{body}.AA",
            enc(&json!({"alg": "RS256", "kid": "k", "crit": ["exp"]}))
        );
        assert_eq!(parse_token(&t).err(), Some(Reject::UnsupportedAlg));
        let t = format!("{}.{body}.AA", enc(&json!({"alg": "RS256"})));
        assert_eq!(parse_token(&t).err(), Some(Reject::MissingKid));
        for t in ["abc", "a.b", "a.b.c.d", "!!.!!.!!", ""] {
            assert_eq!(parse_token(t).err(), Some(Reject::NotAJwt), "{t}");
        }
        let big = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert_eq!(parse_token(&big).err(), Some(Reject::TooLarge));
        let t = format!("{}.{body}.AA", enc(&json!({"alg": "ES256", "kid": "k"})));
        let p = parse_token(&t).unwrap();
        assert_eq!((p.alg, p.kid.as_str()), (Alg::ES256, "k"));
        assert_eq!(p.signed, &t.as_bytes()[..t.rfind('.').unwrap()]);
    }

    #[test]
    fn jwks_parsing_skips_unusable_keys() {
        let b = |n: usize| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vec![7u8; n]);
        let doc = json!({"keys": [
            {"kty": "RSA", "kid": "r", "n": b(256), "e": "AQAB"},
            {"kty": "RSA", "n": b(256), "e": "AQAB"},
            {"kty": "RSA", "kid": "enc", "use": "enc", "n": b(256), "e": "AQAB"},
            {"kty": "EC", "kid": "e", "crv": "P-256", "x": b(32), "y": b(32)},
            {"kty": "EC", "kid": "short", "crv": "P-256", "x": b(31), "y": b(32)},
            {"kty": "EC", "kid": "k1", "crv": "secp256k1", "x": b(32), "y": b(32)},
            {"kty": "oct", "kid": "h", "k": b(32)},
            {"kty": "OKP", "kid": "d", "crv": "Ed25519", "x": b(32)},
        ]});
        let keys = parse_jwks(&doc);
        let mut kids: Vec<&str> = keys.keys().map(String::as_str).collect();
        kids.sort();
        assert_eq!(kids, ["d", "e", "r"]);
        assert!(matches!(keys["e"].material, KeyMaterial::P256(ref p) if p.len() == 65));
        // A key of the wrong type never verifies, whatever the bytes.
        assert!(!keys["e"].verify(Alg::RS256, b"m", b"s"));
        assert!(!keys["r"].verify(Alg::ES256, b"m", b"s"));
        assert!(parse_jwks(&json!({"keys": "nope"})).is_empty());
    }

    #[test]
    fn allowlist_entries() {
        let cfg = |entries: &[&str]| OauthConfig {
            allowed_users: entries.iter().map(|s| s.to_string()).collect(),
            ..OauthConfig::default()
        };
        let list = cfg(&["sub:abc", " email:Alice@Example.com ", ""])
            .load_allowlist()
            .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.allows(&json!({}), "abc"));
        assert!(!list.allows(&json!({}), "abd"));
        let verified = json!({"email": "alice@example.COM", "email_verified": true});
        assert!(list.allows(&verified, "zzz"));
        for unverified in [
            json!({"email": "alice@example.com"}),
            json!({"email": "alice@example.com", "email_verified": false}),
            json!({"email": "alice@example.com", "email_verified": "true"}),
        ] {
            assert!(!list.allows(&unverified, "zzz"), "{unverified}");
        }
        for bad in [&["alice@example.com"][..], &["sub:"], &["email:nobody"]] {
            let err = cfg(bad).load_allowlist().unwrap_err().to_string();
            assert!(!err.contains("alice"), "{err}");
        }
    }

    #[test]
    fn principal_ids_are_pseudonymous_and_prefixed() {
        let a = principal_id("https://i.example/", "user-1");
        assert!(
            a.starts_with("oauth:") && a.len() == "oauth:".len() + 32,
            "{a}"
        );
        assert!(!a.contains("user-1"));
        assert_eq!(a, principal_id("https://i.example/", "user-1"));
        assert_ne!(a, principal_id("https://i.example/", "user-2"));
        assert_ne!(a, principal_id("https://other.example/", "user-1"));
    }

    #[test]
    fn scopes_from_either_claim() {
        let c = json!({"scope": "a b  c"});
        let g = granted_scopes(&c);
        assert!(g.contains("a") && g.contains("c") && g.len() == 3);
        let c = json!({"scp": ["x", 3, "y"]});
        assert_eq!(granted_scopes(&c).len(), 2);
    }
}
