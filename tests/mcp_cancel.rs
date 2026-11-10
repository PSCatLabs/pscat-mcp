//! MCP-level cancellation (issue #5): `notifications/cancelled` for an
//! in-flight `render_postscript` kills its worker process and frees the
//! render slot. The server runs in-process over a duplex pipe, the same
//! JSON-RPC path as stdio, with a tagged backend so the test can find the
//! worker with `pgrep`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pscat_mcp::backend::SubprocessBackend;
use pscat_mcp::config::LimitsConfig;
use pscat_mcp::mcp::PscatMcp;
use pscat_mcp::render::RenderService;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult};
use rmcp::service::PeerRequestOptions;
use serde_json::{Value, json};

/// Processes whose command line contains `tag`.
fn processes_with(tag: &str) -> Vec<String> {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(tag)
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

async fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn render_params(args: Value) -> CallToolRequestParams {
    let Value::Object(map) = args else {
        unreachable!()
    };
    CallToolRequestParams::new("render_postscript").with_arguments(map)
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_notification_kills_the_worker_and_frees_the_slot() {
    let tag = format!("pscat-mcp-cancel-{}", uuid::Uuid::new_v4());
    let limits = LimitsConfig {
        max_concurrent_renders: Some(1),
        queue_timeout_ms: 500,
        // Only cancellation can stop the loop within the test's time frame.
        render_timeout_ms: 60_000,
        interp_time_limit_ms: Some(0),
        max_steps: 0,
        ..LimitsConfig::default()
    };
    let backend = SubprocessBackend::new(
        env!("CARGO_BIN_EXE_pscat-mcp-server").into(),
        limits.clone(),
    )
    .with_tag(tag.clone());
    let service = Arc::new(RenderService::new(Arc::new(backend), limits));

    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    let server = PscatMcp::new(service.clone());
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client initialize");

    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(render_params(
                json!({"source": "{ } loop", "width": 20, "height": 20}),
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("send");

    wait_for("worker started", Duration::from_secs(10), || {
        !processes_with(&tag).is_empty()
    })
    .await;
    assert_eq!(service.counters.executed.load(Ordering::SeqCst), 1);

    handle
        .cancel(Some("test".into()))
        .await
        .expect("send notifications/cancelled");

    wait_for("worker killed", Duration::from_secs(5), || {
        processes_with(&tag).is_empty()
    })
    .await;

    // The only slot is free again: a normal render succeeds within the
    // 500 ms queue timeout instead of returning server_busy.
    let result = client
        .peer()
        .send_request(ClientRequest::CallToolRequest(CallToolRequest::new(
            render_params(json!({"source": "showpage", "width": 20, "height": 20})),
        )))
        .await
        .expect("second render");
    let ServerResult::CallToolResult(result) = result else {
        panic!("unexpected response {result:?}")
    };
    assert_eq!(result.is_error, Some(false), "{result:?}");
    assert_eq!(service.counters.executed.load(Ordering::SeqCst), 2);
    assert!(processes_with(&tag).is_empty());
    client.cancel().await.ok();
}
