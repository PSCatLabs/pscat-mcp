//! `chalkboard_math` end to end: a real rmcp client on the real server
//! binary over stdio, real PSCat in the worker.

use std::time::Duration;

use base64::Engine as _;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents};
use rmcp::service::RunningService;
use serde_json::{Value, json};

mod common;

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// A client on a server started with the extra `PSCAT_MCP_*` variables.
async fn start(env: &[(&str, &str)]) -> Client {
    common::start_stdio(env!("CARGO_BIN_EXE_pscat-mcp-server"), env).await
}

async fn call(client: &Client, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    let params = CallToolRequestParams::new("chalkboard_math").with_arguments(args);
    tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .expect("chalkboard_math timed out")
        .expect("chalkboard_math failed at the protocol level")
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured content")
}

fn error_code(r: &CallToolResult) -> &str {
    structured(r)["error"]["code"].as_str().expect("error code")
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

/// Assert a failure with `code`, the envelope and no image; returns the
/// structured content.
fn assert_fails(r: &CallToolResult, code: &str) -> Value {
    assert_eq!(r.is_error, Some(true), "{}", text_of(r));
    assert!(!has_artifact(r), "no artifact on failure");
    let s = structured(r).clone();
    assert_eq!(s["ok"], false, "{s}");
    assert_eq!(error_code(r), code, "{s}");
    assert_eq!(s["renderer"]["engine"], "pscat");
    assert!(text_of(r).contains(code), "{}", text_of(r));
    s
}

/// Width, height and RGBA pixels of the first PNG image block.
fn png_of(r: &CallToolResult) -> (u32, u32, Vec<[u8; 4]>) {
    let image = r
        .content
        .iter()
        .find_map(|c| c.as_image())
        .expect("an image block");
    assert_eq!(image.mime_type, "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .unwrap();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let ch = info.color_type.samples();
    let px = buf[..info.buffer_size()]
        .chunks_exact(ch)
        .map(|c| match ch {
            4 => [c[0], c[1], c[2], c[3]],
            3 => [c[0], c[1], c[2], 255],
            2 => [c[0], c[0], c[0], c[1]],
            _ => [c[0], c[0], c[0], 255],
        })
        .collect();
    (info.width, info.height, px)
}

fn luma(p: [u8; 4]) -> u32 {
    (u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114) / 1000
}

#[tokio::test]
async fn the_tool_is_listed_with_the_plugins_arguments() {
    let client = start(&[]).await;
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools
        .iter()
        .find(|t| t.name == "chalkboard_math")
        .expect("chalkboard_math listed");
    let props = t.input_schema["properties"].as_object().unwrap();
    let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "align",
            "board",
            "color",
            "config",
            "dpi",
            "format",
            "ghosting",
            "height",
            "irregular",
            "jitter",
            "latex",
            "seed",
            "size",
            "slope",
            "width"
        ]
    );
    assert_eq!(t.input_schema["required"], json!(["latex"]));
    assert_eq!(
        props["color"]["enum"],
        json!(pscat_mcp::pscat_extras::CHALK_COLORS)
    );
    assert_eq!(props["board"]["anyOf"].as_array().unwrap().len(), 4);
    assert_eq!(props["ghosting"]["anyOf"].as_array().unwrap().len(), 2);
    let d = t.description.as_deref().unwrap();
    assert!(d.contains("latex up to 4096 bytes"), "{d}");
    assert!(!d.contains("out_path"), "{d}");
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    assert_eq!(
        info.structured_content.unwrap()["limits"]["max_latex_bytes"],
        4096
    );
    client.cancel().await.ok();
}

/// The default board: 1600x800 pixels, a dark slate, and chalk marks much
/// brighter than it.
#[tokio::test]
async fn writes_a_formula_on_the_default_board() {
    let client = start(&[]).await;
    let r = call(&client, json!({"latex": r"e^{i\pi} + 1 = 0"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["format"], "png");
    assert_eq!(s["page_count"], 1);
    assert_eq!(s["artifacts"][0]["width_px"], 1600);
    assert_eq!(s["artifacts"][0]["height_px"], 800);
    // The board is 800x400 points at the default 144 dpi.
    assert_eq!(s["artifacts"][0]["width_pt"].as_f64(), Some(800.0));
    assert_eq!(s["artifacts"][0]["height_pt"].as_f64(), Some(400.0));
    let (w, h, px) = png_of(&r);
    assert_eq!((w, h), (1600, 800));
    // The slate is dark and opaque at the edges, where nothing is written.
    let edge = px[(10 * w + 10) as usize];
    assert_eq!(edge[3], 255, "{edge:?}");
    assert!(luma(edge) < 90, "board colour {edge:?}");
    // Chalk: thousands of pixels far brighter than the board.
    // The slate (with its haze and ghosts) stays under a luma of about
    // 110; the chalk deposits reach 190.
    let bright = px.iter().filter(|p| luma(**p) >= 128).count();
    assert!(bright > 2000, "only {bright} chalk pixels");
    assert!(bright < px.len() / 4, "{bright} bright pixels: no board?");

    // The same arguments draw the same image.
    let again = call(&client, json!({"latex": r"e^{i\pi} + 1 = 0"})).await;
    assert_eq!(png_of(&again).2, px);
    client.cancel().await.ok();
}

/// The result states the board in points and in pixels (#78): a 400x200 pt
/// board at the default 144 dpi is 800x400 px, in the envelope and in the
/// text, so it is not read as "width and height were ignored".
#[tokio::test]
async fn reports_the_board_in_points_and_pixels() {
    let client = start(&[]).await;
    let r = call(&client, json!({"latex": "x", "width": 400, "height": 200})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let a = &structured(&r)["artifacts"][0];
    assert_eq!(a["width_pt"].as_f64(), Some(400.0), "{a}");
    assert_eq!(a["height_pt"].as_f64(), Some(200.0), "{a}");
    assert_eq!(a["width_px"], 800);
    assert_eq!(a["height_px"], 400);
    // The default dpi is 144, reported with the sizes.
    assert_eq!(a["dpi"].as_f64(), Some(144.0), "{a}");
    assert!(
        text_of(&r).contains("400x200 pt at 144 dpi = 800x400 px"),
        "{}",
        text_of(&r)
    );

    // Another dpi changes the pixels, never the points.
    let r = call(
        &client,
        json!({"latex": "x", "width": 400, "height": 200, "dpi": 72, "format": "pdf"}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let a = &structured(&r)["artifacts"][0];
    assert_eq!(a["width_pt"].as_f64(), Some(400.0), "{a}");
    assert_eq!(a["height_pt"].as_f64(), Some(200.0), "{a}");
    assert_eq!(a["width_px"], 400);
    client.cancel().await.ok();
}

#[tokio::test]
async fn board_false_is_chalk_on_a_transparent_page() {
    let client = start(&[]).await;
    let r = call(
        &client,
        json!({"latex": "a^2 + b^2 = c^2", "board": false, "width": 400, "height": 200, "dpi": 72}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let (w, h, px) = png_of(&r);
    assert_eq!((w, h), (400, 200));
    assert_eq!(px[0][3], 0, "corner is transparent: {:?}", px[0]);
    let clear = px.iter().filter(|p| p[3] == 0).count();
    let inked = px.iter().filter(|p| p[3] > 0).count();
    assert!(clear > px.len() / 2, "{clear} transparent pixels");
    assert!(inked > 100, "{inked} chalk pixels");

    // A colour board is opaque in that colour.
    let r = call(
        &client,
        json!({"latex": "x", "board": [0.0, 0.0, 1.0], "ghosting": false, "width": 400, "height": 200, "dpi": 72}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let (_, _, px) = png_of(&r);
    let corner = px[0];
    assert_eq!(corner[3], 255);
    assert!(corner[2] > corner[0] + 60, "blue board: {corner:?}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn svg_and_pdf_come_back_inline() {
    // A board's SVG is large, so the text limit is raised to the output
    // cap to get it as text.
    let client = start(&[("PSCAT_MCP_MAX_SVG_BYTES", "8388608")]).await;
    let small = |format: &str| json!({"latex": r"\frac{1}{2}", "format": format, "width": 300, "height": 150, "dpi": 72});
    let svg = call(&client, small("svg")).await;
    assert_eq!(svg.is_error, Some(false), "{}", text_of(&svg));
    assert_eq!(structured(&svg)["mime_type"], "image/svg+xml");
    assert_eq!(structured(&svg)["artifacts"][0]["returned_as"], "text");
    eprintln!(
        "small board svg: {}",
        structured(&svg)["artifacts"][0]["bytes"]
    );
    let res: Vec<_> = svg
        .content
        .iter()
        .filter_map(|c| c.as_resource().map(|e| &e.resource))
        .collect();
    assert_eq!(res.len(), 1);
    match res[0] {
        ResourceContents::TextResourceContents {
            text, mime_type, ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
            assert!(text.contains("<svg"), "{:.80}", text);
        }
        other => panic!("expected a text resource, got {other:?}"),
    }

    let pdf = call(&client, small("pdf")).await;
    assert_eq!(pdf.is_error, Some(false), "{}", text_of(&pdf));
    let blob = pdf
        .content
        .iter()
        .find_map(|c| match c.as_resource().map(|e| &e.resource) {
            Some(ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            }) => {
                assert_eq!(mime_type.as_deref(), Some("application/pdf"));
                Some(blob.clone())
            }
            _ => None,
        })
        .expect("a PDF blob");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    // The title comes from the generated program's DSC header, as with
    // `pscat chalk --pdf`.
    assert!(
        bytes.windows(10).any(|w| w == b"Chalkboard"),
        "PDF carries the board's title"
    );
    client.cancel().await.ok();
}

/// A board's SVG keeps every chalk grain as a vector mark, far over the
/// default text limit, so it comes back as a blob resource holding the same
/// SVG; over the output cap it is `output_too_large` with advice for a
/// board (png), not "use svg".
#[tokio::test]
async fn a_board_svg_over_the_text_limit_is_a_blob() {
    let client = start(&[]).await;
    let small =
        json!({"latex": r"\frac{1}{2}", "format": "svg", "width": 300, "height": 150, "dpi": 72});
    let r = call(&client, small.clone()).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_eq!(s["mime_type"], "image/svg+xml", "{s}");
    assert_eq!(s["artifacts"][0]["returned_as"], "blob", "{s}");
    let bytes = s["artifacts"][0]["bytes"].as_u64().unwrap();
    assert!(bytes > 40 * 1024, "{s}");
    let blob = r
        .content
        .iter()
        .find_map(|c| match c.as_resource().map(|e| &e.resource) {
            Some(ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            }) => {
                assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
                Some(blob.clone())
            }
            _ => None,
        })
        .expect("an SVG blob");
    let svg = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .unwrap();
    assert_eq!(svg.len() as u64, bytes);
    assert!(String::from_utf8(svg).unwrap().contains("<svg"));
    client.cancel().await.ok();

    // Over the output cap.
    let client = start(&[("PSCAT_MCP_MAX_OUTPUT_BYTES", "30000")]).await;
    let r = call(&client, small).await;
    let s = assert_fails(&r, "output_too_large");
    assert_eq!(s["error"]["input"]["code"], "output_bytes", "{s}");
    assert_eq!(s["error"]["input"]["limit_bytes"], 30000, "{s}");
    assert!(s["error"]["input"]["reached_bytes"].as_u64().unwrap() > 30000);
    let m = s["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("runs to megabytes") && m.contains("png is the practical format"),
        "{m}"
    );
    assert!(!m.contains("use svg"), "{m}");
    // The same board as PNG is fine.
    let r = call(
        &client,
        json!({"latex": r"\frac{1}{2}", "width": 300, "height": 150, "dpi": 72}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

#[tokio::test]
async fn latex_over_the_limit_is_refused_before_a_worker_starts() {
    let client = start(&[("PSCAT_MCP_MAX_LATEX_BYTES", "64")]).await;
    let r = call(&client, json!({"latex": "x+".repeat(40) + "x"})).await;
    let s = assert_fails(&r, "source_too_large");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("81 bytes") && m.contains("64 bytes"), "{m}");
    let r = call(&client, json!({"latex": "  "})).await;
    assert_fails(&r, "invalid_request");
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_unsupported_command_is_a_postscript_error_naming_it() {
    let client = start(&[]).await;
    let r = call(&client, json!({"latex": r"\nosuchcommand{x} + 1"})).await;
    let s = assert_fails(&r, "postscript_error");
    assert_eq!(s["error"]["input"]["code"], "unsupported_command", "{s}");
    assert_eq!(s["error"]["input"]["token"], r"\nosuchcommand", "{s}");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains(r"\nosuchcommand"), "{m}");
    assert!(!m.contains("mk-"), "no mathkit internals: {m}");
    assert!(s["error"].get("postscript").is_none(), "{s}");

    let text = text_of(&r);
    assert!(
        text.contains(r"Input error: unsupported_command (\nosuchcommand)"),
        "{text}"
    );

    // A malformed display is caught while the rows are split.
    let r = call(&client, json!({"latex": r"\(x + 1"})).await;
    let s = assert_fails(&r, "postscript_error");
    assert_eq!(s["error"]["input"]["code"], "unclosed_math_delimiter");
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_bad_board_config_is_invalid_request() {
    let client = start(&[]).await;
    let cases = [
        (
            json!({"latex": "x", "color": "red"}),
            "color must be one of",
        ),
        (json!({"latex": "x", "size": 0}), "size must be"),
        (json!({"latex": "x", "board": "chalky"}), "board must be"),
        (
            json!({"latex": "x", "board": false, "ghosting": 0.5}),
            "ghosting needs a board",
        ),
        (
            json!({"latex": "x", "config": {"layout": {"margin": -1}}}),
            "layout.margin",
        ),
        (
            json!({"latex": "x", "config": {"colour": "red"}}),
            "unknown key colour",
        ),
        (
            json!({"latex": "x", "width": 9000}),
            "width must be a whole number from 1 to 8000",
        ),
        (
            json!({"latex": "x", "width": 60, "height": 60}),
            "no room to write",
        ),
        (json!({"latex": "% just a comment"}), "no formula"),
    ];
    for (args, needle) in cases {
        let r = call(&client, args.clone()).await;
        let s = assert_fails(&r, "invalid_request");
        let m = s["error"]["message"].as_str().unwrap();
        assert!(m.contains(needle), "{args}: {m}");
    }
    // PSCat quotes a bad value back; a huge one does not make a huge reply.
    let r = call(
        &client,
        json!({"latex": "x", "config": {"board": {"tone": "t".repeat(1_000_000)}}}),
    )
    .await;
    let s = assert_fails(&r, "invalid_request");
    assert!(s.to_string().len() < 2000, "{} bytes", s.to_string().len());
    assert!(text_of(&r).len() < 2000);
    // Over the pixel limit, and an unknown format, use the render codes.
    let r = call(
        &client,
        json!({"latex": "x", "width": 4000, "height": 4000, "dpi": 144}),
    )
    .await;
    assert_fails(&r, "too_many_pixels");
    let r = call(&client, json!({"latex": "x", "format": "jpeg"})).await;
    assert_fails(&r, "unsupported_format");
    client.cancel().await.ok();
}

#[tokio::test]
async fn argument_type_errors_get_the_envelope() {
    let client = start(&[]).await;
    for args in [
        json!({"latex": 5}),
        json!({"size": 10}),
        json!({"latex": "x", "size": "big"}),
        json!({"latex": "x", "config": 3}),
        json!({"latex": "x", "seed": 1.5}),
        json!({"latex": "x", "width": -1}),
    ] {
        let r = call(&client, args.clone()).await;
        let s = assert_fails(&r, "invalid_request");
        let m = s["error"]["message"].as_str().unwrap();
        assert!(m.starts_with("invalid arguments:"), "{args}: {m}");
        assert!(
            text_of(&r).starts_with("Chalkboard failed"),
            "{}",
            text_of(&r)
        );
    }
    client.cancel().await.ok();
}

/// Pathological LaTeX is parsed only in the worker: it ends in a structured
/// error or the deadline, and the server goes on serving (regression test
/// for running the generator out of the server process).
#[tokio::test]
async fn pathological_latex_ends_in_an_error_and_the_server_keeps_serving() {
    let client = start(&[
        ("PSCAT_MCP_MAX_LATEX_BYTES", "400000"),
        ("PSCAT_MCP_RENDER_TIMEOUT_MS", "8000"),
    ])
    .await;
    let depth = 10_000;
    let fracs = format!("{}1{}", r"\frac{".repeat(depth), "}{2}".repeat(depth));
    let braces = format!("{}x{}", "{".repeat(50_000), "}".repeat(50_000));
    let underbraces = r"\underbrace{x}_{a}".repeat(5_000);
    for latex in [fracs, braces, underbraces] {
        let r = call(&client, json!({"latex": latex})).await;
        let head: String = latex.chars().take(24).collect();
        if r.is_error == Some(true) {
            let code = error_code(&r).to_string();
            assert!(
                [
                    "postscript_error",
                    "limit_exceeded",
                    "timeout",
                    "worker_failed"
                ]
                .contains(&code.as_str()),
                "{head}...: {code}: {}",
                text_of(&r)
            );
            assert!(!has_artifact(&r));
        } else {
            // A board it could draw (shrunk to fit) is fine too.
            assert_eq!(structured(&r)["ok"], true);
        }
        let ok = call(
            &client,
            json!({"latex": "x = 1", "width": 300, "height": 150}),
        )
        .await;
        assert_eq!(
            ok.is_error,
            Some(false),
            "after {head}...: {}",
            text_of(&ok)
        );
    }
    client.cancel().await.ok();
}
