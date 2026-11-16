//! Several auth methods side by side (issue #118): `PSCAT_MCP_AUTH`, the
//! `AuthChain` over HTTP with a test-only method beside `bearer`, job
//! ownership by principals of different methods, the startup refusals, and
//! the `auth_method` log field, end to end through the server binary.
//!
//! The test-only method reads an `X-Fake-User` header: `alice` is accepted
//! as `fake:alice`, `mallory` is rejected, and no header is not applicable.

use std::net::{IpAddr, Ipv4Addr};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pscat_mcp::auth::{
    AuthChain, AuthError, AuthFuture, AuthMethod, Authenticator, BearerTokenAuth, Challenge,
    Outcome, Principal,
};
use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::{AuthConfig, HttpConfig, JobsConfig, LimitsConfig};
use pscat_mcp::http::{authenticator, authenticator_with, mcp_server_config, router};
use pscat_mcp::render::RenderService;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "auth-test-token-0123456789";
const OTHER_TOKEN: &str = "auth-test-token-other-9876";
const ACCEPT: &str = "application/json, text/event-stream";
const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
/// Never ends: only a kill stops it.
const FOREVER: &str = "{ } loop";

struct Fake;

impl AuthMethod for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn check<'a>(&'a self, parts: &'a http::request::Parts) -> AuthFuture<'a, Outcome> {
        let outcome = match parts.headers.get("x-fake-user").map(|v| v.to_str()) {
            None => Outcome::NotApplicable,
            Some(Ok("mallory")) => Outcome::Rejected(AuthError::InvalidToken),
            Some(Ok(name)) => Outcome::Accepted(Principal::new("fake", format!("fake:{name}"))),
            Some(Err(_)) => Outcome::Rejected(AuthError::Malformed),
        };
        Box::pin(std::future::ready(outcome))
    }

    fn challenges(&self, rejection: Option<&AuthError>) -> Vec<Challenge> {
        let c = Challenge::new("Fake").param("realm", "test");
        vec![match rejection {
            Some(e) => c.param("error", e.code()),
            None => c,
        }]
    }

    fn summary(&self) -> String {
        "fake".into()
    }
}

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
        async_jobs: true,
    }
}

fn auth_config(list: &str) -> AuthConfig {
    AuthConfig {
        auth: Some(list.into()),
    }
}

fn service() -> Arc<RenderService> {
    let limits = LimitsConfig {
        interp_time_limit_ms: Some(0),
        max_steps: 0,
        max_concurrent_renders: Some(2),
        ..LimitsConfig::default()
    };
    let jobs = JobsConfig {
        job_timeout_ms: 15_000,
        ..JobsConfig::default()
    }
    .enabled();
    jobs.check(&limits).expect("job settings");
    let backend = SubprocessBackend::new(SERVER.into(), limits.clone());
    Arc::new(RenderService::new(Arc::new(backend), limits).with_jobs(jobs))
}

struct Srv {
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
            "clientInfo": { "name": "auth-test", "version": "1.0" }
        }
    })
}

fn tool_call(tool: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    })
}

/// What a client sees: status, `WWW-Authenticate` and the body.
#[derive(Debug, PartialEq)]
struct Seen {
    status: u16,
    challenge: Option<String>,
    body: String,
}

async fn post(url: &str, headers: &[(&str, &str)], body: &Value) -> Seen {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
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

fn structured(seen: &Seen) -> Value {
    let v: Value = serde_json::from_str(&seen.body).unwrap_or_else(|_| panic!("{seen:?}"));
    v["result"]["structuredContent"].clone()
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// `PSCAT_MCP_AUTH=bearer` answers every request exactly as the default
/// (unset) configuration: status, challenge and body.
#[tokio::test(flavor = "multi_thread")]
async fn bearer_by_name_answers_like_the_default() {
    let mut http = http_config();
    http.auth_tokens = vec![TOKEN.into()];
    let legacy = authenticator(&http).unwrap();
    let named = authenticator_with(&http, &auth_config("bearer")).unwrap();
    assert_eq!(legacy.describe(), named.describe());
    assert_eq!(named.methods(), ["bearer"]);
    let unset = authenticator_with(&http, &AuthConfig::default()).unwrap();
    assert_eq!(unset.describe(), legacy.describe());

    let svc = service();
    let a = serve(svc.clone(), legacy).await;
    let b = serve(svc.clone(), named).await;
    let ok = bearer(TOKEN);
    let wrong = bearer("wrong");
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        vec![("Authorization", ok.as_str())],
        vec![("Authorization", wrong.as_str())],
        vec![("Authorization", "Basic dGVzdA==")],
        vec![("Authorization", "Bearer ")],
        vec![("X-Fake-User", "alice")],
    ];
    for headers in &cases {
        let left = post(&a.url, headers, &initialize()).await;
        let right = post(&b.url, headers, &initialize()).await;
        assert_eq!(left, right, "{headers:?}");
        if left.status == 401 {
            assert_eq!(
                left.challenge.as_deref(),
                Some(r#"Bearer realm="pscat-mcp""#)
            );
        }
    }
    assert_eq!(
        svc.counters
            .executed
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fake_method_alone() {
    let srv = serve(service(), Arc::new(Fake)).await;
    assert_eq!(
        post(&srv.url, &[("X-Fake-User", "alice")], &initialize())
            .await
            .status,
        200
    );
    let seen = post(&srv.url, &[], &initialize()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(seen.challenge.as_deref(), Some(r#"Fake realm="test""#));
    let body: Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["error"], "missing_credentials");
    // A valid bearer token means nothing without the bearer method.
    let ok = bearer(TOKEN);
    let seen = post(&srv.url, &[("Authorization", ok.as_str())], &initialize()).await;
    assert_eq!(seen.status, 401);
    let seen = post(&srv.url, &[("X-Fake-User", "mallory")], &initialize()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Fake realm="test", error="invalid_token""#)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_and_another_method_side_by_side() {
    let chain = AuthChain::new(vec![
        Box::new(BearerTokenAuth::new([TOKEN])),
        Box::new(Fake),
    ]);
    let srv = serve(service(), Arc::new(chain)).await;
    let ok = bearer(TOKEN);
    let wrong = bearer("wrong");
    // Each method on its own.
    for headers in [
        vec![("Authorization", ok.as_str())],
        vec![("X-Fake-User", "alice")],
    ] {
        assert_eq!(
            post(&srv.url, &headers, &initialize()).await.status,
            200,
            "{headers:?}"
        );
    }
    // One method rejects, the other accepts: admitted.
    for headers in [
        vec![("Authorization", wrong.as_str()), ("X-Fake-User", "alice")],
        vec![("Authorization", ok.as_str()), ("X-Fake-User", "mallory")],
    ] {
        assert_eq!(
            post(&srv.url, &headers, &initialize()).await.status,
            200,
            "{headers:?}"
        );
    }
    // Nothing presented: both challenges.
    let seen = post(&srv.url, &[], &initialize()).await;
    assert_eq!(seen.status, 401);
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Bearer realm="pscat-mcp", Fake realm="test""#)
    );
    let body: Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["error"], "missing_credentials");
    // Both reject: the first rejection in order is reported, and the body
    // never echoes the credential.
    let seen = post(
        &srv.url,
        &[
            ("Authorization", wrong.as_str()),
            ("X-Fake-User", "mallory"),
        ],
        &initialize(),
    )
    .await;
    assert_eq!(seen.status, 401);
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Bearer realm="pscat-mcp", Fake realm="test", error="invalid_token""#)
    );
    let body: Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["error"], "invalid_token");
    assert!(!seen.body.contains("wrong") && !seen.body.contains("mallory"));
}

/// Job ownership follows the principal whatever method admitted it: a job
/// started by `fake:alice` is unknown to a bearer token and to `fake:bob`.
#[tokio::test(flavor = "multi_thread")]
async fn jobs_belong_to_the_principal_of_any_method() {
    let chain = AuthChain::new(vec![
        Box::new(BearerTokenAuth::new([TOKEN, OTHER_TOKEN])),
        Box::new(Fake),
    ]);
    let srv = serve(service(), Arc::new(chain)).await;
    let tok = bearer(TOKEN);
    let alice = [("X-Fake-User", "alice")];

    let started = post(
        &srv.url,
        &alice,
        &tool_call(
            "start_render",
            json!({ "source": FOREVER, "width": 20, "height": 10 }),
        ),
    )
    .await;
    let s = structured(&started);
    assert_eq!(s["ok"], true, "{started:?}");
    let id = s["job_id"].as_str().unwrap().to_string();

    let status = |headers: Vec<(&'static str, String)>| {
        let url = srv.url.clone();
        let id = id.clone();
        async move {
            let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            structured(
                &post(
                    &url,
                    &h,
                    &tool_call("render_status", json!({ "job_id": id })),
                )
                .await,
            )
        }
    };
    assert_eq!(
        status(vec![("X-Fake-User", "alice".into())]).await["ok"],
        true
    );
    for other in [
        vec![("Authorization", tok.clone())],
        vec![("X-Fake-User", "bob".into())],
    ] {
        let s = status(other.clone()).await;
        assert_eq!(s["error"]["code"], "unknown_job", "{other:?}: {s}");
    }
    // A bearer token that also sends alice's header is the bearer
    // principal (first in order), not alice.
    let s = status(vec![
        ("Authorization", tok.clone()),
        ("X-Fake-User", "alice".into()),
    ])
    .await;
    assert_eq!(s["error"]["code"], "unknown_job", "{s}");

    let cancelled = post(
        &srv.url,
        &alice,
        &tool_call("cancel_render", json!({ "job_id": id })),
    )
    .await;
    assert_eq!(
        structured(&cancelled)["state"],
        "cancelled",
        "{cancelled:?}"
    );
}

/// A `serve-http` command with no `PSCAT_MCP_*` variable from the parent.
fn server_command(bind: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(SERVER);
    for (k, _) in std::env::vars() {
        if k.starts_with("PSCAT_MCP_") || k == "PORT" || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    cmd.args(["serve-http", "--bind", bind, "--port", "0"]);
    cmd
}

/// Refused at startup, through the real binary: an empty, unknown or
/// repeated list, the methods not implemented yet, and `bearer` without a
/// token. None of them binds a port.
#[test]
fn startup_refuses_a_bad_auth_list() {
    for (list, tokens, expect) in [
        (
            "oauth",
            true,
            "`oauth` (PSCAT_MCP_AUTH / --auth) is not implemented yet",
        ),
        (
            "bearer,oauth",
            true,
            "`oauth` (PSCAT_MCP_AUTH / --auth) is not implemented yet",
        ),
        (
            "none",
            true,
            "`none` (PSCAT_MCP_AUTH / --auth) is not implemented yet",
        ),
        (
            "bearer,none",
            true,
            "`none` (PSCAT_MCP_AUTH / --auth) is not implemented yet",
        ),
        ("bogus", true, "unknown auth method \"bogus\""),
        (",", true, "lists no auth method"),
        (" ", true, "lists no auth method"),
        ("bearer,bearer", true, "listed twice"),
        ("bearer", false, "no bearer tokens are configured"),
        // A literal empty value is refused, not taken as unset (no token:
        // a fallback to the default would fail on another message).
        ("", false, "lists no auth method"),
    ] {
        let mut cmd = server_command("127.0.0.1");
        cmd.env("PSCAT_MCP_AUTH", list);
        if tokens {
            cmd.env("PSCAT_MCP_AUTH_TOKENS", TOKEN);
        }
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{list:?} must not start: {stderr}");
        assert!(stderr.contains(expect), "{list:?}: {stderr}");
        assert!(!stderr.contains("listening"), "{list:?}: {stderr}");
        assert!(!stderr.contains(TOKEN), "{list:?}: {stderr}");
    }
    // The flag works like the variable.
    let out = server_command("127.0.0.1")
        .args(["--auth", "oauth", "--auth-tokens", TOKEN])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented yet"));
    let out = server_command("127.0.0.1")
        .args(["--auth", ""])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("lists no auth method"));
    // The loopback rule for --allow-unauthenticated still holds.
    let out = server_command("0.0.0.0")
        .arg("--allow-unauthenticated")
        .env("PSCAT_MCP_AUTH", "bearer")
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("--allow-unauthenticated is only permitted on a loopback"),
        "{stderr}"
    );
}

/// The server binary with `PSCAT_MCP_AUTH=bearer`: startup logs the
/// methods, the response line of an accepted request carries
/// `auth_method`, a rejected one logs `reason` and `rejected_by`, and no
/// token appears anywhere in the logs.
#[tokio::test(flavor = "multi_thread")]
async fn logs_name_the_method_and_never_the_token() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let mut child = tokio::process::Command::from(server_command("127.0.0.1"))
        .env("PSCAT_MCP_AUTH", "bearer")
        .env("PSCAT_MCP_AUTH_TOKENS", TOKEN)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Drain stderr all the time: a server whose stderr pipe is not read
    // blocks on its next log line.
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
        let line = tokio::time::timeout(Duration::from_secs(60), rx.recv())
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
    assert!(!addr.is_empty(), "no address: {log:?}");
    let url = format!("http://{addr}/mcp");
    let ok = bearer(TOKEN);
    let wrong = bearer(OTHER_TOKEN);
    assert_eq!(
        post(&url, &[("Authorization", ok.as_str())], &initialize())
            .await
            .status,
        200
    );
    assert_eq!(
        post(&url, &[("Authorization", wrong.as_str())], &initialize())
            .await
            .status,
        401
    );
    // Read until both response lines are in.
    let mut responses = 0;
    while responses < 2 {
        let v = next_line().await;
        if v["message"] == "finished processing request" {
            responses += 1;
        }
        log.push(v);
    }
    child.kill().await.ok();

    let text = log
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains(TOKEN) && !text.contains(OTHER_TOKEN),
        "{text}"
    );
    let find = |msg: &str| -> Vec<&Value> {
        log.iter()
            .filter(|v| v["message"].as_str().is_some_and(|m| m.starts_with(msg)))
            .collect()
    };
    let enabled = find("auth methods enabled");
    assert_eq!(enabled.len(), 1, "{text}");
    assert_eq!(enabled[0]["auth_methods"], "bearer");
    let listening = find("listening");
    assert_eq!(listening[0]["auth_methods"], "bearer", "{text}");
    let done = find("finished processing request");
    assert_eq!(done[0]["status"], 200, "{text}");
    assert_eq!(done[0]["span"]["auth_method"], "bearer", "{text}");
    assert_eq!(done[1]["status"], 401, "{text}");
    assert!(done[1]["span"].get("auth_method").is_none(), "{text}");
    let rejected = find("rejected unauthenticated request");
    assert_eq!(rejected.len(), 1, "{text}");
    assert_eq!(rejected[0]["reason"], "invalid_token");
    assert_eq!(rejected[0]["rejected_by"], "bearer");
}
