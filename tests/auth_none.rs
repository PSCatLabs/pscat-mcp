//! The explicit no-auth mode and its guard rails (issue #120):
//! `PSCAT_MCP_AUTH=none` and `bearer,none`, the source allowlist, the
//! per-client rate limit, the trusted-hop client address (a forged
//! `X-Forwarded-For` can neither escape the allowlist nor reset or spend
//! another client's limit), the expiry at startup and at request time, and
//! the refusal with async jobs. Most tests run the real server binary over
//! HTTP; the request-time expiry runs the router in-process with an
//! injected clock, so none of them waits for a timer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pscat_mcp::auth::{AuthChain, BearerTokenAuth, NoAuth};
use pscat_mcp::auth_none::{NoneAuthConfig, NoneGuard};
use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::{AuthConfig, HttpConfig, LimitsConfig};
use pscat_mcp::http::{authenticator_with, mcp_server_config, router};
use pscat_mcp::render::RenderService;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "none-test-token-0123456789";
const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const ACCEPT: &str = "application/json, text/event-stream";
/// A documentation address (RFC 5737) that must never reach the logs.
const SECRET_IP: &str = "203.0.113.7";

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "auth-none-test", "version": "1.0" }
        }
    })
}

/// What a client sees.
#[derive(Debug)]
struct Seen {
    status: u16,
    challenge: Option<String>,
    retry_after: Option<String>,
    body: String,
}

impl Seen {
    fn error_code(&self) -> String {
        let v: Value = serde_json::from_str(&self.body).unwrap_or_default();
        v["error"].as_str().unwrap_or_default().to_string()
    }
}

async fn post(url: &str, headers: &[(&str, &str)]) -> Seen {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .body(initialize().to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    Seen {
        status: resp.status().as_u16(),
        challenge: header("www-authenticate"),
        retry_after: header("retry-after"),
        body: resp.text().await.unwrap(),
    }
}

/// A `serve-http` command on loopback, with no `PSCAT_MCP_*` variable from
/// the parent.
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

/// A running server binary and its log lines.
struct Server {
    url: String,
    log: Arc<Mutex<Vec<String>>>,
    _child: tokio::process::Child,
}

impl Server {
    /// Start with `env` and wait for the listening line.
    async fn start(env: &[(&str, &str)]) -> Server {
        let mut cmd = server_command();
        cmd.env("PSCAT_MCP_LOG_FORMAT", "json")
            .env("RUST_LOG", "info");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = tokio::process::Command::from(cmd)
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sink = log.clone();
        // Drain stderr all the time: an unread pipe blocks the server.
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                sink.lock().unwrap().push(line.clone());
                let _ = tx.send(line);
            }
        });
        let url = loop {
            let line = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                .await
                .expect("the server logs in time")
                .expect("the server keeps running");
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if let Some(addr) = v["addr"].as_str() {
                break format!("http://{addr}/mcp");
            }
        };
        Server {
            url,
            log,
            _child: child,
        }
    }

    /// The log lines so far, parsed.
    fn lines(&self) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    async fn post(&self, headers: &[(&str, &str)]) -> Seen {
        post(&self.url, headers).await
    }
}

/// Run the binary expecting a refusal; returns its stderr.
fn refused(env: &[(&str, &str)]) -> String {
    let mut cmd = server_command();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "{env:?} must not start: {stderr}");
    assert!(!stderr.contains("listening"), "{env:?}: {stderr}");
    stderr
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// `none` alone: a request with no credential is admitted as anonymous and
/// counted as `auth_method=none`; the startup lines warn loudly; no client
/// address reaches the INFO logs, whatever header carried it.
#[tokio::test(flavor = "multi_thread")]
async fn none_alone_admits_an_anonymous_request() {
    let srv = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        // One trusted hop, so the address in the header is the client's.
        ("PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS", "1"),
    ])
    .await;
    let xff = [("X-Forwarded-For", SECRET_IP)];
    assert_eq!(srv.post(&xff).await.status, 200);
    // With no other method, a header `none` has no use for does not matter.
    let seen = srv
        .post(&[
            ("X-Forwarded-For", SECRET_IP),
            ("Authorization", "Bearer anything"),
        ])
        .await;
    assert_eq!(seen.status, 200, "{seen:?}");
    // A call reaches a tool.
    let resp = reqwest::Client::new()
        .post(&srv.url)
        .header("Content-Type", "application/json")
        .header("Accept", ACCEPT)
        .header("X-Forwarded-For", SECRET_IP)
        .body(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "render_postscript", "arguments": { "source": "0 0 moveto 10 10 lineto stroke", "width": 20, "height": 10 } }
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["structuredContent"]["ok"], true, "{body}");

    // Wait until the response lines of all three requests are logged.
    let mut lines = Vec::new();
    for _ in 0..100 {
        lines = srv.lines();
        let done = lines
            .iter()
            .filter(|v| v["message"] == "finished processing request")
            .count();
        if done >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = srv.log.lock().unwrap().join("\n");
    assert!(
        !text.contains(SECRET_IP),
        "a client address was logged: {text}"
    );
    let done: Vec<&Value> = lines
        .iter()
        .filter(|v| v["message"] == "finished processing request")
        .collect();
    assert!(done.len() >= 3, "{text}");
    for d in done {
        assert_eq!(d["span"]["auth_method"], "none", "{text}");
        assert_eq!(d["status"], 200);
    }
    let warned = lines
        .iter()
        .find(|v| {
            v["message"]
                .as_str()
                .is_some_and(|m| m.contains("is ENABLED"))
        })
        .unwrap_or_else(|| panic!("no startup warning: {text}"));
    assert_eq!(warned["level"], "WARN");
    let enabled = lines
        .iter()
        .find(|v| v["message"] == "auth methods enabled (PSCAT_MCP_AUTH)")
        .unwrap();
    assert_eq!(enabled["auth_methods"], "none");
}

/// `bearer,none`: no header is anonymous, a right token is the bearer
/// principal, and a wrong or malformed credential is a 401 that never falls
/// through to anonymous.
#[tokio::test(flavor = "multi_thread")]
async fn bearer_and_none_never_downgrade_a_bad_credential() {
    let srv = Server::start(&[
        ("PSCAT_MCP_AUTH", "bearer,none"),
        ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
    ])
    .await;
    let ok = bearer(TOKEN);
    assert_eq!(
        srv.post(&[("Authorization", ok.as_str())]).await.status,
        200
    );
    // No Authorization header: anonymous.
    assert_eq!(srv.post(&[]).await.status, 200);
    // A wrong token is rejected, with the bearer challenge, whatever the
    // order of the list.
    let wrong = bearer("not-the-token");
    for headers in [
        vec![("Authorization", wrong.as_str())],
        vec![("Authorization", "Basic dGVzdA==")],
        vec![("Authorization", "Bearer ")],
    ] {
        let seen = srv.post(&headers).await;
        assert_eq!(seen.status, 401, "{headers:?}: {seen:?}");
        assert_eq!(
            seen.challenge.as_deref(),
            Some(r#"Bearer realm="pscat-mcp""#),
            "{headers:?}"
        );
        assert!(!seen.body.contains("not-the-token"));
    }
    let seen = srv.post(&[("Authorization", wrong.as_str())]).await;
    assert_eq!(seen.error_code(), "invalid_token");
    let order = Server::start(&[
        ("PSCAT_MCP_AUTH", "none,bearer"),
        ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
    ])
    .await;
    assert_eq!(
        order
            .post(&[("Authorization", wrong.as_str())])
            .await
            .status,
        401
    );
    assert_eq!(order.post(&[]).await.status, 200);

    // The logs: the anonymous request is `none`, the token's is `bearer`.
    let mut methods = Vec::new();
    for _ in 0..100 {
        methods = srv
            .lines()
            .iter()
            .filter(|v| v["message"] == "finished processing request")
            .map(|v| v["span"]["auth_method"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        if methods.len() >= 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(methods[0].as_deref(), Some("bearer"), "{methods:?}");
    assert_eq!(methods[1].as_deref(), Some("none"), "{methods:?}");
    assert!(methods[2..5].iter().all(Option::is_none), "{methods:?}");
}

/// Without `none` in the list a request with no credential is still a 401:
/// the setting being unset changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn without_none_nothing_is_anonymous() {
    for env in [
        vec![("PSCAT_MCP_AUTH_TOKENS", TOKEN)],
        vec![
            ("PSCAT_MCP_AUTH", "bearer"),
            ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
        ],
        // The guard-rail settings alone do not enable it.
        vec![
            ("PSCAT_MCP_AUTH", "bearer"),
            ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
            ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "5"),
        ],
    ] {
        let srv = Server::start(&env).await;
        let seen = srv.post(&[]).await;
        assert_eq!(seen.status, 401, "{env:?}: {seen:?}");
        assert_eq!(
            seen.challenge.as_deref(),
            Some(r#"Bearer realm="pscat-mcp""#)
        );
        let ok = bearer(TOKEN);
        assert_eq!(
            srv.post(&[("Authorization", ok.as_str())]).await.status,
            200
        );
    }
    // No tokens and no method: refused, as before.
    let stderr = refused(&[]);
    assert!(stderr.contains("no auth tokens configured"), "{stderr}");
    let stderr = refused(&[("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "5")]);
    assert!(stderr.contains("no auth tokens configured"), "{stderr}");
}

/// Off loopback, only an explicit `none` runs without a token:
/// `--allow-unauthenticated` alone is refused, and so is a list without
/// `none` and without tokens. (Checked through the configuration, so that
/// no test binds a public address.)
#[tokio::test(flavor = "multi_thread")]
async fn only_an_explicit_none_may_run_off_loopback() {
    let off = |allow: bool| HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        port: 0,
        auth_tokens: vec![],
        auth_token_file: None,
        allow_unauthenticated: allow,
        allowed_hosts: vec!["localhost".into()],
        allowed_origins: vec![],
        shutdown_grace_secs: 1,
        async_jobs: false,
    };
    let list = |l: &str| AuthConfig {
        auth: Some(l.into()),
        ..AuthConfig::default()
    };
    let none = authenticator_with(&off(false), &list("none")).unwrap();
    assert_eq!(none.methods(), ["none"]);
    assert!(
        none.describe().contains("UNAUTHENTICATED"),
        "{}",
        none.describe()
    );
    // Unset, with the flag: refused off loopback, as before.
    let e = authenticator_with(&off(true), &AuthConfig::default())
        .err()
        .unwrap();
    assert!(e.to_string().contains("loopback"), "{e}");
    // Unset, without it: refused for want of tokens.
    let e = authenticator_with(&off(false), &AuthConfig::default())
        .err()
        .unwrap();
    assert!(e.to_string().contains("no auth tokens"), "{e}");
    // The flag does not make `none` more permissive.
    let e = authenticator_with(&off(true), &list("none")).err().unwrap();
    assert!(e.to_string().contains("loopback"), "{e}");
    // A list without `none` and without tokens: refused.
    let e = authenticator_with(&off(false), &list("bearer"))
        .err()
        .unwrap();
    assert!(e.to_string().contains("no bearer tokens"), "{e}");
}

/// The source allowlist: a client outside every CIDR gets 403 with no bare
/// 401, and `none` alone sends no `WWW-Authenticate` for it.
#[tokio::test(flavor = "multi_thread")]
async fn the_cidr_allowlist_answers_403() {
    let outside = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        (
            "PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS",
            "10.0.0.0/8, 2001:db8::/32",
        ),
    ])
    .await;
    let seen = outside.post(&[]).await;
    assert_eq!(seen.status, 403, "{seen:?}");
    assert_eq!(seen.error_code(), "source_not_allowed");
    assert_eq!(seen.challenge, None);
    assert_eq!(seen.retry_after, None);
    let inside = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS", "10.0.0.0/8,127.0.0.0/8"),
    ])
    .await;
    assert_eq!(inside.post(&[]).await.status, 200);
    // The rejection logs a reason and no address.
    let lines = outside.lines();
    let rejected = lines
        .iter()
        .find(|v| v["message"] == "rejected unauthenticated request")
        .unwrap();
    assert_eq!(rejected["reason"], "source_not_allowed");
    assert!(!rejected.to_string().contains("127.0.0.1"), "{rejected}");
    // With a bearer method beside it, a valid token needs no allowlisted
    // address; an anonymous request outside it is still 403.
    let both = Server::start(&[
        ("PSCAT_MCP_AUTH", "bearer,none"),
        ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
        ("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS", "10.0.0.0/8"),
    ])
    .await;
    let ok = bearer(TOKEN);
    assert_eq!(
        both.post(&[("Authorization", ok.as_str())]).await.status,
        200
    );
    let seen = both.post(&[]).await;
    assert_eq!(seen.status, 403);
    // The challenge still tells a client how to authenticate.
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Bearer realm="pscat-mcp""#)
    );
}

/// The rate limit: the request past it gets 429 with `Retry-After`, and it
/// stays 429 for the rest of the window. The window is an hour, so no
/// timer can reset it mid-test; the test makes no timing assumption.
#[tokio::test(flavor = "multi_thread")]
async fn the_rate_limit_answers_429_with_retry_after() {
    for _ in 0..3 {
        let srv = Server::start(&[
            ("PSCAT_MCP_AUTH", "none"),
            ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "3"),
            ("PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS", "3600"),
        ])
        .await;
        for i in 0..3 {
            assert_eq!(srv.post(&[]).await.status, 200, "request {i}");
        }
        for i in 0..4 {
            let seen = srv.post(&[]).await;
            assert_eq!(seen.status, 429, "refused request {i}: {seen:?}");
            assert_eq!(seen.error_code(), "rate_limited");
            assert_eq!(seen.challenge, None);
            let wait: u64 = seen.retry_after.as_deref().unwrap().parse().unwrap();
            assert!((1..=3600).contains(&wait), "{seen:?}");
        }
        let lines = srv.lines();
        let reasons: Vec<&str> = lines
            .iter()
            .filter(|v| v["message"] == "rejected unauthenticated request")
            .filter_map(|v| v["reason"].as_str())
            .collect();
        assert_eq!(reasons, ["rate_limited"; 4]);
    }
}

/// With a bearer method beside `none`, the limit applies to anonymous
/// requests only: a valid token is never counted, and a wrong token is a 401
/// that does not spend the anonymous budget.
#[tokio::test(flavor = "multi_thread")]
async fn the_rate_limit_does_not_touch_bearer_requests() {
    let srv = Server::start(&[
        ("PSCAT_MCP_AUTH", "bearer,none"),
        ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
        ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "1"),
        ("PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS", "3600"),
    ])
    .await;
    let ok = bearer(TOKEN);
    let wrong = bearer("wrong");
    for _ in 0..5 {
        assert_eq!(
            srv.post(&[("Authorization", ok.as_str())]).await.status,
            200
        );
        assert_eq!(
            srv.post(&[("Authorization", wrong.as_str())]).await.status,
            401
        );
    }
    assert_eq!(srv.post(&[]).await.status, 200);
    let seen = srv.post(&[]).await;
    assert_eq!(seen.status, 429, "{seen:?}");
    assert!(seen.retry_after.is_some());
    assert_eq!(
        srv.post(&[("Authorization", ok.as_str())]).await.status,
        200
    );
}

/// The client address: by default the TCP peer, so a forged
/// `X-Forwarded-For` changes nothing; with trusted hops, the N-th entry
/// from the right, so the entries a client forges (to the left of what the
/// proxy appended) can neither escape the allowlist nor reset another
/// client's limit nor spend it.
#[tokio::test(flavor = "multi_thread")]
async fn a_forged_x_forwarded_for_changes_nothing() {
    // No trusted proxy: the header is not read. The peer is 127.0.0.1.
    let plain = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS", "10.0.0.0/8"),
    ])
    .await;
    for forged in ["10.1.1.1", "10.1.1.1, 10.2.2.2", "127.0.0.1"] {
        let seen = plain.post(&[("X-Forwarded-For", forged)]).await;
        assert_eq!(seen.status, 403, "{forged}: {seen:?}");
    }
    let limited = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "1"),
        ("PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS", "3600"),
    ])
    .await;
    assert_eq!(
        limited.post(&[("X-Forwarded-For", "1.1.1.1")]).await.status,
        200
    );
    for forged in ["2.2.2.2", "3.3.3.3", ""] {
        let seen = limited.post(&[("X-Forwarded-For", forged)]).await;
        assert_eq!(seen.status, 429, "{forged:?}: {seen:?}");
    }

    // One trusted proxy, which appends the address it saw.
    let proxied = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS", "1"),
        ("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS", "10.0.0.0/8"),
    ])
    .await;
    // The forged entry is in the allowlist, the real one is not.
    let seen = proxied
        .post(&[("X-Forwarded-For", "10.1.1.1, 198.51.100.4")])
        .await;
    assert_eq!(seen.status, 403, "{seen:?}");
    // The real one is: the forged entry to its left is ignored.
    let seen = proxied
        .post(&[("X-Forwarded-For", "198.51.100.4, 10.1.1.1")])
        .await;
    assert_eq!(seen.status, 200, "{seen:?}");
    // The header split over lines is the same list.
    let seen = proxied
        .post(&[
            ("X-Forwarded-For", "10.9.9.9"),
            ("X-Forwarded-For", "198.51.100.4"),
        ])
        .await;
    assert_eq!(seen.status, 403, "{seen:?}");
    // No header, or one that is not an address: the source is unknown.
    for headers in [
        vec![],
        vec![("X-Forwarded-For", "10.1.1.1, unknown")],
        vec![("X-Forwarded-For", "")],
    ] {
        let seen = proxied.post(&headers).await;
        assert_eq!(seen.status, 403, "{headers:?}: {seen:?}");
        assert_eq!(seen.error_code(), "source_not_allowed");
    }

    let proxied = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS", "1"),
        ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "2"),
        ("PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS", "3600"),
    ])
    .await;
    let xff = |forged: &str, real: &str| format!("{forged}, {real}");
    for forged in ["1.1.1.1", "2.2.2.2"] {
        let h = xff(forged, "198.51.100.4");
        assert_eq!(proxied.post(&[("X-Forwarded-For", &h)]).await.status, 200);
    }
    // A new forged entry does not give the client a new budget...
    for forged in ["3.3.3.3", "4.4.4.4", "198.51.100.9"] {
        let h = xff(forged, "198.51.100.4");
        let seen = proxied.post(&[("X-Forwarded-For", &h)]).await;
        assert_eq!(seen.status, 429, "{forged}: {seen:?}");
    }
    // ...and naming the victim spends nothing of the victim's.
    for _ in 0..3 {
        let h = xff("198.51.100.4", "198.51.100.9");
        let seen = proxied.post(&[("X-Forwarded-For", &h)]).await;
        assert!(seen.status == 200 || seen.status == 429);
    }
    let h = xff("198.51.100.9", "198.51.100.10");
    assert_eq!(proxied.post(&[("X-Forwarded-For", &h)]).await.status, 200);
}

/// Startup refuses what cannot work, with a message that names the setting.
#[test]
fn startup_refusals() {
    let none = ("PSCAT_MCP_AUTH", "none");
    // Already past.
    let stderr = refused(&[none, ("PSCAT_MCP_AUTH_NONE_UNTIL", "2020-01-01T00:00:00Z")]);
    assert!(
        stderr.contains("has expired") && stderr.contains("already passed"),
        "{stderr}"
    );
    assert!(stderr.contains("PSCAT_MCP_AUTH_NONE_UNTIL"), "{stderr}");
    // With an offset, too.
    let stderr = refused(&[
        none,
        ("PSCAT_MCP_AUTH_NONE_UNTIL", "2020-01-01T00:00:00+02:00"),
    ]);
    assert!(stderr.contains("already passed"), "{stderr}");
    // Not a time, a bad CIDR, a zero limit or window.
    let stderr = refused(&[none, ("PSCAT_MCP_AUTH_NONE_UNTIL", "next tuesday")]);
    assert!(stderr.contains("not an RFC 3339 time"), "{stderr}");
    let stderr = refused(&[
        none,
        ("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS", "10.0.0.0/8,banana"),
    ]);
    assert!(
        stderr.contains("PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS") && stderr.contains("banana"),
        "{stderr}"
    );
    let stderr = refused(&[none, ("PSCAT_MCP_AUTH_NONE_RATE_LIMIT", "0")]);
    assert!(stderr.contains("RATE_LIMIT"), "{stderr}");
    let stderr = refused(&[none, ("PSCAT_MCP_AUTH_NONE_RATE_WINDOW_SECS", "0")]);
    assert!(stderr.contains("RATE_WINDOW_SECS"), "{stderr}");
    // Async jobs (and so Tasks) are one principal's: refused with `none`.
    for env in [
        vec![none, ("PSCAT_MCP_ASYNC_JOBS", "true")],
        vec![
            ("PSCAT_MCP_AUTH", "bearer,none"),
            ("PSCAT_MCP_AUTH_TOKENS", TOKEN),
            ("PSCAT_MCP_ASYNC_JOBS", "true"),
            ("PSCAT_MCP_TASKS", "true"),
        ],
    ] {
        let stderr = refused(&env);
        assert!(
            stderr.contains("cannot be combined with async jobs"),
            "{env:?}: {stderr}"
        );
    }
    // `bearer` alone with async jobs is fine to configure (not started here).
}

/// A future expiry starts and serves, and the startup line says when.
#[tokio::test(flavor = "multi_thread")]
async fn a_future_expiry_serves() {
    let srv = Server::start(&[
        ("PSCAT_MCP_AUTH", "none"),
        ("PSCAT_MCP_AUTH_NONE_UNTIL", "2999-01-01T00:00:00Z"),
    ])
    .await;
    assert_eq!(srv.post(&[]).await.status, 200);
    let text = srv.log.lock().unwrap().join("\n");
    assert!(text.contains("until 2999-01-01T00:00:00+00:00"), "{text}");
}

// ------------------------------------------------ in-process, injected clock

const T0: i64 = 1_800_000_000_000;

struct InProcess {
    url: String,
    ms: Arc<AtomicI64>,
    ct: CancellationToken,
}

impl Drop for InProcess {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

/// The router over `chain`, served with connection info, as the binary does.
async fn serve_in_process(chain: AuthChain, ms: Arc<AtomicI64>) -> InProcess {
    let limits = LimitsConfig {
        interp_time_limit_ms: Some(0),
        max_steps: 0,
        max_concurrent_renders: Some(1),
        ..LimitsConfig::default()
    };
    let backend = SubprocessBackend::new(SERVER.into(), limits.clone());
    let service = Arc::new(RenderService::new(Arc::new(backend), limits));
    let http = HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        auth_tokens: vec![],
        auth_token_file: None,
        allow_unauthenticated: false,
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        allowed_origins: vec![],
        shutdown_grace_secs: 1,
        async_jobs: false,
    };
    let ct = CancellationToken::new();
    let app = router(
        service,
        Arc::new(chain),
        mcp_server_config(&http, ct.child_token()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop.cancelled_owned())
        .await;
    });
    InProcess {
        url: format!("http://{addr}/mcp"),
        ms,
        ct,
    }
}

fn guarded(until_ms: i64, ms: &Arc<AtomicI64>) -> NoAuth {
    let clock = ms.clone();
    let cfg = NoneAuthConfig {
        until: Some(
            chrono::DateTime::from_timestamp_millis(until_ms)
                .unwrap()
                .to_rfc3339(),
        ),
        ..NoneAuthConfig::default()
    };
    NoAuth::guarded(
        NoneGuard::with_clock(
            &cfg,
            Arc::new(move || {
                chrono::DateTime::from_timestamp_millis(clock.load(Ordering::SeqCst)).unwrap()
            }),
        )
        .unwrap(),
    )
}

/// A running server stops admitting anonymous requests when the expiry
/// passes, even though it started before it: 403, no bare 401 and no
/// challenge for `none` alone. A valid token still works beside it.
#[tokio::test(flavor = "multi_thread")]
async fn requests_are_refused_after_the_expiry_while_running() {
    let ms = Arc::new(AtomicI64::new(T0));
    let alone = serve_in_process(
        AuthChain::new(vec![Box::new(guarded(T0 + 60_000, &ms))]),
        ms.clone(),
    )
    .await;
    assert_eq!(post(&alone.url, &[]).await.status, 200);
    alone.ms.store(T0 + 59_999, Ordering::SeqCst);
    assert_eq!(post(&alone.url, &[]).await.status, 200);
    alone.ms.store(T0 + 60_000, Ordering::SeqCst);
    let seen = post(&alone.url, &[]).await;
    assert_eq!(seen.status, 403, "{seen:?}");
    assert_eq!(seen.error_code(), "unauthenticated_access_expired");
    assert_eq!(seen.challenge, None);

    let ms = Arc::new(AtomicI64::new(T0));
    let both = serve_in_process(
        AuthChain::new(vec![
            Box::new(BearerTokenAuth::new([TOKEN])),
            Box::new(guarded(T0 + 60_000, &ms)),
        ]),
        ms.clone(),
    )
    .await;
    let ok = bearer(TOKEN);
    assert_eq!(post(&both.url, &[]).await.status, 200);
    both.ms.store(T0 + 61_000, Ordering::SeqCst);
    let seen = post(&both.url, &[]).await;
    assert_eq!(seen.status, 403, "{seen:?}");
    assert_eq!(
        seen.challenge.as_deref(),
        Some(r#"Bearer realm="pscat-mcp""#)
    );
    assert_eq!(
        post(&both.url, &[("Authorization", ok.as_str())])
            .await
            .status,
        200
    );
}
