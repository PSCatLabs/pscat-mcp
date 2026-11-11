//! Capture mode: a debug aid that records what HTTP clients send.
//!
//! Off by default. With `--capture-file PATH` (`PSCAT_MCP_CAPTURE_FILE`) the
//! server appends one JSON line per `/mcp` request to `PATH`, so a real
//! client's traffic can be turned into a replay fixture (`evals/server/`).
//!
//! What a record holds:
//!
//! * the request line (method, path, HTTP version; **never the query
//!   string**, only whether there was one);
//! * the values of an allowlist of protocol headers (`Accept`,
//!   `Content-Type`, `MCP-Protocol-Version`, `Mcp-Session-Id`, `Mcp-Method`,
//!   `Mcp-Name`, `User-Agent`, `Host`) and only the **names** of every other
//!   header;
//! * for `Authorization`, only the scheme (`Bearer`), never the credential;
//! * the request body, cut at [`MAX_BODY_BYTES`];
//! * the response status and content type (not the response body).
//!
//! The request body is the one place a secret could still appear (a client
//! that put a token in a tool argument), and PostScript sources are
//! recorded as sent: capture mode is for a throwaway local server, not for
//! production. The file is created with mode 0600.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use clap::Args;
use http::header;
use http_body_util::BodyExt as _;
use serde_json::{Map, Value, json};

/// Most bytes of a request body written to the capture file.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Headers whose values are recorded, lowercase. Everything else is recorded
/// by name only, and `Authorization` as its scheme.
pub const RECORDED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "mcp-protocol-version",
    "mcp-session-id",
    "mcp-method",
    "mcp-name",
    "user-agent",
    "host",
];

/// Capture settings (`serve-http` only).
#[derive(Debug, Clone, Args, Default)]
pub struct CaptureConfig {
    /// Debug: append every `/mcp` request (headers allowlisted, no tokens,
    /// no query string, body cut at 64 KiB) as a JSON line to this file.
    /// Off by default. Records PostScript sources as sent: never use it in
    /// production.
    #[arg(long, env = "PSCAT_MCP_CAPTURE_FILE")]
    pub capture_file: Option<PathBuf>,
}

/// An open capture file.
#[derive(Debug)]
pub struct Capture {
    file: Mutex<File>,
    seq: AtomicU64,
    started: std::time::Instant,
}

impl Capture {
    /// Create (or append to) `path`, mode 0600 on Unix.
    pub fn open(path: &Path) -> std::io::Result<Capture> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        Ok(Capture {
            file: Mutex::new(options.open(path)?),
            seq: AtomicU64::new(0),
            started: std::time::Instant::now(),
        })
    }

    fn write(&self, record: &Value) {
        let mut line = record.to_string();
        line.push('\n');
        // A poisoned lock only means another record's writer panicked.
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = file.write_all(line.as_bytes()).and_then(|_| file.flush()) {
            tracing::warn!(error = %e, "writing the capture file failed");
        }
    }
}

/// The request-head part of a record. Pure, so the scrubbing is testable.
pub fn head_record(
    method: &str,
    uri: &http::Uri,
    version: http::Version,
    headers: &http::HeaderMap,
) -> Value {
    let mut recorded = Map::new();
    let mut other = Vec::new();
    let mut authorization = Value::Null;
    for name in headers.keys() {
        let lname = name.as_str().to_ascii_lowercase();
        if lname == "authorization" {
            let scheme = headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split_whitespace().next())
                .unwrap_or("");
            // Only a known scheme word is kept; anything else could be the
            // credential itself.
            let scheme = if scheme.eq_ignore_ascii_case("bearer") {
                "Bearer"
            } else if scheme.eq_ignore_ascii_case("basic") {
                "Basic"
            } else {
                "other"
            };
            authorization = json!({ "scheme": scheme });
        } else if RECORDED_HEADERS.contains(&lname.as_str()) {
            let values: Vec<String> = headers
                .get_all(name)
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect();
            recorded.insert(lname, Value::String(values.join(", ")));
        } else {
            other.push(lname);
        }
    }
    other.sort();
    json!({
        "method": method,
        "path": uri.path(),
        "query": uri.query().is_some(),
        "http_version": format!("{version:?}"),
        "headers": recorded,
        "other_header_names": other,
        "authorization": authorization,
    })
}

/// Axum middleware recording each request that passes through it. Place it
/// after authentication and the body limit, directly in front of rmcp.
pub async fn capture_layer(
    State(capture): State<Option<std::sync::Arc<Capture>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(capture) = capture else {
        return next.run(req).await;
    };
    let seq = capture.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let t_ms = capture.started.elapsed().as_millis() as u64;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let (parts, body) = req.into_parts();
    let mut record = head_record(
        parts.method.as_str(),
        &parts.uri,
        parts.version,
        &parts.headers,
    );

    // The body was buffered by the body-limit layer, so this costs nothing.
    let bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => Default::default(),
    };
    let cut = bytes.len().min(MAX_BODY_BYTES);
    // Cut on a character boundary where possible.
    let text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    let obj = record.as_object_mut().expect("object");
    obj.insert("seq".into(), json!(seq));
    obj.insert("unix_ms".into(), json!(unix_ms));
    obj.insert("t_ms".into(), json!(t_ms));
    obj.insert("body".into(), Value::String(text));
    obj.insert("body_bytes".into(), json!(bytes.len()));
    obj.insert("body_truncated".into(), json!(bytes.len() > cut));

    let resp = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    obj.insert(
        "response".into(),
        json!({
            "status": resp.status().as_u16(),
            "content_type": resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        }),
    );
    capture.write(&record);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_record_keeps_the_allowlist_and_never_the_credential() {
        let mut h = http::HeaderMap::new();
        h.insert(
            "authorization",
            "Bearer s3cret-token-value".parse().unwrap(),
        );
        h.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        h.insert("x-api-key", "another-secret".parse().unwrap());
        h.insert("cookie", "sid=abc".parse().unwrap());
        h.insert("user-agent", "claude-code/9.9".parse().unwrap());
        let uri: http::Uri = "/mcp?access_token=in-the-query".parse().unwrap();
        let rec = head_record("POST", &uri, http::Version::HTTP_11, &h);
        let text = rec.to_string();
        for secret in [
            "s3cret-token-value",
            "another-secret",
            "sid=abc",
            "in-the-query",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        assert_eq!(rec["authorization"]["scheme"], "Bearer");
        assert_eq!(rec["query"], true);
        assert_eq!(rec["path"], "/mcp");
        assert_eq!(
            rec["headers"]["accept"],
            "application/json, text/event-stream"
        );
        assert_eq!(rec["headers"]["user-agent"], "claude-code/9.9");
        assert_eq!(rec["other_header_names"], json!(["cookie", "x-api-key"]));
    }

    #[test]
    fn an_unknown_authorization_scheme_is_not_echoed() {
        let mut h = http::HeaderMap::new();
        h.insert("authorization", "TopSecretScheme abc".parse().unwrap());
        let uri: http::Uri = "/mcp".parse().unwrap();
        let rec = head_record("POST", &uri, http::Version::HTTP_11, &h);
        assert_eq!(rec["authorization"]["scheme"], "other");
        assert!(!rec.to_string().contains("TopSecretScheme"));
    }
}
