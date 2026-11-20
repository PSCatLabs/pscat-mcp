//! The `oauth` auth method (issue #119) against a local mock authorization
//! server: token validation (valid RS256/ES256 tokens; expired, wrong
//! audience, wrong issuer, bad signature, an HS256 token signed with the
//! public key, `alg: none`, ...), the allowlist's `403`, scopes, `bearer` and
//! `oauth` side by side with one merged challenge, the Protected Resource
//! Metadata routes, the JWKS cache (rate-limited refetches, a hung issuer),
//! discovery, the startup refusals and the logs, through the router and the
//! server binary.
//!
//! The mock authorization server is an axum app on a loopback port (plain
//! `http` is accepted for loopback hosts only). It signs nothing itself: the
//! tests sign tokens with the keys whose public halves it serves.

use std::net::{IpAddr, Ipv4Addr};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine as _;
use pscat_mcp::auth::Authenticator;
use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::{AuthConfig, HttpConfig, LimitsConfig};
use pscat_mcp::http::{authenticator_with, mcp_server_config, router};
use pscat_mcp::oauth::{OauthAuth, OauthConfig};
use pscat_mcp::render::RenderService;
use ring::rand::SystemRandom;
use ring::signature::{self as sig, EcdsaKeyPair, KeyPair as _, RsaKeyPair};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const ACCEPT: &str = "application/json, text/event-stream";
const STATIC_TOKEN: &str = "oauth-test-static-token-0123456789";
const RESOURCE: &str = "https://mcp.example.test/mcp";
const META_URL: &str = "https://mcp.example.test/.well-known/oauth-protected-resource/mcp";
/// Allowlisted by subject.
const ALICE: &str = "alice-subject-4f9c2";
/// Allowlisted by (verified) email.
const CAROL_EMAIL: &str = "carol.oauth-test@example.com";
/// Signs in fine, but is on no list.
const BOB: &str = "bob-subject-77e1a";
/// A test-only RSA key (PKCS#1 DER), generated for these tests.
const RSA_DER: &[u8] = include_bytes!("data/oauth-test-rsa-2048.der");

// --------------------------------------------------------------- keys

struct Keys {
    rsa: RsaKeyPair,
    ec: EcdsaKeyPair,
    /// Not in the JWKS: signs "bad signature" tokens.
    rogue: EcdsaKeyPair,
    /// Served only once a test rotates it in.
    ec2: EcdsaKeyPair,
}

fn ec_key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pk8 = EcdsaKeyPair::generate_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, pk8.as_ref(), &rng).unwrap()
}

fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| Keys {
        rsa: RsaKeyPair::from_der(RSA_DER).unwrap(),
        ec: ec_key(),
        rogue: ec_key(),
        ec2: ec_key(),
    })
}

fn b64(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn rsa_jwk() -> Value {
    let c = sig::RsaPublicKeyComponents::<Vec<u8>>::from(keys().rsa.public());
    json!({"kty": "RSA", "kid": "rsa-1", "alg": "RS256", "use": "sig", "n": b64(&c.n), "e": b64(&c.e)})
}

fn ec_jwk(key: &EcdsaKeyPair, kid: &str) -> Value {
    let p = key.public_key().as_ref();
    json!({"kty": "EC", "kid": kid, "crv": "P-256", "x": b64(&p[1..33]), "y": b64(&p[33..65])})
}

fn jwks() -> Value {
    json!({"keys": [rsa_jwk(), ec_jwk(&keys().ec, "ec-1")]})
}

/// Which key signs a token.
#[derive(Clone, Copy)]
enum Signer {
    Rs256,
    Es256,
    Rogue,
    Ec2,
}

fn sign(signer: Signer, kid: &str, claims: &Value) -> String {
    let alg = match signer {
        Signer::Rs256 => "RS256",
        _ => "ES256",
    };
    let header = json!({"alg": alg, "typ": "at+jwt", "kid": kid});
    let input = format!("{}.{}", b64(header.to_string()), b64(claims.to_string()));
    let rng = SystemRandom::new();
    let signature = match signer {
        Signer::Rs256 => {
            let k = &keys().rsa;
            let mut out = vec![0; k.public().modulus_len()];
            k.sign(&sig::RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut out)
                .unwrap();
            out
        }
        Signer::Es256 | Signer::Rogue | Signer::Ec2 => {
            let k = match signer {
                Signer::Es256 => &keys().ec,
                Signer::Rogue => &keys().rogue,
                _ => &keys().ec2,
            };
            k.sign(&rng, input.as_bytes()).unwrap().as_ref().to_vec()
        }
    };
    format!("{input}.{}", b64(signature))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(issuer: &str, sub: &str) -> Value {
    json!({
        "iss": issuer,
        "aud": RESOURCE,
        "sub": sub,
        "iat": now(),
        "exp": now() + 300,
    })
}

fn with(mut c: Value, f: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    f(c.as_object_mut().unwrap());
    c
}

// ----------------------------------------------- mock authorization server

#[derive(Default)]
struct Mock {
    base: String,
    /// Requests to any route.
    hits: AtomicUsize,
    jwks_fetches: AtomicUsize,
    /// Every route hangs forever.
    hang: AtomicBool,
    /// Serve RFC 8414 metadata (else 404; OpenID Connect only).
    no_rfc8414: AtomicBool,
    /// The `issuer` the metadata claims (default: the real one).
    metadata_issuer: Mutex<Option<String>>,
    jwks: Mutex<Value>,
}

struct MockSrv {
    mock: Arc<Mock>,
    _stop: CancellationToken,
}

impl std::ops::Deref for MockSrv {
    type Target = Mock;
    fn deref(&self) -> &Mock {
        &self.mock
    }
}

impl Drop for MockSrv {
    fn drop(&mut self) {
        self._stop.cancel();
    }
}

async fn gate(m: &Mock) {
    m.hits.fetch_add(1, Ordering::SeqCst);
    if m.hang.load(Ordering::SeqCst) {
        std::future::pending::<()>().await;
    }
}

fn metadata(m: &Mock) -> Value {
    let issuer = m
        .metadata_issuer
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| m.base.clone());
    json!({
        "issuer": issuer,
        "jwks_uri": format!("{}/jwks", m.base),
        "authorization_endpoint": format!("{}/authorize", m.base),
        "token_endpoint": format!("{}/token", m.base),
        "code_challenge_methods_supported": ["S256"],
    })
}

async fn mock_as() -> MockSrv {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = Arc::new(Mock {
        base,
        jwks: Mutex::new(jwks()),
        ..Mock::default()
    });
    let app = Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(|State(m): State<Arc<Mock>>| async move {
                gate(&m).await;
                if m.no_rfc8414.load(Ordering::SeqCst) {
                    return http::StatusCode::NOT_FOUND.into_response();
                }
                axum::Json(metadata(&m)).into_response()
            }),
        )
        .route(
            "/.well-known/openid-configuration",
            get(|State(m): State<Arc<Mock>>| async move {
                gate(&m).await;
                axum::Json(metadata(&m)).into_response()
            }),
        )
        .route(
            "/jwks",
            get(|State(m): State<Arc<Mock>>| async move {
                gate(&m).await;
                m.jwks_fetches.fetch_add(1, Ordering::SeqCst);
                let doc = m.jwks.lock().unwrap().clone();
                axum::Json(doc).into_response()
            }),
        )
        .with_state(mock.clone());
    let stop = CancellationToken::new();
    let s = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(s.cancelled_owned())
            .await;
    });
    MockSrv { mock, _stop: stop }
}

// ------------------------------------------------------------ the server

fn http_config() -> HttpConfig {
    HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        auth_tokens: vec![],
        auth_token_file: None,
        allow_unauthenticated: false,
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        allowed_origins: vec![],
        shutdown_grace_secs: 1,
        async_jobs: false,
    }
}

fn oauth_config(m: &Mock) -> OauthConfig {
    OauthConfig {
        issuer: Some(m.base.clone()),
        resource: Some(RESOURCE.into()),
        allowed_users: vec![format!("sub:{ALICE}"), format!("email:{CAROL_EMAIL}")],
        jwks_timeout_ms: 2000,
        leeway_secs: 5,
        ..OauthConfig::default()
    }
}

fn auth_config(list: &str, oauth: OauthConfig) -> AuthConfig {
    AuthConfig {
        auth: Some(list.into()),
        oauth,
        ..AuthConfig::default()
    }
}

fn authn(list: &str, oauth: OauthConfig) -> Arc<dyn Authenticator> {
    let mut http = http_config();
    http.auth_tokens = vec![STATIC_TOKEN.into()];
    authenticator_with(&http, &auth_config(list, oauth)).unwrap()
}

fn service() -> Arc<RenderService> {
    let limits = LimitsConfig {
        interp_time_limit_ms: Some(0),
        max_steps: 0,
        max_concurrent_renders: Some(2),
        ..LimitsConfig::default()
    };
    let backend = SubprocessBackend::new(SERVER.into(), limits.clone());
    Arc::new(RenderService::new(Arc::new(backend), limits))
}

struct Srv {
    base: String,
    url: String,
    ct: CancellationToken,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

async fn serve(service: Arc<RenderService>, auth: Arc<dyn Authenticator>) -> Srv {
    let ct = CancellationToken::new();
    let app = router(
        service,
        auth,
        mcp_server_config(&http_config(), ct.child_token()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await;
    });
    Srv {
        base: format!("http://{addr}"),
        url: format!("http://{addr}/mcp"),
        ct,
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "oauth-test", "version": "1.0" }
        }
    })
}

fn render_call() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "render_postscript",
            "arguments": { "source": "0 0 moveto 10 10 lineto stroke showpage", "width": 20, "height": 20 }
        }
    })
}

#[derive(Debug, PartialEq)]
struct Seen {
    status: u16,
    challenge: Option<String>,
    body: String,
}

impl Seen {
    fn error(&self) -> Value {
        serde_json::from_str::<Value>(&self.body).unwrap_or(Value::Null)["error"].clone()
    }
}

async fn post_auth(url: &str, authorization: Option<&str>, body: &Value) -> Seen {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .body(body.to_string());
    if let Some(a) = authorization {
        req = req.header("Authorization", a);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string());
    Seen {
        status,
        challenge,
        body: resp.text().await.unwrap(),
    }
}

async fn post(url: &str, token: &str, body: &Value) -> Seen {
    post_auth(url, Some(&format!("Bearer {token}")), body).await
}

fn challenge(error: Option<&str>) -> String {
    let base = format!(r#"Bearer realm="pscat-mcp", resource_metadata="{META_URL}""#);
    match error {
        Some(e) => format!(r#"{base}, error="{e}""#),
        None => base,
    }
}

fn executed(svc: &RenderService) -> u64 {
    svc.counters.executed.load(Ordering::SeqCst)
}

// ------------------------------------------------------------------ tests

/// Every token the method must refuse with `401 invalid_token`, against
/// `issuer`.
fn invalid_tokens(issuer: &str) -> Vec<(&'static str, String)> {
    let good = claims(issuer, ALICE);
    let hs256_with_public_key = {
        // The classic key-confusion attack: HMAC keyed with the public key.
        let header = json!({"alg": "HS256", "typ": "JWT", "kid": "rsa-1"});
        let input = format!("{}.{}", b64(header.to_string()), b64(good.to_string()));
        let secret = rsa_jwk().to_string();
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let tag = ring::hmac::sign(&key, input.as_bytes());
        format!("{input}.{}", b64(tag.as_ref()))
    };
    let alg_none = format!(
        "{}.{}.",
        b64(json!({"alg": "none", "kid": "rsa-1"}).to_string()),
        b64(good.to_string())
    );
    let tampered = {
        let t = sign(Signer::Es256, "ec-1", &claims(issuer, BOB));
        let mut parts: Vec<&str> = t.split('.').collect();
        let payload = b64(good.to_string());
        parts[1] = &payload;
        parts.join(".")
    };
    vec![
        (
            "expired",
            sign(
                Signer::Rs256,
                "rsa-1",
                &with(good.clone(), |c| {
                    c.insert("exp".into(), json!(now() - 3600));
                }),
            ),
        ),
        (
            "no exp",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.remove("exp");
                }),
            ),
        ),
        (
            "not yet valid",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("nbf".into(), json!(now() + 3600));
                }),
            ),
        ),
        (
            "wrong audience",
            sign(
                Signer::Rs256,
                "rsa-1",
                &with(good.clone(), |c| {
                    c.insert("aud".into(), json!("https://other.example.test/mcp"));
                }),
            ),
        ),
        (
            "no audience",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.remove("aud");
                }),
            ),
        ),
        (
            "wrong issuer",
            sign(
                Signer::Rs256,
                "rsa-1",
                &with(good.clone(), |c| {
                    c.insert("iss".into(), json!("https://evil.example.test"));
                }),
            ),
        ),
        ("bad signature", sign(Signer::Rogue, "ec-1", &good)),
        ("tampered payload", tampered),
        (
            "ES256 header on the RSA key",
            sign(Signer::Es256, "rsa-1", &good),
        ),
        ("HS256 signed with the public key", hs256_with_public_key),
        ("alg none", alg_none),
        ("unknown kid", sign(Signer::Es256, "nope", &good)),
        (
            "no sub",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.remove("sub");
                }),
            ),
        ),
        (
            "empty sub",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("sub".into(), json!(""));
                }),
            ),
        ),
        ("not a JWT", "abc.def".into()),
        ("too large", "a".repeat(9000)),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_validated() {
    let m = mock_as().await;
    let svc = service();
    let srv = serve(svc.clone(), authn("oauth", oauth_config(&m))).await;

    // Valid: RS256 and ES256 for the allowlisted subject, an audience array,
    // and the verified email.
    let good = claims(&m.base, ALICE);
    for (what, token) in [
        ("RS256", sign(Signer::Rs256, "rsa-1", &good)),
        ("ES256", sign(Signer::Es256, "ec-1", &good)),
        (
            "aud array",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("aud".into(), json!(["https://x.example", RESOURCE]));
                }),
            ),
        ),
        (
            "verified email",
            sign(
                Signer::Es256,
                "ec-1",
                &with(claims(&m.base, "carol-sub"), |c| {
                    c.insert("email".into(), json!(CAROL_EMAIL.to_uppercase()));
                    c.insert("email_verified".into(), json!(true));
                }),
            ),
        ),
    ] {
        let seen = post(&srv.url, &token, &initialize()).await;
        assert_eq!(seen.status, 200, "{what}: {seen:?}");
    }

    // Invalid: 401 invalid_token, and nothing runs.
    for (what, token) in invalid_tokens(&m.base) {
        let seen = post(&srv.url, &token, &render_call()).await;
        assert_eq!(seen.status, 401, "{what}: {seen:?}");
        assert_eq!(seen.error(), "invalid_token", "{what}");
        assert_eq!(
            seen.challenge.as_deref(),
            Some(challenge(Some("invalid_token")).as_str()),
            "{what}"
        );
        assert!(token.len() < 10 || !seen.body.contains(&token), "{what}");
    }

    // No credentials: the challenge without an error.
    let seen = post_auth(&srv.url, None, &render_call()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(seen.error(), "missing_credentials");
    assert_eq!(seen.challenge.as_deref(), Some(challenge(None).as_str()));
    // Another scheme.
    let seen = post_auth(&srv.url, Some("Basic dGVzdA=="), &render_call()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(
        seen.challenge.as_deref(),
        Some(challenge(Some("invalid_request")).as_str())
    );

    // Signed in, not allowed: 403 without an error parameter.
    for (what, c) in [
        ("unknown subject", claims(&m.base, BOB)),
        (
            "unverified email",
            with(claims(&m.base, "carol-sub"), |c| {
                c.insert("email".into(), json!(CAROL_EMAIL));
                c.insert("email_verified".into(), json!(false));
            }),
        ),
        (
            "email_verified as a string",
            with(claims(&m.base, "carol-sub"), |c| {
                c.insert("email".into(), json!(CAROL_EMAIL));
                c.insert("email_verified".into(), json!("true"));
            }),
        ),
        (
            "email without email_verified",
            with(claims(&m.base, "carol-sub"), |c| {
                c.insert("email".into(), json!(CAROL_EMAIL));
            }),
        ),
    ] {
        let seen = post(&srv.url, &sign(Signer::Es256, "ec-1", &c), &render_call()).await;
        assert_eq!(seen.status, 403, "{what}: {seen:?}");
        assert_eq!(seen.error(), "not_allowed", "{what}");
        assert_eq!(
            seen.challenge.as_deref(),
            Some(challenge(None).as_str()),
            "{what}"
        );
    }
    assert_eq!(executed(&svc), 0, "no PostScript ran for a refused request");

    // An accepted token does reach the tools.
    let seen = post(
        &srv.url,
        &sign(Signer::Es256, "ec-1", &good),
        &render_call(),
    )
    .await;
    assert_eq!(seen.status, 200, "{seen:?}");
    assert_eq!(executed(&svc), 1);
    // One discovery and one JWKS fetch served everything above.
    assert_eq!(m.jwks_fetches.load(Ordering::SeqCst), 1);
}

/// `verified:*` (issue #160) admits any subject whose token says
/// `email_verified: true`, with or without an `email`, next to the other
/// entries; every other check still applies.
#[tokio::test(flavor = "multi_thread")]
async fn verified_star_admits_any_verified_email() {
    let m = mock_as().await;
    let svc = service();
    let mut oauth = oauth_config(&m);
    oauth.allowed_users = vec![format!("sub:{ALICE}"), "verified:*".into()];
    let srv = serve(svc.clone(), authn("oauth", oauth)).await;
    let verified = |sub: &str| {
        with(claims(&m.base, sub), |c| {
            c.insert("email_verified".into(), json!(true));
        })
    };

    // Any subject with a verified email, with or without the `email`
    // claim; the `sub:` entry still admits ALICE without one.
    for (what, c) in [
        ("verified, no email", verified(BOB)),
        (
            "verified with an email",
            with(verified("dave-subject-31"), |c| {
                c.insert("email".into(), json!("dave@example.com"));
            }),
        ),
        ("sub: entry, no email_verified", claims(&m.base, ALICE)),
    ] {
        let seen = post(&srv.url, &sign(Signer::Es256, "ec-1", &c), &initialize()).await;
        assert_eq!(seen.status, 200, "{what}: {seen:?}");
    }

    // Not vouched for: 403 not_allowed.
    for (what, c) in [
        ("no email_verified", claims(&m.base, BOB)),
        (
            "email_verified false",
            with(claims(&m.base, BOB), |c| {
                c.insert("email_verified".into(), json!(false));
            }),
        ),
        (
            "email_verified as a string",
            with(claims(&m.base, BOB), |c| {
                c.insert("email_verified".into(), json!("true"));
            }),
        ),
        (
            "email_verified as a number",
            with(claims(&m.base, BOB), |c| {
                c.insert("email_verified".into(), json!(1));
            }),
        ),
    ] {
        let seen = post(&srv.url, &sign(Signer::Rs256, "rsa-1", &c), &render_call()).await;
        assert_eq!(seen.status, 403, "{what}: {seen:?}");
        assert_eq!(seen.error(), "not_allowed", "{what}");
    }

    // Every other check still applies, to a verified token too: 401.
    let good = verified(BOB);
    for (what, token) in [
        ("bad signature", sign(Signer::Rogue, "ec-1", &good)),
        (
            "wrong audience",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("aud".into(), json!("https://other.example.test/mcp"));
                }),
            ),
        ),
        (
            "wrong issuer",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("iss".into(), json!("https://evil.example.test"));
                }),
            ),
        ),
        (
            "expired",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("exp".into(), json!(now() - 3600));
                }),
            ),
        ),
        (
            "empty sub",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.insert("sub".into(), json!(""));
                }),
            ),
        ),
        (
            "no sub",
            sign(
                Signer::Es256,
                "ec-1",
                &with(good.clone(), |c| {
                    c.remove("sub");
                }),
            ),
        ),
    ] {
        let seen = post(&srv.url, &token, &render_call()).await;
        assert_eq!(seen.status, 401, "{what}: {seen:?}");
        assert_eq!(seen.error(), "invalid_token", "{what}");
    }
    // The shared invalid set too.
    for (what, token) in invalid_tokens(&m.base) {
        let seen = post(&srv.url, &token, &render_call()).await;
        assert_eq!(seen.status, 401, "{what}: {seen:?}");
    }
    assert_eq!(executed(&svc), 0, "no PostScript ran for a refused request");
}

#[tokio::test(flavor = "multi_thread")]
async fn scopes_are_required_and_advertised() {
    let m = mock_as().await;
    let mut cfg = oauth_config(&m);
    cfg.scopes = vec!["pscat.render".into(), "openid".into()];
    let auth = authn("oauth", cfg);
    let meta = auth.published_metadata().unwrap();
    assert_eq!(
        meta.document["scopes_supported"],
        json!(["pscat.render", "openid"])
    );
    let srv = serve(service(), auth).await;
    let want = format!(r#"{}, scope="pscat.render openid""#, challenge(None));
    let seen = post_auth(&srv.url, None, &initialize()).await;
    assert_eq!(seen.challenge.as_deref(), Some(want.as_str()));

    let base = claims(&m.base, ALICE);
    for (scopes, status) in [
        (json!({"scope": "openid pscat.render email"}), 200),
        (json!({"scp": ["pscat.render", "openid"]}), 200),
        (json!({"scope": "openid"}), 403),
        (json!({}), 403),
    ] {
        let c = with(base.clone(), |c| {
            for (k, v) in scopes.as_object().unwrap() {
                c.insert(k.clone(), v.clone());
            }
        });
        let seen = post(&srv.url, &sign(Signer::Rs256, "rsa-1", &c), &initialize()).await;
        assert_eq!(seen.status, status, "{scopes}: {seen:?}");
        if status == 403 {
            assert_eq!(seen.error(), "insufficient_scope");
            assert_eq!(
                seen.challenge.as_deref(),
                Some(
                    format!(
                        r#"Bearer realm="pscat-mcp", resource_metadata="{META_URL}", error="insufficient_scope", scope="pscat.render openid""#
                    )
                    .as_str()
                )
            );
        }
    }
}

/// `bearer,oauth`: a static token (bearer), a JWT (oauth), garbage (401,
/// one merged challenge), and a JWT for a user not allowed (403).
#[tokio::test(flavor = "multi_thread")]
async fn bearer_and_oauth_side_by_side() {
    let m = mock_as().await;
    let svc = service();
    let auth = authn("bearer,oauth", oauth_config(&m));
    assert_eq!(auth.methods(), ["bearer", "oauth"]);
    let srv = serve(svc.clone(), auth).await;
    assert_eq!(
        post(&srv.url, STATIC_TOKEN, &initialize()).await.status,
        200
    );
    // A static token never touches the issuer.
    assert_eq!(m.hits.load(Ordering::SeqCst), 0);
    let jwt = sign(Signer::Rs256, "rsa-1", &claims(&m.base, ALICE));
    assert_eq!(post(&srv.url, &jwt, &initialize()).await.status, 200);

    for garbage in ["garbage", "a.b.c"] {
        let seen = post(&srv.url, garbage, &render_call()).await;
        assert_eq!(seen.status, 401, "{garbage}");
        // Both rejected it: bearer is first, so its wording is reported;
        // the code is `invalid_token` either way.
        assert_eq!(seen.error(), "invalid_token");
        assert_eq!(
            seen.challenge.as_deref(),
            Some(challenge(Some("invalid_token")).as_str())
        );
    }
    let seen = post_auth(&srv.url, None, &initialize()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(seen.challenge.as_deref(), Some(challenge(None).as_str()));

    // The 403 outranks bearer's 401.
    let bob = sign(Signer::Es256, "ec-1", &claims(&m.base, BOB));
    let seen = post(&srv.url, &bob, &render_call()).await;
    assert_eq!(seen.status, 403, "{seen:?}");
    assert_eq!(seen.error(), "not_allowed");
    assert_eq!(executed(&svc), 0);

    // The other order admits the same requests.
    let srv2 = serve(svc.clone(), authn("oauth,bearer", oauth_config(&m))).await;
    assert_eq!(
        post(&srv2.url, STATIC_TOKEN, &initialize()).await.status,
        200
    );
    assert_eq!(post(&srv2.url, &jwt, &initialize()).await.status, 200);
    let seen = post(&srv2.url, "garbage", &initialize()).await;
    assert_eq!(seen.status, 401);
}

/// The Protected Resource Metadata: served at both well-known paths without
/// authentication, and the `resource_metadata` URL of a real 401 is one of
/// them. Without `oauth` there is no such route.
#[tokio::test(flavor = "multi_thread")]
async fn metadata_is_served_without_auth() {
    let m = mock_as().await;
    let mut cfg = oauth_config(&m);
    cfg.scopes = vec!["pscat.render".into()];
    let srv = serve(service(), authn("bearer,oauth", cfg)).await;
    let expected = json!({
        "resource": RESOURCE,
        "authorization_servers": [m.base],
        "bearer_methods_supported": ["header"],
        "resource_name": "pscat-mcp",
        "scopes_supported": ["pscat.render"],
    });
    let seen = post_auth(&srv.url, None, &initialize()).await;
    let header = seen.challenge.unwrap();
    let url = header
        .split("resource_metadata=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap();
    assert_eq!(url, META_URL);
    let served_path = url.strip_prefix("https://mcp.example.test").unwrap();
    for path in [served_path, "/.well-known/oauth-protected-resource"] {
        let resp = reqwest::get(format!("{}{path}", srv.base)).await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(resp.headers()["access-control-allow-origin"], "*", "{path}");
        let doc: Value = resp.json().await.unwrap();
        assert_eq!(doc, expected, "{path}");
    }
    // The resource URL comes from the configuration, never from Host.
    let resp = reqwest::Client::new()
        .get(format!("{}/.well-known/oauth-protected-resource", srv.base))
        .header("Host", "evil.example.test")
        .send()
        .await
        .unwrap();
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["resource"], RESOURCE);
    assert_eq!(m.hits.load(Ordering::SeqCst), 0, "metadata needs no issuer");

    // Bearer only: no metadata route, and the challenge is unchanged.
    let mut http = http_config();
    http.auth_tokens = vec![STATIC_TOKEN.into()];
    let bearer = authenticator_with(&http, &auth_config("bearer", OauthConfig::default())).unwrap();
    assert!(bearer.published_metadata().is_none());
    let plain = serve(service(), bearer).await;
    for path in [served_path, "/.well-known/oauth-protected-resource"] {
        let resp = reqwest::get(format!("{}{path}", plain.base)).await.unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    let seen = post_auth(&plain.url, None, &initialize()).await;
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Bearer realm="pscat-mcp""#)
    );
}

/// A resource URL at the root has one metadata path.
#[test]
fn metadata_paths_follow_the_resource() {
    for (resource, paths, url) in [
        (
            "https://mcp.example.test/mcp",
            vec![
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource",
            ],
            "https://mcp.example.test/.well-known/oauth-protected-resource/mcp",
        ),
        (
            "https://mcp.example.test:8443/a/mcp/",
            vec![
                "/.well-known/oauth-protected-resource/a/mcp",
                "/.well-known/oauth-protected-resource",
            ],
            "https://mcp.example.test:8443/.well-known/oauth-protected-resource/a/mcp",
        ),
        (
            "https://mcp.example.test",
            vec!["/.well-known/oauth-protected-resource"],
            "https://mcp.example.test/.well-known/oauth-protected-resource",
        ),
    ] {
        let cfg = OauthConfig {
            issuer: Some("https://issuer.example.test/".into()),
            resource: Some(resource.into()),
            allowed_users: vec!["sub:x".into()],
            ..OauthConfig::default()
        };
        let a = OauthAuth::from_config(&cfg).unwrap();
        let meta = pscat_mcp::auth::AuthMethod::resource_metadata(&a).unwrap();
        assert_eq!(meta.paths, paths, "{resource}");
        assert_eq!(a.resource_metadata_url(), url);
        assert_eq!(meta.document["resource"], resource);
    }
}

/// A hung issuer: requests with a JWT give up after the JWKS timeout (all
/// at once, one fetch between them), and a request with a static token never
/// waits at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_issuer_does_not_hang_requests() {
    let m = mock_as().await;
    m.hang.store(true, Ordering::SeqCst);
    // Long enough that "did not wait for the issuer" (well under it) is
    // clear on a loaded machine.
    let timeout = Duration::from_millis(5000);
    let mut cfg = oauth_config(&m);
    cfg.jwks_timeout_ms = timeout.as_millis() as u64;
    let svc = service();
    let srv = Arc::new(serve(svc.clone(), authn("bearer,oauth", cfg)).await);

    let started = Instant::now();
    let mut jwt_requests = Vec::new();
    for i in 0..10 {
        let srv = srv.clone();
        let base = m.base.clone();
        jwt_requests.push(tokio::spawn(async move {
            let t = sign(
                Signer::Es256,
                &format!("garbage-{i}"),
                &claims(&base, ALICE),
            );
            let seen = post(&srv.url, &t, &render_call()).await;
            (seen, started.elapsed())
        }));
    }
    // While those wait on the issuer, a static token goes straight through.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = Instant::now();
    assert_eq!(
        post(&srv.url, STATIC_TOKEN, &initialize()).await.status,
        200
    );
    let static_took = t0.elapsed();
    assert!(
        static_took < Duration::from_millis(2500),
        "a static token waited {static_took:?}"
    );
    for r in jwt_requests {
        let (seen, took) = r.await.unwrap();
        assert_eq!(seen.status, 401, "{seen:?}");
        assert_eq!(seen.error(), "invalid_token");
        assert!(
            took < timeout + Duration::from_secs(3),
            "a JWT request took {took:?} with a {timeout:?} timeout"
        );
    }
    assert!(
        m.hits.load(Ordering::SeqCst) <= 1,
        "the issuer was asked {} times",
        m.hits.load(Ordering::SeqCst)
    );
    // Within the refetch interval nobody asks again, so a second wave is
    // refused at once.
    let t0 = Instant::now();
    let t = sign(Signer::Es256, "ec-1", &claims(&m.base, ALICE));
    assert_eq!(post(&srv.url, &t, &initialize()).await.status, 401);
    assert!(
        t0.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t0.elapsed()
    );
    assert!(m.hits.load(Ordering::SeqCst) <= 1);
    assert_eq!(executed(&svc), 0);
}

/// Requests without a JWT never wait for a hung issuer, even when `oauth`
/// is consulted first (`oauth,bearer`) or alone, while a JWT request is
/// waiting on the issuer.
#[tokio::test(flavor = "multi_thread")]
async fn requests_without_a_jwt_never_wait_through_oauth() {
    let m = mock_as().await;
    m.hang.store(true, Ordering::SeqCst);
    let mut cfg = oauth_config(&m);
    cfg.jwks_timeout_ms = 8000;
    let first = Arc::new(serve(service(), authn("oauth,bearer", cfg.clone())).await);
    let alone = serve(service(), authn("oauth", cfg)).await;
    // A JWT request holds the (hung) fetch.
    let waiting = {
        let srv = first.clone();
        let t = sign(Signer::Es256, "ec-1", &claims(&m.base, ALICE));
        tokio::spawn(async move { post(&srv.url, &t, &initialize()).await.status })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    for (what, url, auth, status) in [
        (
            "static token, oauth first",
            &first.url,
            Some(STATIC_TOKEN),
            200,
        ),
        ("no header, oauth first", &first.url, None, 401),
        ("garbage, oauth first", &first.url, Some("not-a-jwt"), 401),
        ("no header, oauth alone", &alone.url, None, 401),
        ("garbage, oauth alone", &alone.url, Some("x.y.z"), 401),
    ] {
        let t0 = Instant::now();
        let header = auth.map(|a| format!("Bearer {a}"));
        let seen = post_auth(url, header.as_deref(), &initialize()).await;
        assert_eq!(seen.status, status, "{what}: {seen:?}");
        assert!(
            t0.elapsed() < Duration::from_millis(3000),
            "{what} waited {:?}",
            t0.elapsed()
        );
    }
    assert_eq!(waiting.await.unwrap(), 401);
}

/// A known key past its TTL stays usable while the issuer hangs: one
/// request waits for the refresh (and then uses the stale key), the others
/// do not wait at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_key_is_used_without_waiting() {
    let m = mock_as().await;
    let mut cfg = oauth_config(&m);
    cfg.jwks_timeout_ms = 6000;
    cfg.jwks_ttl_secs = 0;
    cfg.jwks_refetch_secs = 0;
    let srv = Arc::new(serve(service(), authn("oauth", cfg)).await);
    let t = sign(Signer::Es256, "ec-1", &claims(&m.base, ALICE));
    assert_eq!(post(&srv.url, &t, &initialize()).await.status, 200);
    m.hang.store(true, Ordering::SeqCst);
    let refresher = {
        let srv = srv.clone();
        let t = t.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            (post(&srv.url, &t, &initialize()).await.status, t0.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = Instant::now();
    assert_eq!(post(&srv.url, &t, &initialize()).await.status, 200);
    assert!(
        t0.elapsed() < Duration::from_millis(3000),
        "waited {:?}",
        t0.elapsed()
    );
    let (status, took) = refresher.await.unwrap();
    assert_eq!(status, 200, "the refresher falls back to the stale key");
    assert!(
        took < Duration::from_millis(6000) + Duration::from_secs(3),
        "{took:?}"
    );
}

/// Unknown `kid`s cannot make the server hammer the issuer; a rotated-in key
/// is picked up by the next allowed refetch.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_kids_are_rate_limited_and_rotation_works() {
    let m = mock_as().await;
    let srv = serve(service(), authn("oauth", oauth_config(&m))).await;
    let good = claims(&m.base, ALICE);
    for i in 0..20 {
        let t = sign(Signer::Es256, &format!("random-{i}"), &good);
        assert_eq!(post(&srv.url, &t, &initialize()).await.status, 401);
    }
    assert_eq!(m.jwks_fetches.load(Ordering::SeqCst), 1);
    // A known key is served from the cache.
    let t = sign(Signer::Es256, "ec-1", &good);
    assert_eq!(post(&srv.url, &t, &initialize()).await.status, 200);
    assert_eq!(m.jwks_fetches.load(Ordering::SeqCst), 1);

    // Rotation, with no refetch interval: the new key is fetched once.
    let mut cfg = oauth_config(&m);
    cfg.jwks_refetch_secs = 0;
    let srv = serve(service(), authn("oauth", cfg)).await;
    let t = sign(Signer::Es256, "ec-1", &good);
    assert_eq!(post(&srv.url, &t, &initialize()).await.status, 200);
    let before = m.jwks_fetches.load(Ordering::SeqCst);
    let rotated = sign(Signer::Ec2, "ec-2", &good);
    assert_eq!(post(&srv.url, &rotated, &initialize()).await.status, 401);
    m.jwks
        .lock()
        .unwrap()
        .get_mut("keys")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(ec_jwk(&keys().ec2, "ec-2"));
    assert_eq!(post(&srv.url, &rotated, &initialize()).await.status, 200);
    assert_eq!(m.jwks_fetches.load(Ordering::SeqCst), before + 2);
}

/// Discovery: OpenID Connect when RFC 8414 is missing, an explicit JWKS URL
/// skips it, and metadata naming another issuer is refused.
#[tokio::test(flavor = "multi_thread")]
async fn discovery() {
    let good = |m: &Mock| sign(Signer::Es256, "ec-1", &claims(&m.base, ALICE));

    let m = mock_as().await;
    m.no_rfc8414.store(true, Ordering::SeqCst);
    let srv = serve(service(), authn("oauth", oauth_config(&m))).await;
    assert_eq!(post(&srv.url, &good(&m), &initialize()).await.status, 200);
    assert_eq!(m.hits.load(Ordering::SeqCst), 3, "8414, OIDC, then JWKS");

    let m = mock_as().await;
    let mut cfg = oauth_config(&m);
    cfg.jwks_url = Some(format!("{}/jwks", m.base));
    let srv = serve(service(), authn("oauth", cfg)).await;
    assert_eq!(post(&srv.url, &good(&m), &initialize()).await.status, 200);
    assert_eq!(m.hits.load(Ordering::SeqCst), 1, "JWKS only");

    let m = mock_as().await;
    *m.metadata_issuer.lock().unwrap() = Some("https://evil.example.test".into());
    let srv = serve(service(), authn("oauth", oauth_config(&m))).await;
    let seen = post(&srv.url, &good(&m), &initialize()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(m.jwks_fetches.load(Ordering::SeqCst), 0);
}

// --------------------------------------------------- the server binary

fn server_command() -> std::process::Command {
    let mut cmd = std::process::Command::new(SERVER);
    for (k, _) in std::env::vars() {
        if k.starts_with("PSCAT_MCP_") || k == "PORT" || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    cmd.args(["serve-http", "--bind", "127.0.0.1", "--port", "0"]);
    cmd
}

/// The binary refuses an incomplete or unsafe `oauth` configuration before
/// binding a port, without echoing allowlist entries.
#[test]
fn startup_refuses_a_bad_oauth_config() {
    let dir = std::env::temp_dir().join(format!("pscat-mcp-oauth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join("allowed");
    std::fs::write(&list, "# comment\nsecret.person@example.com\n").unwrap();
    let list = list.to_str().unwrap().to_string();
    let full: Vec<(&str, &str)> = vec![
        ("PSCAT_MCP_OAUTH_ISSUER", "https://issuer.example.test/"),
        ("PSCAT_MCP_OAUTH_RESOURCE", RESOURCE),
        ("PSCAT_MCP_OAUTH_ALLOWED_USERS", "sub:abc"),
    ];
    let without =
        |k: &str| -> Vec<(&str, &str)> { full.iter().filter(|(n, _)| *n != k).cloned().collect() };
    let replace = |k: &'static str, v: &'static str| -> Vec<(&str, &str)> {
        let mut out = without(k);
        out.push((k, v));
        out
    };
    for (env, expect) in [
        (
            without("PSCAT_MCP_OAUTH_ISSUER"),
            "needs PSCAT_MCP_OAUTH_ISSUER",
        ),
        (
            without("PSCAT_MCP_OAUTH_RESOURCE"),
            "needs PSCAT_MCP_OAUTH_RESOURCE",
        ),
        (
            without("PSCAT_MCP_OAUTH_ALLOWED_USERS"),
            "needs an allowlist",
        ),
        (
            replace("PSCAT_MCP_OAUTH_ISSUER", "http://issuer.example.test/"),
            "PSCAT_MCP_OAUTH_ISSUER must be an https URL",
        ),
        (
            replace("PSCAT_MCP_OAUTH_RESOURCE", "https://mcp.example.test/mcp#x"),
            "must not contain a fragment",
        ),
        (
            replace("PSCAT_MCP_OAUTH_ALLOWED_USERS", "alice@example.com"),
            "neither `sub:<subject>` nor `email:<address>`",
        ),
    ] {
        let mut cmd = server_command();
        cmd.env("PSCAT_MCP_AUTH", "oauth");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{env:?} must not start: {stderr}");
        assert!(stderr.contains(expect), "{env:?}: {stderr}");
        assert!(!stderr.contains("listening"), "{stderr}");
        assert!(!stderr.contains("alice@"), "{stderr}");
    }
    // The file is read, and its bad entry refused without being echoed.
    let mut cmd = server_command();
    cmd.env("PSCAT_MCP_AUTH", "oauth");
    for (k, v) in without("PSCAT_MCP_OAUTH_ALLOWED_USERS") {
        cmd.env(k, v);
    }
    cmd.env("PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE", &list);
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("neither"), "{stderr}");
    assert!(!stderr.contains("secret.person"), "{stderr}");
    // `verified:` takes only `*`; anything else is refused, not echoed.
    for entry in ["verified:secret-person", "verified:", "sub:abc,verified:no"] {
        let mut cmd = server_command();
        cmd.env("PSCAT_MCP_AUTH", "oauth");
        for (k, v) in replace("PSCAT_MCP_OAUTH_ALLOWED_USERS", "x") {
            if k != "PSCAT_MCP_OAUTH_ALLOWED_USERS" {
                cmd.env(k, v);
            }
        }
        cmd.env("PSCAT_MCP_OAUTH_ALLOWED_USERS", entry);
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{entry} must not start: {stderr}");
        assert!(stderr.contains("exactly `verified:*`"), "{entry}: {stderr}");
        assert!(!stderr.contains("secret-person"), "{stderr}");
        assert!(!stderr.contains("listening"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The binary with `PSCAT_MCP_AUTH=bearer,oauth` against the mock issuer
/// and an allowlist with `verified:*`: startup warns about `verified:*`; an
/// accepted OAuth request logs `auth_method=oauth` and the token's subject as
/// `sub` (issue #160) on its own lines, its render's and its async job's; a
/// bearer request carries no `sub`; a refused one logs its reason and no
/// subject; and no email, other claim or token appears in the logs at INFO.
#[tokio::test(flavor = "multi_thread")]
async fn logs_carry_the_subject_but_never_emails_or_tokens() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let m = mock_as().await;
    let mut child = tokio::process::Command::from(server_command())
        .env("PSCAT_MCP_AUTH", "bearer,oauth")
        .env("PSCAT_MCP_AUTH_TOKENS", STATIC_TOKEN)
        .env("PSCAT_MCP_ASYNC_JOBS", "true")
        .env("PSCAT_MCP_OAUTH_ISSUER", &m.base)
        .env("PSCAT_MCP_OAUTH_RESOURCE", RESOURCE)
        .env(
            "PSCAT_MCP_OAUTH_ALLOWED_USERS",
            format!("sub:{ALICE},email:{CAROL_EMAIL},verified:*"),
        )
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        // The first OAuth request makes the server fetch the issuer's
        // metadata and keys from the mock above, and the renders each start a
        // worker. All answer in milliseconds, so the server's defaults (5 s
        // per fetch, 20 s per render) are far above what the test needs; but
        // a machine starved for seconds (parallel builds) can miss them, and
        // the test then fails on a 401 or a killed render that say nothing
        // about the logs. These bound a hang, not the work.
        .env("PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS", "60000")
        .env("PSCAT_MCP_RENDER_TIMEOUT_MS", "100000")
        .env("PSCAT_MCP_QUEUE_TIMEOUT_MS", "100000")
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut next_line = async || -> Value {
        // Above the render timeout set above, so a render the server gives
        // up on is seen as its log line rather than as a silent test.
        let line = tokio::time::timeout(Duration::from_secs(150), rx.recv())
            .await
            .expect("a log line in time")
            .expect("the server is still running");
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line}"))
    };
    let mut log: Vec<Value> = Vec::new();
    let mut addr = String::new();
    while addr.is_empty() {
        let v = next_line().await;
        if let Some(a) = v["addr"].as_str() {
            addr = a.to_string();
        }
        log.push(v);
    }
    let url = format!("http://{addr}/mcp");
    let alice = sign(
        Signer::Es256,
        "ec-1",
        &with(claims(&m.base, ALICE), |c| {
            c.insert("email".into(), json!("alice.private@example.com"));
            c.insert("email_verified".into(), json!(true));
        }),
    );
    let carol = sign(
        Signer::Rs256,
        "rsa-1",
        &with(claims(&m.base, "carol-subject-0a1"), |c| {
            c.insert("email".into(), json!(CAROL_EMAIL));
            c.insert("email_verified".into(), json!(true));
        }),
    );
    let bob = sign(Signer::Es256, "ec-1", &claims(&m.base, BOB));
    let expired = sign(
        Signer::Es256,
        "ec-1",
        &with(claims(&m.base, ALICE), |c| {
            c.insert("exp".into(), json!(now() - 3600));
        }),
    );
    assert_eq!(post(&url, &alice, &initialize()).await.status, 200);
    assert_eq!(post(&url, &carol, &initialize()).await.status, 200);
    assert_eq!(post(&url, &bob, &initialize()).await.status, 403);
    assert_eq!(post(&url, &expired, &initialize()).await.status, 401);
    // A render and an async job as ALICE, and a render with the bearer
    // token; the three programs differ in length, so `source_bytes` tells
    // their lines apart.
    let tool = |name: &str, source: &str| {
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": name,
            "arguments": {"source": source, "width": 20, "height": 20},
        }})
    };
    const SYNC: &str = "0 0 moveto 10 10 lineto stroke showpage";
    const JOB: &str = "showpage";
    const BEARER: &str = "1 pop showpage";
    let seen = post(&url, &alice, &tool("render_postscript", SYNC)).await;
    assert_eq!(seen.status, 200, "{seen:?}");
    let seen = post(&url, &alice, &tool("start_render", JOB)).await;
    assert_eq!(seen.status, 200, "{seen:?}");
    assert!(seen.body.contains("job_id"), "{seen:?}");
    let seen = post(&url, STATIC_TOKEN, &tool("render_postscript", BEARER)).await;
    assert_eq!(seen.status, 200, "{seen:?}");
    // Seven responses, and three renders ended (the job's may come after
    // its response; on a loaded machine one may be killed at its deadline).
    let (mut responses, mut ended) = (0, 0);
    while responses < 7 || ended < 3 {
        let v = next_line().await;
        if v["message"] == "finished processing request" {
            responses += 1;
        }
        if matches!(
            v["message"].as_str(),
            Some("render finished" | "render killed at the deadline" | "render worker failed")
        ) {
            ended += 1;
        }
        log.push(v);
    }
    child.kill().await.ok();

    let text = log
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    // ALICE is logged, on purpose, only as the span's `sub`; BOB and the
    // expired token never got past the auth layer.
    assert!(
        log.iter()
            .filter(|v| v.to_string().contains(ALICE))
            .all(|v| v["span"]["sub"] == ALICE),
        "{text}"
    );
    for secret in [
        BOB,
        "alice.private",
        CAROL_EMAIL,
        &alice[alice.len() - 20..],
        &bob[bob.len() - 20..],
        &carol[carol.len() - 20..],
    ] {
        assert!(!text.contains(secret), "{secret} logged:\n{text}");
    }
    let find = |msg: &str| -> Vec<&Value> {
        log.iter()
            .filter(|v| v["message"].as_str().is_some_and(|m| m.starts_with(msg)))
            .collect()
    };
    assert_eq!(
        find("auth methods enabled")[0]["auth_methods"],
        "bearer,oauth"
    );
    assert_eq!(
        find("the OAuth allowlist has `verified:*`").len(),
        1,
        "{text}"
    );
    assert_eq!(
        find("the OAuth allowlist has `verified:*`")[0]["level"],
        "WARN"
    );
    let listening = find("listening");
    assert!(
        listening[0]["auth"]
            .as_str()
            .is_some_and(|a| a.contains("any verified email (verified:*)")),
        "{text}"
    );
    let done = find("finished processing request");
    let statuses: Vec<i64> = done.iter().map(|v| v["status"].as_i64().unwrap()).collect();
    assert_eq!(statuses, [200, 200, 403, 401, 200, 200, 200], "{text}");
    assert_eq!(done[0]["span"]["auth_method"], "oauth", "{text}");
    assert_eq!(done[0]["span"]["sub"], ALICE, "{text}");
    // Carol's subject too: the field is the token's `sub`, never the email.
    assert_eq!(done[1]["span"]["sub"], "carol-subject-0a1", "{text}");
    for refused in &done[2..4] {
        assert!(refused["span"].get("sub").is_none(), "{text}");
    }
    assert_eq!(done[4]["span"]["sub"], ALICE, "{text}");
    assert_eq!(done[5]["span"]["sub"], ALICE, "{text}");
    assert_eq!(done[6]["span"]["auth_method"], "bearer", "{text}");
    assert!(done[6]["span"].get("sub").is_none(), "{text}");
    // The render lines carry the span: ALICE's sync render and her job's
    // render (in the span of the request that started it), not the
    // bearer's.
    let render_lines = |bytes: usize| -> Vec<&Value> {
        log.iter()
            .filter(|v| {
                v["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("render "))
                    && v["source_bytes"] == bytes
            })
            .collect()
    };
    for (what, bytes, sub) in [
        ("sync", SYNC.len(), Some(ALICE)),
        ("job", JOB.len(), Some(ALICE)),
        ("bearer", BEARER.len(), None),
    ] {
        let lines = render_lines(bytes);
        assert_eq!(lines.len(), 2, "{what}: {text}");
        for l in lines {
            assert_eq!(l["span"]["sub"].as_str(), sub, "{what}: {l}");
            assert!(l["span"]["request_id"].is_string(), "{what}: {l}");
        }
    }
    let rejected = find("rejected unauthenticated request");
    let reasons: Vec<&str> = rejected
        .iter()
        .map(|v| v["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["not_allowed", "invalid_token"], "{text}");
    let detail: Vec<&str> = find("oauth: token rejected")
        .iter()
        .map(|v| v["reason"].as_str().unwrap())
        .collect();
    assert_eq!(detail, ["not_allowed", "expired"], "{text}");
}
