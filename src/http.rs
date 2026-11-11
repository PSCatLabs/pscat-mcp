//! HTTP transport: `/mcp` (Streamable HTTP, authenticated) and `/healthz`.
//!
//! * `GET /healthz` — unauthenticated liveness probe; does no rendering.
//! * `/mcp` — rmcp's [`StreamableHttpService`] in stateless mode with JSON
//!   responses, behind the [`Authenticator`] (checked before rmcp sees the
//!   request, so an unauthenticated request never reaches a tool) and a
//!   request-body limit.
//!
//! Every response carries an `x-request-id` (generated when the client sent
//! none), and each request is traced in a span with method, path and
//! request id. Query strings are never logged.

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

use crate::auth::{AuthError, Authenticator, BearerTokenAuth, NoAuth};
use crate::config::HttpConfig;
use crate::mcp::PscatMcp;
use crate::render::RenderService;

/// Slack added to the body limit for the JSON-RPC envelope and arguments.
const BODY_ENVELOPE_BYTES: usize = 64 * 1024;

/// Largest `/mcp` request body: the source limit times 6 (worst-case JSON
/// escaping, `\u00XX`) plus envelope slack. Sources a little over the source
/// limit still reach the tool and get a structured `source_too_large`
/// error; only truly huge bodies are cut off with `413`.
pub fn mcp_body_limit(max_source_bytes: usize) -> usize {
    max_source_bytes
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
    Ok(Arc::new(NoAuth))
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

/// The application router: `/healthz` and authenticated `/mcp` over
/// `service`. Testable without binding a port. The request-body limit is
/// derived from the service's `max_source_bytes` and also applied to rmcp's
/// own limit (overriding `mcp_config.max_request_body_bytes`).
pub fn router(
    service: Arc<RenderService>,
    auth: Arc<dyn Authenticator>,
    mcp_config: StreamableHttpServerConfig,
) -> Router {
    let body_limit = mcp_body_limit(service.limits().max_source_bytes);
    let mcp_config = mcp_config.with_max_request_body_bytes(body_limit);
    let mcp = StreamableHttpService::new(
        move || Ok(PscatMcp::new(service.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );
    // Outer to inner: authenticate (before the body is used), then read the
    // body under the limit, then rmcp.
    let auth_state = AuthState {
        auth,
        drain_max: body_limit as u64,
    };
    let mcp = ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(auth_state, require_auth))
        .layer(middleware::from_fn_with_state(body_limit, limit_body))
        .service(mcp);

    Router::new()
        .route("/healthz", get(healthz))
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
                            tracing::info_span!(
                                "http",
                                method = %req.method(),
                                path = %req.uri().path(),
                                request_id = %request_id,
                            )
                        })
                        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
                )
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
}

/// Most bytes of an oversized body read and thrown away before answering
/// `413`, and the longest that may take (also the time limit for draining a
/// rejected body before `401`). Answering while the client is still
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
                    drain(body, (buf.len() + data.len()) as u64, DRAIN_MAX_BYTES).await;
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

/// Read and discard the rest of `body`, up to `max` bytes in total (`seen`
/// already read) and [`DRAIN_MAX_TIME`].
async fn drain(mut body: Body, mut seen: u64, max: u64) {
    let _ = tokio::time::timeout(DRAIN_MAX_TIME, async {
        while seen <= max {
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

#[derive(Clone)]
struct AuthState {
    auth: Arc<dyn Authenticator>,
    /// Most bytes of a rejected request's body read and thrown away before
    /// answering `401`: the `/mcp` body limit.
    drain_max: u64,
}

/// Authenticate from the headers alone. An accepted request goes on with its
/// body untouched. A rejected one never reaches the body limit, rmcp or a
/// render; its body, if it declares no more than `drain_max` bytes, is read
/// and discarded first (at most [`DRAIN_MAX_TIME`]) so that a client still
/// uploading gets the `401` rather than a connection reset (issue #30).
async fn require_auth(State(state): State<AuthState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    match state.auth.authenticate(&parts) {
        Ok(principal) => {
            tracing::debug!(principal = %principal.id, "authenticated");
            parts.extensions.insert(principal);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(err) => {
            tracing::warn!(reason = err.code(), "rejected unauthenticated request");
            let declared = parts
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if declared.is_none_or(|n| n <= state.drain_max) {
                drain(body, 0, state.drain_max).await;
            }
            unauthorized(state.auth.as_ref(), &err)
        }
    }
}

fn unauthorized(auth: &dyn Authenticator, err: &AuthError) -> Response {
    let body = serde_json::json!({
        "error": err.code(),
        "message": err.to_string(),
    });
    let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    if let Ok(v) = HeaderValue::from_str(&auth.challenge(err)) {
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

/// Serve until SIGTERM/SIGINT, then shut down gracefully.
///
/// On a signal the listener stops accepting connections and in-flight
/// requests get up to `shutdown_grace_secs` to finish. The rmcp
/// cancellation token is cancelled *after* that drain: in JSON-response
/// mode cancelling it aborts in-flight tool calls, so cancelling first would
/// turn every running render into an error. Whatever is still running when
/// the grace period ends is cancelled and the function returns.
pub async fn serve(config: HttpConfig, service: Arc<RenderService>) -> anyhow::Result<()> {
    let auth = authenticator(&config)?;
    let mcp_ct = CancellationToken::new();
    let app = router(
        service,
        auth.clone(),
        mcp_server_config(&config, mcp_ct.clone()),
    );

    let listener = tokio::net::TcpListener::bind(config.addr())
        .await
        .with_context(|| format!("binding {}", config.addr()))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    tracing::info!(
        %addr,
        auth = %auth.describe(),
        allowed_hosts = ?config.allowed_hosts,
        allowed_origins = ?config.allowed_origins,
        "listening; MCP endpoint at /mcp, health at /healthz"
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
            axum::serve(listener, app)
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
    Ok(())
}
