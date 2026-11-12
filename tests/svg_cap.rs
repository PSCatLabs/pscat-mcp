//! How SVG is returned (`PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES`) end to end: a real rmcp
//! client on the real server binary over stdio, real PSCat in the worker.
//! Up to the limit (all pages together) each page is an embedded text
//! resource; above it, an embedded blob resource (base64), up to the output
//! cap. `chalkboard_math`'s SVG is in `tests/chalkboard.rs`, the async path
//! in `tests/jobs.rs`.

use std::time::Duration;

use base64::Engine as _;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents};
use rmcp::service::RunningService;
use serde_json::{Value, json};

mod common;

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
    common::start_stdio(env!("CARGO_BIN_EXE_pscat-mcp-server"), env).await
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

/// One SVG page as it came back: its markup, and whether it was a blob.
#[derive(Debug)]
struct Page {
    svg: String,
    blob: bool,
}

/// Every SVG resource of a successful result, in order, checked for its
/// MIME type, and the `returned_as` of every artifact summary.
fn pages(r: &CallToolResult) -> (Vec<Page>, Vec<String>) {
    assert_eq!(r.is_error, Some(false), "{}", text_of(r));
    let pages = r
        .content
        .iter()
        .filter_map(|c| c.as_resource().map(|e| &e.resource))
        .map(|res| match res {
            ResourceContents::TextResourceContents {
                text, mime_type, ..
            } => {
                assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
                Page {
                    svg: text.clone(),
                    blob: false,
                }
            }
            ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            } => {
                assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(blob)
                    .expect("base64");
                Page {
                    svg: String::from_utf8(bytes).expect("utf-8 svg"),
                    blob: true,
                }
            }
            other => panic!("unexpected resource {other:?}"),
        })
        .collect::<Vec<_>>();
    for p in &pages {
        assert!(p.svg.contains("<svg"), "{:.80}", p.svg);
    }
    let s = r.structured_content.as_ref().expect("structured content");
    let returned_as = s["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["returned_as"].as_str().unwrap().to_string())
        .collect();
    (pages, returned_as)
}

/// Every key of the structured content, and of each artifact summary, is
/// declared by the tool's output schema.
async fn assert_in_schema(client: &Client, s: &Value) {
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    let schema = tool.output_schema.as_ref().unwrap();
    for key in s.as_object().unwrap().keys() {
        assert!(schema["properties"][key].is_object(), "{key} not in schema");
    }
    let artifact = &schema["$defs"]["ArtifactSummary"]["properties"];
    for a in s["artifacts"].as_array().into_iter().flatten() {
        for key in a.as_object().unwrap().keys() {
            assert!(artifact[key].is_object(), "artifacts.{key} not in schema");
        }
    }
    let input = &schema["$defs"]["InputDiagnostic"]["properties"];
    for key in s["error"]["input"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|m| m.0)
    {
        assert!(input[key].is_object(), "error.input.{key} not in schema");
    }
}

#[tokio::test]
async fn an_svg_up_to_the_limit_is_a_text_resource() {
    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "20000")]).await;
    let r = render(&client, SHAPES, "svg").await;
    let (pages, returned_as) = pages(&r);
    assert_eq!(pages.len(), 1);
    assert!(!pages[0].blob, "a small SVG stays text");
    assert_eq!(returned_as, ["text"]);
    assert!(!text_of(&r).contains("blob"), "{}", text_of(&r));
    assert_in_schema(&client, r.structured_content.as_ref().unwrap()).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_svg_over_the_limit_is_a_blob_resource_with_the_same_markup() {
    // The same program with the limit out of the way: text.
    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "8388608")]).await;
    let r = render(&client, MARKS, "svg").await;
    let (text_pages, returned_as) = pages(&r);
    assert_eq!(returned_as, ["text"]);
    let size = text_pages[0].svg.len();
    assert!(size > 40_000, "{size}");
    client.cancel().await.ok();

    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "20000")]).await;
    let r = render(&client, MARKS, "svg").await;
    let (blob_pages, returned_as) = pages(&r);
    assert_eq!(blob_pages.len(), 1);
    assert!(blob_pages[0].blob, "over the limit: a blob");
    assert_eq!(returned_as, ["blob"]);
    // Byte for byte the SVG the text form carries.
    assert_eq!(blob_pages[0].svg, text_pages[0].svg);
    let s = r.structured_content.clone().unwrap();
    assert_eq!(s["ok"], true, "{s}");
    assert_eq!(s["mime_type"], "image/svg+xml", "{s}");
    assert_eq!(s["artifacts"][0]["bytes"], size, "{s}");
    assert_eq!(s["artifacts"][0]["mime_type"], "image/svg+xml", "{s}");
    // The text summary says why.
    let text = text_of(&r);
    assert!(
        text.contains("over 20000 bytes")
            && text.contains("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES")
            && text.contains("blob"),
        "{text}"
    );
    assert_in_schema(&client, &s).await;
    client.cancel().await.ok();
}

/// The limit is on all pages together, and one result never mixes text and
/// blob pages: each page of this program is under the limit, the two
/// together are over it.
#[tokio::test]
async fn the_limit_counts_every_page_together() {
    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "8388608")]).await;
    let r = render(&client, TWO_PAGES, "svg").await;
    let (text_pages, returned_as) = pages(&r);
    assert_eq!(returned_as, ["text", "text"]);
    client.cancel().await.ok();
    let sizes: Vec<usize> = text_pages.iter().map(|p| p.svg.len()).collect();
    let total: usize = sizes.iter().sum();
    let limit = sizes.iter().max().unwrap() + 1;
    assert!(limit < total, "{sizes:?}");
    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", &limit.to_string())]).await;
    let r = render(&client, TWO_PAGES, "svg").await;
    let (blob_pages, returned_as) = pages(&r);
    assert_eq!(returned_as, ["blob", "blob"]);
    assert!(blob_pages.iter().all(|p| p.blob));
    for (b, t) in blob_pages.iter().zip(&text_pages) {
        assert_eq!(b.svg, t.svg);
    }
    // Exactly at the limit is still text.
    let client2 = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", &total.to_string())]).await;
    let r = render(&client2, TWO_PAGES, "svg").await;
    assert_eq!(pages(&r).1, ["text", "text"]);
    client.cancel().await.ok();
    client2.cancel().await.ok();
}

/// 0 returns every SVG as a blob; PNG and PDF are unaffected.
#[tokio::test]
async fn a_limit_of_zero_returns_every_svg_as_a_blob() {
    let client = start(&[("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "0")]).await;
    let r = render(&client, SHAPES, "svg").await;
    assert_eq!(pages(&r).1, ["blob"]);
    let png = render(&client, SHAPES, "png").await;
    assert_eq!(
        png.structured_content.as_ref().unwrap()["artifacts"][0]["returned_as"],
        "image"
    );
    let pdf = render(&client, SHAPES, "pdf").await;
    assert_eq!(
        pdf.structured_content.as_ref().unwrap()["artifacts"][0]["returned_as"],
        "blob"
    );
    client.cancel().await.ok();
}

/// A blob is held to the output cap: over it, `output_too_large` with
/// `error.input.code` `output_bytes` and the SVG's whole size, before any
/// page is returned.
#[tokio::test]
async fn an_svg_over_the_output_cap_is_output_too_large() {
    let client = start(&[
        ("PSCAT_MCP_MAX_OUTPUT_BYTES", "30000"),
        ("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "10000"),
    ])
    .await;
    let r = render(&client, MARKS, "svg").await;
    assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
    assert!(!has_artifact(&r), "no artifact on failure");
    let s = r.structured_content.clone().expect("structured content");
    assert_eq!(s["ok"], false, "{s}");
    assert_eq!(s["error"]["code"], "output_too_large", "{s}");
    assert_eq!(s["error"]["input"]["code"], "output_bytes", "{s}");
    assert_eq!(s["error"]["input"]["limit_bytes"], 30000, "{s}");
    assert!(
        s["error"]["input"]["reached_bytes"].as_u64().unwrap() > 40_000,
        "{s}"
    );
    let m = s["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("PSCAT_MCP_MAX_OUTPUT_BYTES") && m.contains("use png or pdf"),
        "{m}"
    );
    let text = text_of(&r);
    assert!(
        text.contains("output_too_large") && text.contains("output_bytes"),
        "{text}"
    );
    assert_in_schema(&client, &s).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn the_default_is_40_kib_and_the_limit_is_in_the_descriptions_and_renderer_info() {
    let client = start(&[]).await;
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    let info = info.structured_content.unwrap();
    let limits = &info["limits"];
    assert_eq!(limits["svg_blob_threshold_bytes"], 40 * 1024);
    assert_eq!(limits["max_output_bytes"], 8 * 1024 * 1024);
    let svg = info["formats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "svg")
        .unwrap();
    let how = svg["returned_as"].as_str().unwrap();
    assert!(
        how.contains("text resource") && how.contains("blob resource"),
        "{how}"
    );
    let tools = client.list_all_tools().await.unwrap();
    for name in ["render_postscript", "chalkboard_math"] {
        let d = tools.iter().find(|t| t.name == name).unwrap();
        let d = d.description.as_deref().unwrap();
        assert!(
            d.contains("svg up to 40960 bytes") && d.contains("blob resource"),
            "{name}: {d}"
        );
        assert!(!d.contains("over it the call fails"), "{name}: {d}");
    }
    assert!(limits.get("max_svg_bytes").is_none(), "{limits}");
    client.cancel().await.ok();
    // A small output cap pulls the default down with it.
    let client = start(&[("PSCAT_MCP_MAX_OUTPUT_BYTES", "10000")]).await;
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    assert_eq!(
        info.structured_content.unwrap()["limits"]["svg_blob_threshold_bytes"],
        10_000
    );
    client.cancel().await.ok();
}

/// A limit above the output cap is refused at startup, not clamped.
#[test]
fn an_svg_limit_above_the_output_cap_is_refused_at_startup() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .arg("serve-stdio")
        .env("PSCAT_MCP_MAX_OUTPUT_BYTES", "1000")
        .env("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "1001")
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the server");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("svg-blob-threshold-bytes") && err.contains("max-output-bytes"),
        "{err}"
    );
}

/// The deprecated name still sets the limit, and `get_renderer_info` reports
/// it under the new field name.
#[tokio::test]
async fn the_deprecated_env_name_still_sets_the_limit() {
    let client = start(&[("PSCAT_MCP_MAX_SVG_BYTES", "20000")]).await;
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    let limits = info.structured_content.unwrap()["limits"].clone();
    assert_eq!(limits["svg_blob_threshold_bytes"], 20_000, "{limits}");
    assert!(limits.get("max_svg_bytes").is_none(), "{limits}");
    client.cancel().await.ok();
}

/// The deprecated name is logged as deprecated at startup (on stderr; stdout
/// carries the protocol). The server exits by itself on EOF on stdin.
#[test]
fn the_deprecated_env_name_logs_a_warning_at_startup() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .arg("serve-stdio")
        .env("PSCAT_MCP_MAX_SVG_BYTES", "20000")
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the server");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PSCAT_MCP_MAX_SVG_BYTES")
            && err.contains("deprecated")
            && err.contains("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES"),
        "{err}"
    );
}

/// Both names together are refused at startup, and the error names both.
#[test]
fn both_svg_names_together_are_refused_at_startup() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .arg("serve-stdio")
        .env("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES", "20000")
        .env("PSCAT_MCP_MAX_SVG_BYTES", "20000")
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the server");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PSCAT_MCP_SVG_BLOB_THRESHOLD_BYTES")
            && err.contains("PSCAT_MCP_MAX_SVG_BYTES")
            && err.contains("not both"),
        "{err}"
    );
}
