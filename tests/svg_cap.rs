//! The inline SVG cap (`PSCAT_MCP_MAX_SVG_BYTES`) end to end: a real rmcp
//! client on the real server binary over stdio, real PSCat in the worker.
//! `chalkboard_math`'s SVG over the cap is in `tests/chalkboard.rs`.

use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Many small marks: a large SVG (each rect becomes its own path).
const MARKS: &str = "1 srand 0 1 2000 { pop rand 500 mod rand 500 mod 3 3 rectfill } for showpage";

/// Two pages of 600 marks each.
const TWO_PAGES: &str = "1 srand \
    0 1 600 { pop rand 500 mod rand 500 mod 3 3 rectfill } for showpage \
    0 1 600 { pop rand 500 mod rand 500 mod 3 3 rectfill } for showpage";

const SHAPES: &str = "1 0 0 setrgbcolor 0 0 100 100 rectfill showpage";

async fn start(env: &[(&str, &str)]) -> Client {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server")).configure(|c| {
        c.arg("serve-stdio")
            .env("PSCAT_MCP_LOG_FORMAT", "pretty")
            .env("RUST_LOG", "warn");
        for (k, v) in &env {
            c.env(k, v);
        }
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn server");
    tokio::time::timeout(CALL_TIMEOUT, ().serve(transport))
        .await
        .expect("initialize timed out")
        .expect("initialize")
}

async fn render(client: &Client, source: &str, format: &str) -> CallToolResult {
    let args = json!({"source": source, "width": 500, "height": 500, "format": format});
    let Value::Object(args) = args else {
        unreachable!()
    };
    tokio::time::timeout(
        CALL_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("render_postscript").with_arguments(args)),
    )
    .await
    .expect("render timed out")
    .expect("render failed at the protocol level")
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_artifact(r: &CallToolResult) -> bool {
    r.content
        .iter()
        .any(|c| matches!(c, ContentBlock::Image(_)) || c.as_resource().is_some())
}

/// The svg size of one render of `source`, with no cap in the way.
async fn svg_bytes(source: &str) -> usize {
    let client = start(&[]).await;
    let r = render(&client, source, "svg").await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let n = r.structured_content.as_ref().unwrap()["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["bytes"].as_u64().unwrap() as usize)
        .sum();
    client.cancel().await.ok();
    n
}

/// Assert an `output_too_large` failure on the SVG cap `limit`; returns the
/// structured content, checked against the tool's output schema.
async fn assert_svg_cap_failure(client: &Client, r: &CallToolResult, limit: usize) -> Value {
    assert_eq!(r.is_error, Some(true), "{}", text_of(r));
    assert!(!has_artifact(r), "no artifact on failure");
    let s = r.structured_content.clone().expect("structured content");
    assert_eq!(s["ok"], false, "{s}");
    assert_eq!(s["error"]["code"], "output_too_large", "{s}");
    assert_eq!(s["error"]["input"]["code"], "svg_bytes", "{s}");
    assert_eq!(s["error"]["input"]["limit_bytes"], limit, "{s}");
    let reached = s["error"]["input"]["reached_bytes"].as_u64().unwrap();
    assert!(reached > limit as u64, "{s}");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(
        m.contains(&format!("{limit} bytes")) && m.contains("PSCAT_MCP_MAX_SVG_BYTES"),
        "{m}"
    );
    // The text content carries the same information.
    let text = text_of(r);
    assert!(
        text.contains("output_too_large")
            && text.contains("svg_bytes")
            && text.contains(&format!("{limit} bytes")),
        "{text}"
    );
    // Every key is declared by the output schema, down to `error.input`.
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    let schema = tool.output_schema.as_ref().unwrap();
    for key in s.as_object().unwrap().keys() {
        assert!(schema["properties"][key].is_object(), "{key} not in schema");
    }
    let input = &schema["$defs"]["InputDiagnostic"]["properties"];
    for key in s["error"]["input"].as_object().unwrap().keys() {
        assert!(input[key].is_object(), "error.input.{key} not in schema");
    }
    s
}

#[tokio::test]
async fn an_svg_under_the_cap_is_returned() {
    let client = start(&[("PSCAT_MCP_MAX_SVG_BYTES", "20000")]).await;
    let r = render(&client, SHAPES, "svg").await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert!(has_artifact(&r));
    assert_eq!(r.structured_content.as_ref().unwrap()["ok"], true);
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_svg_over_the_cap_is_output_too_large_naming_the_budget() {
    let size = svg_bytes(MARKS).await;
    assert!(size > 40_000, "{size}");
    let client = start(&[("PSCAT_MCP_MAX_SVG_BYTES", "20000")]).await;
    let r = render(&client, MARKS, "svg").await;
    let s = assert_svg_cap_failure(&client, &r, 20_000).await;
    assert_eq!(s["error"]["input"]["reached_bytes"], size, "{s}");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("use png or pdf") && m.contains("lower the page size"),
        "{m}"
    );
    // The same program as PNG or PDF is held only to the output cap.
    for format in ["png", "pdf"] {
        let r = render(&client, MARKS, format).await;
        assert_eq!(r.is_error, Some(false), "{format}: {}", text_of(&r));
    }
    client.cancel().await.ok();
}

/// The cap is on all pages together: each page of this program is under it.
#[tokio::test]
async fn the_cap_counts_every_page_together() {
    let client = start(&[]).await;
    let r = render(&client, TWO_PAGES, "svg").await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
    let pages: Vec<usize> = r.structured_content.as_ref().unwrap()["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["bytes"].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    let total: usize = pages.iter().sum();
    let cap = pages.iter().max().unwrap() + 1;
    assert!(cap < total, "{pages:?}");
    let client = start(&[("PSCAT_MCP_MAX_SVG_BYTES", &cap.to_string())]).await;
    let r = render(&client, TWO_PAGES, "svg").await;
    let s = assert_svg_cap_failure(&client, &r, cap).await;
    assert_eq!(s["error"]["input"]["reached_bytes"], total, "{s}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn the_default_is_256_kib_and_the_cap_is_in_the_descriptions_and_renderer_info() {
    let client = start(&[]).await;
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    let limits = &info.structured_content.unwrap()["limits"];
    assert_eq!(limits["max_svg_bytes"], 256 * 1024);
    assert_eq!(limits["max_output_bytes"], 8 * 1024 * 1024);
    let tools = client.list_all_tools().await.unwrap();
    for name in ["render_postscript", "chalkboard_math"] {
        let d = tools.iter().find(|t| t.name == name).unwrap();
        let d = d.description.as_deref().unwrap();
        assert!(d.contains("svg, up to 262144 bytes"), "{name}: {d}");
    }
    client.cancel().await.ok();
    // A small output cap pulls the default down with it.
    let client = start(&[("PSCAT_MCP_MAX_OUTPUT_BYTES", "100000")]).await;
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    assert_eq!(
        info.structured_content.unwrap()["limits"]["max_svg_bytes"],
        100_000
    );
    client.cancel().await.ok();
}

/// A cap above the output cap is refused at startup, not clamped.
#[test]
fn an_svg_cap_above_the_output_cap_is_refused_at_startup() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .arg("serve-stdio")
        .env("PSCAT_MCP_MAX_OUTPUT_BYTES", "1000")
        .env("PSCAT_MCP_MAX_SVG_BYTES", "1001")
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the server");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("max-svg-bytes") && err.contains("max-output-bytes"),
        "{err}"
    );
}
