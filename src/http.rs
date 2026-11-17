//! HTTP transport: `/mcp` (Streamable HTTP, authenticated), the health
//! paths `/healthz` and `/health`, and with `oauth` enabled the Protected
//! Resource Metadata.
//!
//! * `GET /healthz` and `GET /health` — the same unauthenticated liveness
//!   handler; does no rendering. `/health` exists because Cloud Run's front
//!   end reserves paths ending in `z` and answers 404 for `/healthz` from
//!   outside the service; `/healthz` stays for the in-container probes.
//! * `GET /.well-known/oauth-protected-resource` and the same with the
//!   resource URL's path appended (`…/mcp`), only with `oauth` enabled:
//!   the RFC 9728 document naming the resource, the authorization server and
//!   the scopes; unauthenticated, as the MCP authorization spec requires.
//! * `/mcp` — rmcp's [`StreamableHttpService`] in stateless mode with JSON
//!   responses, behind the [`Authenticator`] (every enabled auth method,
//!   checked before rmcp sees the request, so an unauthenticated request
//!   never reaches a tool), a
//!   request-body limit, and a small `Accept` fix-up for `POST` (see
//!   [`accept_json`]).
//!
//! Every response carries an `x-request-id` (generated when the client sent
//! none), and each request is traced in a span with method, path and
//! request id, plus `auth_method` once an auth method has accepted it.
//! Query strings and credentials are never logged. (Capture mode, opt-in and
//! loopback-only, is the one exception for request bodies: see `capture.rs`.)

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use http_body_util::BodyExt as _;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};

use crate::auth::{
    AuthChain, AuthMethod, Authenticator, BearerTokenAuth, Denial, ExpiredNoAuth, NoAuth,
};
use crate::auth_none::NoneGuard;
use crate::capture::{Capture, capture_layer};
use crate::config::{AuthConfig, AuthMethodKind, HttpConfig};
use crate::mcp::PscatMcp;
use crate::oauth::OauthAuth;
use crate::render::RenderService;

/// Slack added to the body limit for the JSON-RPC envelope and arguments.
const BODY_ENVELOPE_BYTES: usize = 64 * 1024;

/// Largest `/mcp` request body: the largest text input a tool takes (the
/// source limit, `handwrite`'s text limit or a `review_sheet`'s sources
/// together, whichever is larger;
/// [`crate::config::LimitsConfig::max_text_bytes`]) times 6
/// (worst-case JSON escaping, `\u00XX`) plus envelope slack. Inputs a little
/// over their limit still reach the tool and get a structured
/// `source_too_large` error; only truly huge bodies are cut off with `413`.
pub fn mcp_body_limit(max_text_bytes: usize) -> usize {
    max_text_bytes
        .saturating_mul(6)
        .saturating_add(BODY_ENVELOPE_BYTES)
}

/// Whether `ip` is a loopback address (including IPv4-mapped `::ffff:127.x`).
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Capture mode records request bodies, which are users' PostScript, so it
/// is for local debugging only: refuse it unless the bind address is
/// loopback (the same rule as `--allow-unauthenticated`).
pub fn check_capture_bind(http: &HttpConfig) -> anyhow::Result<()> {
    if !is_loopback(http.bind) {
        anyhow::bail!(
            "refusing to start: --capture-file records request bodies (users' PostScript) and is \
             only permitted on a loopback bind address, but the server would listen on {}",
            http.bind
        );
    }
    Ok(())
}

/// Choose the authenticator for `http`.
///
/// * Tokens configured (flag, env or file) → [`BearerTokenAuth`].
/// * No tokens and no `--allow-unauthenticated` → error.
/// * `--allow-unauthenticated` on a non-loopback bind → error, always.
/// * `--allow-unauthenticated` on loopback without tokens → [`NoAuth`].
pub fn authenticator(http: &HttpConfig) -> anyhow::Result<Arc<dyn Authenticator>> {
    if http.allow_unauthenticated && !is_loopback(http.bind) {
        anyhow::bail!(
            "refusing to start: --allow-unauthenticated is only permitted on a loopback bind \
             address, but the server would listen on {}",
            http.bind
        );
    }
    let tokens = http.load_tokens().context("reading the auth token file")?;
    if !tokens.is_empty() {
        if http.allow_unauthenticated {
            tracing::warn!("auth tokens are configured; --allow-unauthenticated is ignored");
        }
        return Ok(Arc::new(BearerTokenAuth::new(tokens)));
    }
    if !http.allow_unauthenticated {
        anyhow::bail!(
            "no auth tokens configured: set PSCAT_MCP_AUTH_TOKENS or PSCAT_MCP_AUTH_TOKEN_FILE \
             (or --auth-tokens / --auth-token-file); for local development on a loopback \
             address, --allow-unauthenticated serves /mcp without authentication"
        );
    }
    tracing::warn!(
        bind = %http.bind,
        "serving /mcp WITHOUT authentication (loopback development mode)"
    );
    Ok(Arc::new(NoAuth::default()))
}

/// Choose the authenticator for `http` and `auth`.
///
/// * `PSCAT_MCP_AUTH` / `--auth` unset: exactly [`authenticator`].
/// * Set: an [`AuthChain`] of the listed methods, in order. An empty or
///   unknown list is refused. `bearer` needs at least one token; `oauth`
///   needs its issuer, resource and allowlist ([`OauthAuth::from_config`]).
///   `none` is the explicit
///   no-auth mode (issue #120, [`crate::auth_none`]): it is refused together
///   with async jobs (every anonymous caller is one principal, so one could
///   read or cancel another's jobs; Tasks need async jobs, so are covered),
///   (an expired `PSCAT_MCP_AUTH_NONE_UNTIL` is not refused: `none` is
///   disabled instead, see below); it may run off loopback, and a loud
///   warning is logged. A `PSCAT_MCP_AUTH_NONE_UNTIL` that has already
///   passed at startup (issue #128) starts the server with `none` left out
///   of the enabled methods and [`ExpiredNoAuth`] in its place, which
///   answers `403 unauthenticated_access_expired` to a request with no
///   credential. Scale-to-zero cold starts must not take the other methods
///   down; with `none` alone, every request gets that 403. `--allow-unauthenticated` is
///   still refused on a non-loopback bind, and is otherwise ignored (with a
///   warning): the list says what is accepted.
pub fn authenticator_with(
    http: &HttpConfig,
    auth: &AuthConfig,
) -> anyhow::Result<Arc<dyn Authenticator>> {
    let kinds = auth
        .methods()
        .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
    if auth.oauth.is_configured()
        && !kinds
            .as_ref()
            .is_some_and(|k| k.contains(&AuthMethodKind::Oauth))
    {
        tracing::warn!(
            "PSCAT_MCP_OAUTH_* settings are given but `oauth` is not in PSCAT_MCP_AUTH; they are \
             ignored"
        );
    }
    let Some(kinds) = kinds else {
        warn_unused_none_settings(auth);
        return authenticator(http);
    };
    if kinds.contains(&AuthMethodKind::None) {
        if http.async_jobs {
            anyhow::bail!(
                "refusing to start: auth method `none` (PSCAT_MCP_AUTH / --auth) cannot be \
                 combined with async jobs (PSCAT_MCP_ASYNC_JOBS, which MCP Tasks also need): \
                 every unauthenticated caller is the same principal, so one could poll, fetch or \
                 cancel another's jobs. Turn async jobs off, or remove `none`"
            );
        }
    } else {
        warn_unused_none_settings(auth);
    }
    if http.allow_unauthenticated {
        if !is_loopback(http.bind) {
            anyhow::bail!(
                "refusing to start: --allow-unauthenticated is only permitted on a loopback bind \
                 address, but the server would listen on {}",
                http.bind
            );
        }
        tracing::warn!("PSCAT_MCP_AUTH is set; --allow-unauthenticated is ignored");
    }
    let tokens = http.load_tokens().context("reading the auth token file")?;
    let mut methods: Vec<Box<dyn AuthMethod>> = Vec::with_capacity(kinds.len());
    for kind in &kinds {
        match kind {
            AuthMethodKind::Bearer => {
                if tokens.is_empty() {
                    anyhow::bail!(
                        "refusing to start: auth method `bearer` is enabled (PSCAT_MCP_AUTH / \
                         --auth) but no bearer tokens are configured: set PSCAT_MCP_AUTH_TOKENS \
                         or PSCAT_MCP_AUTH_TOKEN_FILE (or --auth-tokens / --auth-token-file)"
                    );
                }
                methods.push(Box::new(BearerTokenAuth::new(tokens.iter())));
            }
            AuthMethodKind::Oauth => {
                let oauth = OauthAuth::from_config(&auth.oauth)
                    .map_err(|e| anyhow::anyhow!("refusing to start: {e:#}"))?;
                methods.push(Box::new(oauth));
            }
            AuthMethodKind::None => {
                let guard = NoneGuard::new(&auth.none)
                    .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
                if let Some(until) = guard.until().filter(|_| guard.is_expired()) {
                    tracing::warn!(
                        until = %until.to_rfc3339(),
                        "auth method `none` is DISABLED: PSCAT_MCP_AUTH_NONE_UNTIL / \
                         --auth-none-until has already passed, so requests with no credential \
                         get 403 unauthenticated_access_expired; the other methods are \
                         unaffected. Remove `none` from PSCAT_MCP_AUTH or set a later time"
                    );
                    methods.push(Box::new(ExpiredNoAuth));
                    continue;
                }
                tracing::warn!(
                    guard = %guard.summary(),
                    "auth method `none` is ENABLED: /mcp accepts requests with NO credentials \
                     from anyone who can reach this server, who can then spend its render CPU \
                     time and quota. For staging only; never leave it on in production"
                );
                methods.push(Box::new(NoAuth::guarded(guard)));
            }
        }
    }
    if !kinds.contains(&AuthMethodKind::Bearer) && !tokens.is_empty() {
        tracing::warn!(
            "bearer tokens are configured but `bearer` is not in PSCAT_MCP_AUTH; they are ignored"
        );
    }
    let chain = AuthChain::new(methods);
    if chain.methods().is_empty() {
        tracing::warn!(
            "no auth method is enabled (`none` is the only one in PSCAT_MCP_AUTH and it has \
             expired): every request to /mcp gets 403 unauthenticated_access_expired"
        );
    }
    tracing::info!(
        auth_methods = %chain.methods().join(","),
        "auth methods enabled (PSCAT_MCP_AUTH)"
    );
    Ok(Arc::new(chain))
}

/// Warn when `--auth-none-*` settings are given but `none` is not enabled.
fn warn_unused_none_settings(auth: &AuthConfig) {
    if auth.none.is_customised() {
        tracing::warn!(
            "PSCAT_MCP_AUTH_NONE_* settings are given but `none` is not in PSCAT_MCP_AUTH; \
             they are ignored"
        );
    }
}

/// rmcp Streamable HTTP settings for `http`: stateless, JSON responses, no
/// SSE keep-alive, Host and Origin validation from the configuration.
///
/// * `allowed_hosts` containing `*` disables the Host check (only behind a
///   proxy that validates it); an empty list falls back to loopback names
///   rather than rmcp's "empty means anything".
/// * Empty `allowed_origins` rejects every request that carries an
///   `Origin` header (MCP clients do not send one; browsers do).
pub fn mcp_server_config(http: &HttpConfig, ct: CancellationToken) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct);

    let hosts: Vec<&str> = http
        .allowed_hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .collect();
    config = if hosts.contains(&"*") {
        config.disable_allowed_hosts()
    } else if hosts.is_empty() {
        config.with_allowed_hosts(["localhost", "127.0.0.1", "::1"])
    } else {
        config.with_allowed_hosts(hosts)
    };

    let origins: Vec<&str> = http
        .allowed_origins
        .iter()
        .map(|o| o.trim())
        .filter(|o| !o.is_empty())
        .collect();
    if origins.is_empty() {
        config.enforce_origin_validation()
    } else {
        config.with_allowed_origins(origins)
    }
}

/// The application router: `/healthz`, `/health` and authenticated `/mcp` over
/// `service`. Testable without binding a port. The request-body limit is
/// derived from the service's input limits and also applied to rmcp's
/// own limit (overriding `mcp_config.max_request_body_bytes`).
pub fn router(
    service: Arc<RenderService>,
    auth: Arc<dyn Authenticator>,
    mcp_config: StreamableHttpServerConfig,
) -> Router {
    router_with_capture(service, auth, mcp_config, None)
}

/// [`router`] with capture mode (see [`crate::capture`]): when `capture` is
/// set, each request that passed authentication and the body limit is
/// recorded just before rmcp handles it.
pub fn router_with_capture(
    service: Arc<RenderService>,
    auth: Arc<dyn Authenticator>,
    mcp_config: StreamableHttpServerConfig,
    capture: Option<Arc<Capture>>,
) -> Router {
    let auth_metadata = auth.published_metadata();
    let limits = service.limits();
    let body_limit = mcp_body_limit(limits.max_text_bytes());
    let mcp_config = mcp_config.with_max_request_body_bytes(body_limit);
    let mcp = StreamableHttpService::new(
        move || Ok(PscatMcp::for_http(service.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );
    // Outer to inner: authenticate (before the body is used), then read the
    // body under the limit, then capture (which records the `Accept` the
    // client sent), then the `Accept` fix-up, then rmcp.
    let mcp = ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(auth, require_auth))
        .layer(middleware::from_fn_with_state(body_limit, limit_body))
        .layer(middleware::from_fn_with_state(capture, capture_layer))
        .layer(middleware::from_fn(accept_json))
        .service(mcp);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/health", get(healthz))
        .merge(metadata_routes(auth_metadata))
        .route_service("/mcp", mcp)
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|req: &Request| {
                            let request_id = req
                                .headers()
                                .get("x-request-id")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            // `auth_method` is recorded by `require_auth`
                            // once a method accepts the request.
                            tracing::info_span!(
                                "http",
                                method = %req.method(),
                                path = %req.uri().path(),
                                request_id = %request_id,
                                auth_method = tracing::field::Empty,
                            )
                        })
                        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
                )
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
}

/// RFC 9728 Protected Resource Metadata routes, unauthenticated like the
/// health paths; empty unless an enabled method (`oauth`) publishes one.
fn metadata_routes(meta: Option<crate::auth::ResourceMetadata>) -> Router {
    let mut app = Router::new();
    if let Some(meta) = meta {
        let doc = Arc::new(meta.document);
        for path in &meta.paths {
            app = app.route(path, get(resource_metadata).with_state(doc.clone()));
        }
    }
    app
}

/// Which of `application/json` and `text/event-stream` a request's `Accept`
/// header(s) mention, by name or by a wildcard.
#[derive(Default, Debug, PartialEq, Eq)]
struct AcceptSeen {
    /// Listed by name, whatever its weight (rmcp applies `q=0` itself).
    json: bool,
    sse: bool,
    /// `*/*` or `application/*` with a non-zero weight, or no `Accept` at all.
    json_by_wildcard: bool,
}

/// Whether a media-range parameter list carries a zero weight (`q=0`,
/// `q=0.000`). A weight that does not parse counts as non-zero, as in rmcp.
fn has_zero_weight<'a>(mut params: impl Iterator<Item = &'a str>) -> bool {
    params.any(|p| {
        p.split_once('=').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("q")
                && value.trim().parse::<f32>().is_ok_and(|q| q == 0.0)
        })
    })
}

fn scan_accept(headers: &http::HeaderMap) -> AcceptSeen {
    let mut seen = AcceptSeen::default();
    let mut any_header = false;
    for value in headers.get_all(header::ACCEPT) {
        any_header = true;
        let Ok(value) = value.to_str() else { continue };
        for entry in value.split(',') {
            let mut parts = entry.split(';');
            let essence = parts.next().unwrap_or("").trim();
            if essence.eq_ignore_ascii_case("application/json") {
                seen.json = true;
            } else if essence.eq_ignore_ascii_case("text/event-stream") {
                seen.sse = true;
            } else if (essence == "*/*" || essence.eq_ignore_ascii_case("application/*"))
                && !has_zero_weight(parts)
            {
                seen.json_by_wildcard = true;
            }
        }
    }
    // RFC 9110 12.5.1: no `Accept` means any media type is acceptable.
    seen.json_by_wildcard |= !any_header;
    seen
}

/// Make `POST /mcp` accept clients that do not list `text/event-stream`.
///
/// rmcp 3.9 answers `406` to a POST unless `Accept` names both
/// `application/json` and `text/event-stream` (what the MCP spec asks of a
/// client), and does not read `*/*` or a missing header as either. This
/// server always answers a POST with a single JSON body (`json_response`),
/// never a stream, so the second type is a formality. A request whose
/// `Accept` allows JSON gets the missing names appended, so rmcp's check
/// passes:
///
/// * `application/json` alone (issue #17): `text/event-stream` is added.
/// * No `Accept` header, `*/*` and `application/*` (RFC 9110: anything is
///   acceptable): both names are added; the answer is JSON.
/// * Anything that does not allow JSON (`text/html`, `text/event-stream`
///   alone, `application/json;q=0`) is left alone and gets rmcp's `406`: the
///   server could not honestly answer it.
///
/// `GET` and `DELETE` are untouched: a stateless server has no event stream
/// to open (`GET` is 405 whatever `Accept` says) and `DELETE` carries no
/// `Accept` check, so there is nothing to fix up. This
/// runs after authentication, the body limit and capture, so none of those
/// is bypassed, and capture records the `Accept` the client sent.
async fn accept_json(mut req: Request, next: Next) -> Response {
    if req.method() == http::Method::POST {
        let seen = scan_accept(req.headers());
        let headers = req.headers_mut();
        if !seen.json && seen.json_by_wildcard {
            headers.append(header::ACCEPT, HeaderValue::from_static("application/json"));
        }
        if !seen.sse && (seen.json || seen.json_by_wildcard) {
            headers.append(
                header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            );
        }
    }
    next.run(req).await
}

/// Most bytes of an oversized or rejected body read and thrown away before
/// answering `413` or `401`, and the longest that may take. Answering while the client is still
/// uploading makes the server close a connection with unread data, which the
/// client can see as a TCP reset instead of the `413` (issue #20). Bodies
/// declared larger than this are refused without reading.
const DRAIN_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DRAIN_MAX_TIME: Duration = Duration::from_secs(5);

/// Read the whole request body, at most `limit` bytes, before rmcp sees it.
///
/// Larger bodies get `413`: at once when `Content-Length` is over
/// [`DRAIN_MAX_BYTES`], otherwise once the body passes `limit`, after the
/// rest has been drained. This covers chunked bodies (no `Content-Length`)
/// too, which rmcp would otherwise turn into a `500` (issue #10). A body cut
/// off or malformed in transit is a `400`. rmcp reads the whole body anyway,
/// so buffering it here costs nothing extra.
async fn limit_body(State(limit): State<usize>, req: Request, next: Next) -> Response {
    let too_large = || {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("request body is larger than {limit} bytes"),
        )
            .into_response()
    };
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > DRAIN_MAX_BYTES) {
        return too_large();
    }
    let (parts, mut body) = req.into_parts();
    let mut buf = Vec::with_capacity(declared.map_or(0, |n| (n as usize).min(limit)));
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                if buf.len() + data.len() > limit {
                    drain(body, (buf.len() + data.len()) as u64).await;
                    return too_large();
                }
                buf.extend_from_slice(data);
            }
            Err(e) => {
                tracing::debug!(error = %e, "reading the request body failed");
                return (StatusCode::BAD_REQUEST, "could not read the request body")
                    .into_response();
            }
        }
    }
    next.run(Request::from_parts(parts, Body::from(buf))).await
}

/// Read and discard the rest of `body`, up to [`DRAIN_MAX_BYTES`] in total
/// (`seen` already read) and [`DRAIN_MAX_TIME`].
async fn drain(mut body: Body, mut seen: u64) {
    let _ = tokio::time::timeout(DRAIN_MAX_TIME, async {
        while seen <= DRAIN_MAX_BYTES {
            match body.frame().await {
                Some(Ok(frame)) => {
                    seen += frame.data_ref().map_or(0, |d| d.len() as u64);
                }
                _ => break,
            }
        }
    })
    .await;
}

async fn healthz() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

/// The Protected Resource Metadata document (RFC 9728). Public and static,
/// so any origin may read it (browser-based MCP clients fetch it too).
async fn resource_metadata(State(doc): State<Arc<serde_json::Value>>) -> Response {
    let mut resp = axum::Json(doc.as_ref().clone()).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    resp
}

/// Authenticate from the headers alone, by every enabled method in turn. An
/// accepted request goes on with its body untouched, and the request span
/// records `auth_method`. A rejected one never reaches the body limit, rmcp
/// or a render; unless it declares more than [`DRAIN_MAX_BYTES`], its body is
/// read and discarded first (at most [`DRAIN_MAX_TIME`]) so that a client
/// still uploading gets the `401` rather than a connection reset (issue #30).
async fn require_auth(
    State(auth): State<Arc<dyn Authenticator>>,
    req: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();
    match auth.admit(&parts).await {
        Ok(principal) => {
            tracing::Span::current().record("auth_method", principal.method);
            tracing::debug!(
                principal = %principal.id,
                auth_method = principal.method,
                "authenticated"
            );
            parts.extensions.insert(principal);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(denial) => {
            tracing::warn!(
                reason = denial.error.code(),
                rejected_by = %denial.rejected_by.join(","),
                "rejected unauthenticated request"
            );
            let declared = parts
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if declared.is_none_or(|n| n <= DRAIN_MAX_BYTES) {
                drain(body, 0).await;
            }
            denied(&denial)
        }
    }
}

/// The answer to a denied request: the error's status (`401`, `403` or
/// `429`), a JSON body with the error's code and message, a `Retry-After`
/// when the error has one, and the merged challenge.
fn denied(denial: &Denial) -> Response {
    let body = serde_json::json!({
        "error": denial.error.code(),
        "message": denial.error.to_string(),
    });
    let mut resp = (denial.error.status(), axum::Json(body)).into_response();
    if let Some(v) = denial.error.retry_after().map(HeaderValue::from) {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    if let Some(v) = denial
        .challenge
        .as_deref()
        .and_then(|c| HeaderValue::from_str(c).ok())
    {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Resolves on SIGTERM or Ctrl-C (SIGINT).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!(signal = "SIGINT", "shutdown requested"),
        _ = terminate => tracing::info!(signal = "SIGTERM", "shutdown requested"),
    }
}

/// Refuse the `none` auth method together with artifact links. With
/// in-memory links every unauthenticated caller is the one principal
/// `anonymous`, so one of them could fill the results budget the links share
/// with every other client (`PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES`), and read
/// the others' links; the same reason async jobs are refused with `none`.
/// With Cloud Storage links, every anonymous caller would share one upload
/// quota, and anyone on the internet could mint signed URLs (bearer
/// capabilities) to objects in the operator's bucket.
pub fn check_none_with_artifact_links(
    auth: &AuthConfig,
    service: &RenderService,
) -> anyhow::Result<()> {
    let none = matches!(auth.methods(), Ok(Some(kinds)) if kinds.contains(&AuthMethodKind::None));
    match service.artifacts().map(|a| a.name()) {
        Some("gcs") if none => anyhow::bail!(
            "refusing to start: auth method `none` (PSCAT_MCP_AUTH / --auth) cannot be combined \
             with Cloud Storage artifact links (PSCAT_MCP_ARTIFACT_LINKS=gcs): every \
             unauthenticated caller is the same principal, so they would share one upload quota, \
             and anyone who can reach the server could upload to the bucket and mint signed URLs, \
             which work for whoever holds them until they expire. Turn the links off, or remove \
             `none`"
        ),
        Some(_) if none => anyhow::bail!(
            "refusing to start: auth method `none` (PSCAT_MCP_AUTH / --auth) cannot be combined \
             with in-memory artifact links (PSCAT_MCP_ARTIFACT_LINKS=memory): every \
             unauthenticated caller is the same principal, so one could fill the results budget \
             the links share with authenticated clients (PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES) \
             and read another's links. Turn the links off, or remove `none`"
        ),
        _ => Ok(()),
    }
}

/// Serve until SIGTERM/SIGINT, then shut down gracefully.
///
/// On a signal the listener stops accepting connections and in-flight
/// requests get up to `shutdown_grace_secs` to finish. The rmcp
/// cancellation token is cancelled *after* that drain: in JSON-response
/// mode cancelling it aborts in-flight tool calls, so cancelling first would
/// turn every running render into an error. Whatever is still running when
/// the grace period ends is cancelled and the function returns.
pub async fn serve(config: HttpConfig, service: Arc<RenderService>) -> anyhow::Result<()> {
    serve_with_capture(config, &AuthConfig::default(), service, None).await
}

/// [`serve`] with the auth methods of `auth_config` (see
/// [`authenticator_with`]) and capture mode on when `capture` is set.
pub async fn serve_with_capture(
    config: HttpConfig,
    auth_config: &AuthConfig,
    service: Arc<RenderService>,
    capture: Option<Arc<Capture>>,
) -> anyhow::Result<()> {
    if capture.is_some() {
        check_capture_bind(&config)?;
    }
    check_none_with_artifact_links(auth_config, &service)?;
    let auth = authenticator_with(&config, auth_config)?;
    let mcp_ct = CancellationToken::new();
    let jobs_service = service.clone();
    let app = router_with_capture(
        service,
        auth.clone(),
        mcp_server_config(&config, mcp_ct.clone()),
        capture,
    );

    let listener = tokio::net::TcpListener::bind(config.addr())
        .await
        .with_context(|| format!("binding {}", config.addr()))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    tracing::info!(
        %addr,
        auth = %auth.describe(),
        auth_methods = %auth.methods().join(","),
        async_jobs = jobs_service.jobs().enabled(),
        artifact_links = jobs_service.artifacts().map_or("off", |a| a.name()),
        allowed_hosts = ?config.allowed_hosts,
        allowed_origins = ?config.allowed_origins,
        "listening; MCP endpoint at /mcp, health at /healthz and /health"
    );
    if !is_loopback(config.bind)
        && !config.allowed_hosts.iter().any(|h| h.trim() == "*")
        && config
            .allowed_hosts
            .iter()
            .all(|h| matches!(h.trim(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
    {
        tracing::warn!(
            "bound to a non-loopback address but PSCAT_MCP_ALLOWED_HOSTS only lists loopback \
             names; requests for any public hostname will be rejected with 403"
        );
    }

    let stop_accepting = CancellationToken::new();
    let mut server = tokio::spawn({
        let stop = stop_accepting.clone();
        async move {
            // Connection info gives `none`'s guard rails the TCP peer address.
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
        }
    });

    tokio::select! {
        res = &mut server => {
            mcp_ct.cancel();
            return res.context("server task failed")?.context("server error");
        }
        _ = shutdown_signal() => {}
    }

    let grace = Duration::from_secs(config.shutdown_grace_secs);
    tracing::info!(
        grace_secs = config.shutdown_grace_secs,
        "no longer accepting connections; draining in-flight requests"
    );
    stop_accepting.cancel();
    let drained = tokio::time::timeout(grace, &mut server).await;
    mcp_ct.cancel();
    match drained {
        Ok(res) => {
            res.context("server task failed")?.context("server error")?;
            tracing::info!("all requests finished; shut down cleanly");
        }
        Err(_) => {
            tracing::warn!("grace period elapsed; cancelling remaining requests");
            // Give cancelled handlers a moment to answer, then stop waiting.
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut server).await;
            server.abort();
        }
    }
    // Async jobs are killed, not drained: a job may run for minutes, and its
    // in-memory result would not outlive the process anyway.
    jobs_service.shutdown_jobs(Duration::from_secs(2)).await;
    Ok(())
}
