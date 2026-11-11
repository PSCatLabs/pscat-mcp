//! End-to-end MCP tests over stdio: a real rmcp client spawns the real
//! server binary (`serve-stdio`) and calls its tools.

use std::time::Duration;

use base64::Engine as _;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents, Tool};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Two halves (red | blue) with a green square in the bottom-left corner of
/// a 200x100 page.
const SHAPES: &str = "\
1 0 0 setrgbcolor 0 0 100 100 rectfill
0 0 1 setrgbcolor 100 0 100 100 rectfill
0 1 0 setrgbcolor 10 10 30 30 rectfill
showpage
";

/// Every tool the server lists, with its title. Adding a tool means adding
/// one row here (and its own behavioural test); the listing test checks the
/// annotations, descriptions and output schema of every row.
/// Every tool over stdio (async jobs are always on there): name, title,
/// and the read-only and idempotent hints (every tool is non-destructive
/// and closed-world).
const EXPECTED_TOOLS: &[(&str, &str, bool, bool)] = &[
    ("cancel_render", "Cancel a render job", false, true),
    ("chalkboard_math", "Chalkboard math", true, true),
    (
        "describe_art_capabilities",
        "List art capabilities",
        true,
        true,
    ),
    ("get_renderer_info", "PSCat renderer info", true, true),
    ("handwrite", "Handwriting-style note", true, true),
    ("render_postscript", "Render PostScript", true, true),
    ("render_result", "Render job result", true, true),
    ("render_status", "Render job status", true, true),
    ("review_sheet", "Review sheet", true, true),
    ("start_render", "Start a render job", false, false),
    ("validate_postscript", "Validate PostScript", true, true),
];

async fn start() -> Client {
    start_with(env!("CARGO_BIN_EXE_pscat-mcp-server")).await
}

/// A client on a server started from the executable at `exe`.
async fn start_with(exe: impl AsRef<std::ffi::OsStr>) -> Client {
    let cmd = tokio::process::Command::new(exe).configure(|c| {
        c.arg("serve-stdio")
            .env("PSCAT_MCP_LOG_FORMAT", "pretty")
            .env("RUST_LOG", "warn");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn server");
    tokio::time::timeout(CALL_TIMEOUT, ().serve(transport))
        .await
        .expect("initialize timed out")
        .expect("initialize")
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{tool} timed out"))
        .unwrap_or_else(|e| panic!("{tool} failed at the protocol level: {e}"))
}

async fn tools(client: &Client) -> Vec<Tool> {
    tokio::time::timeout(CALL_TIMEOUT, client.list_all_tools())
        .await
        .expect("list_tools timed out")
        .expect("list_tools")
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured content")
}

fn has_image(r: &CallToolResult) -> bool {
    r.content
        .iter()
        .any(|c| matches!(c, ContentBlock::Image(_)))
}

fn error_code(r: &CallToolResult) -> &str {
    structured(r)["error"]["code"].as_str().expect("error code")
}

fn resources(r: &CallToolResult) -> Vec<&ResourceContents> {
    r.content
        .iter()
        .filter_map(|c| c.as_resource().map(|e| &e.resource))
        .collect()
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every key of `value` is a declared property and every required property
/// is present.
fn assert_conforms(schema: &serde_json::Map<String, Value>, value: &Value) {
    let props = schema["properties"].as_object().expect("schema properties");
    let obj = value.as_object().expect("structured content is an object");
    for key in obj.keys() {
        assert!(props.contains_key(key), "{key} is not in the output schema");
    }
    for req in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let req = req.as_str().unwrap();
        assert!(obj.contains_key(req), "required {req} missing from {value}");
    }
}

/// RGBA of the pixel at (x, y), y counted from the top.
fn png_pixel(png_bytes: &[u8], x: u32, y: u32) -> (u32, u32, [u8; 4]) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .expect("PNG buffer size fits in memory")
    ];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    let i = (y as usize * info.width as usize + x as usize) * channels;
    let px = match channels {
        4 => [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]],
        3 => [buf[i], buf[i + 1], buf[i + 2], 255],
        2 => [buf[i], buf[i], buf[i], buf[i + 1]],
        _ => [buf[i], buf[i], buf[i], 255],
    };
    (info.width, info.height, px)
}

/// Every pixel of a PNG as RGBA, row by row from the top-left.
fn png_pixels(png_bytes: &[u8]) -> (u32, u32, Vec<[u8; 4]>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .expect("PNG buffer size fits in memory")
    ];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    let pixels = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .map(|c| match channels {
            4 => [c[0], c[1], c[2], c[3]],
            3 => [c[0], c[1], c[2], 255],
            2 => [c[0], c[0], c[0], c[1]],
            _ => [c[0], c[0], c[0], 255],
        })
        .collect();
    (info.width, info.height, pixels)
}

#[tokio::test]
async fn lists_annotated_tools() {
    let client = start().await;
    let info = client.peer_info().expect("server info");
    let server = info.server_info.as_ref().expect("server_info");
    assert_eq!(server.name, "pscat-mcp");
    assert_eq!(server.version, env!("CARGO_PKG_VERSION"));
    assert!(info.capabilities.tools.is_some());
    // The guide prompts and resources (tests/guide.rs).
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());

    let tools = tools(&client).await;
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = EXPECTED_TOOLS.iter().map(|(name, ..)| *name).collect();
    expected.sort_unstable();
    assert_eq!(names, expected, "tool set differs from EXPECTED_TOOLS");
    for (name, title, read_only, idempotent) in EXPECTED_TOOLS {
        let t = tools.iter().find(|t| t.name == *name).unwrap();
        assert_eq!(t.title.as_deref(), Some(*title), "{name} title");
        let a = t.annotations.as_ref().expect("annotations");
        assert_eq!(a.read_only_hint, Some(*read_only), "{name}");
        assert_eq!(a.destructive_hint, Some(false), "{name}");
        assert_eq!(a.idempotent_hint, Some(*idempotent), "{name}");
        assert_eq!(a.open_world_hint, Some(false), "{name}");
        assert!(t.description.as_deref().is_some_and(|d| !d.is_empty()));
        assert!(
            t.output_schema.is_some(),
            "{name} declares an output schema"
        );
    }

    let render = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    let schema = &render.input_schema;
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, ["source"]);
    assert_eq!(
        schema["properties"]["format"]["enum"],
        json!(["png", "svg", "pdf"]),
        "{}",
        Value::Object((**schema).clone())
    );
    for prop in ["width", "height", "dpi"] {
        assert!(schema["properties"].get(prop).is_some(), "{prop}");
    }
    let output = render.output_schema.as_ref().expect("render output schema");
    let props = output["properties"].as_object().unwrap();
    for key in ["ok", "format", "artifacts", "error", "renderer"] {
        assert!(props.contains_key(key), "{key} in output schema");
    }
    let mut required: Vec<&str> = output["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["ok", "renderer"]);

    let validate = tools
        .iter()
        .find(|t| t.name == "validate_postscript")
        .unwrap();
    let vschema = validate
        .output_schema
        .as_ref()
        .expect("validate output schema");
    let vprops = vschema["properties"].as_object().unwrap();
    for key in [
        "ok",
        "valid",
        "syntax",
        "execution_error",
        "lint",
        "error",
        "renderer",
    ] {
        assert!(
            vprops.contains_key(key),
            "{key} in {}",
            Value::Object((**vschema).clone())
        );
    }
    let mut vrequired: Vec<&str> = vschema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    vrequired.sort_unstable();
    assert_eq!(vrequired, ["ok", "renderer"]);
    client.cancel().await.ok();
}

/// Every `format` keyword in `value`, with its JSON pointer.
fn formats_in(value: &Value, path: String, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(f)) = map.get("format") {
                out.push((path.clone(), f.clone()));
            }
            for (k, v) in map {
                formats_in(v, format!("{path}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                formats_in(v, format!("{path}/{i}"), out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn tool_schemas_have_no_uint_formats() {
    // schemars marks unsigned integers `format: uint32` (or `uint`,
    // `uint64`), which Claude Code reports as `unknown format ... ignored`
    // for every such field (#44). The minimum of 0 must survive.
    let client = start().await;
    let tools = tools(&client).await;
    let mut found = Vec::new();
    for t in &tools {
        let input = Value::Object((*t.input_schema).clone());
        formats_in(&input, format!("{}/input", t.name), &mut found);
        if let Some(out) = &t.output_schema {
            let output = Value::Object((**out).clone());
            formats_in(&output, format!("{}/output", t.name), &mut found);
        }
    }
    let uint: Vec<_> = found
        .iter()
        .filter(|(_, f)| f.starts_with("uint"))
        .collect();
    assert!(uint.is_empty(), "uint formats left: {uint:?}");
    let render = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    for prop in ["width", "height"] {
        let p = &render.input_schema["properties"][prop];
        assert_eq!(p["minimum"], json!(0), "{prop}: {p}");
        assert_eq!(p["type"], json!(["integer", "null"]), "{prop}: {p}");
    }
    let info = tools
        .iter()
        .find(|t| t.name == "get_renderer_info")
        .unwrap();
    let limits = &info.output_schema.as_ref().unwrap()["$defs"]["LimitsInfo"]["properties"];
    assert_eq!(limits["max_pixels"]["minimum"], json!(0), "{limits}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn renders_png_with_expected_pixels() {
    let client = start().await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": SHAPES, "width": 200, "height": 100}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let images: Vec<_> = r.content.iter().filter_map(|c| c.as_image()).collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&images[0].data)
        .unwrap();
    assert!(bytes.starts_with(b"\x89PNG"));

    let (w, h, left) = png_pixel(&bytes, 50, 20);
    assert_eq!((w, h), (200, 100));
    assert_eq!(left, [255, 0, 0, 255], "left half is red");
    let (_, _, right) = png_pixel(&bytes, 150, 50);
    assert_eq!(right, [0, 0, 255, 255], "right half is blue");
    // PostScript (25, 25) is PNG row 100 - 1 - 25 = 74.
    let (_, _, green) = png_pixel(&bytes, 25, 74);
    assert_eq!(green, [0, 255, 0, 255], "bottom-left square is green");

    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["format"], "png");
    assert_eq!(s["mime_type"], "image/png");
    assert_eq!(s["page_count"], 1);
    assert_eq!(s["artifacts"][0]["width_px"], 200);
    assert_eq!(s["artifacts"][0]["height_px"], 100);
    assert_eq!(s["artifacts"][0]["bytes"], bytes.len());
    assert_eq!(s["renderer"]["engine"], "pscat");
    assert!(
        !s.to_string().contains(&images[0].data),
        "no image data in structured content"
    );
    assert!(text_of(&r).contains("200x100"));

    let tools = tools(&client).await;
    let render = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    assert_conforms(render.output_schema.as_ref().unwrap(), s);
    client.cancel().await.ok();
}

#[tokio::test]
async fn renders_svg_and_pdf_as_embedded_resources() {
    let client = start().await;
    let svg = call(
        &client,
        "render_postscript",
        json!({"source": SHAPES, "width": 200, "height": 100, "format": "svg"}),
    )
    .await;
    assert_eq!(svg.is_error, Some(false), "{}", text_of(&svg));
    assert!(!has_image(&svg));
    let res = resources(&svg);
    assert_eq!(res.len(), 1);
    match res[0] {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
            assert!(
                uri.starts_with("urn:pscat:render:") && uri.ends_with(":page:1"),
                "{uri}"
            );
            let head = text.trim_start();
            assert!(
                head.starts_with("<?xml") || head.starts_with("<svg"),
                "{head:.80}"
            );
            assert!(text.contains("<svg"));
        }
        other => panic!("expected a text resource, got {other:?}"),
    }
    assert_eq!(structured(&svg)["mime_type"], "image/svg+xml");

    let pdf = call(
        &client,
        "render_postscript",
        json!({"source": SHAPES, "width": 200, "height": 100, "format": "pdf"}),
    )
    .await;
    assert_eq!(pdf.is_error, Some(false), "{}", text_of(&pdf));
    assert!(!has_image(&pdf));
    let res = resources(&pdf);
    assert_eq!(res.len(), 1);
    match res[0] {
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("application/pdf"));
            assert!(
                uri.starts_with("urn:pscat:render:") && uri.ends_with(":document"),
                "{uri}"
            );
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .unwrap();
            assert!(bytes.starts_with(b"%PDF-"));
        }
        other => panic!("expected a blob resource, got {other:?}"),
    }
    let s = structured(&pdf);
    assert_eq!(s["format"], "pdf");
    assert!(s["artifacts"][0].get("page").is_none());
    client.cancel().await.ok();
}

#[tokio::test]
async fn postscript_errors_are_tool_errors_without_images() {
    let client = start().await;
    let render_tool = tools(&client)
        .await
        .into_iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    for (source, name) in [("1 2 nosuchop", "undefined"), ("{ 1 2", "syntaxerror")] {
        let r = call(&client, "render_postscript", json!({"source": source})).await;
        assert_eq!(r.is_error, Some(true), "{source}");
        assert!(!has_image(&r), "{source}: no image on failure");
        assert!(resources(&r).is_empty());
        let s = structured(&r);
        assert_eq!(s["ok"], false);
        assert_eq!(error_code(&r), "postscript_error", "{source}: {s}");
        assert_eq!(s["error"]["postscript"]["name"], name, "{source}: {s}");
        assert!(s.get("artifacts").is_none());
        assert!(!text_of(&r).is_empty());
        assert_conforms(render_tool.output_schema.as_ref().unwrap(), s);
    }

    // The same connection still renders after errors.
    let ok = call(
        &client,
        "render_postscript",
        json!({"source": SHAPES, "width": 200, "height": 100}),
    )
    .await;
    assert_eq!(ok.is_error, Some(false), "{}", text_of(&ok));
    assert!(has_image(&ok));
    client.cancel().await.ok();
}

#[tokio::test]
async fn rejects_bad_requests_before_execution() {
    let client = start().await;
    let r = call(
        &client,
        "render_postscript",
        json!({"source": "showpage", "format": "jpeg"}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(error_code(&r), "unsupported_format");
    assert!(!has_image(&r));

    let big = "% padding padding padding padding\n".repeat(9000);
    assert!(big.len() > 256 * 1024);
    let r = call(&client, "render_postscript", json!({"source": big})).await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(error_code(&r), "source_too_large");

    let r = call(
        &client,
        "render_postscript",
        json!({"source": "showpage", "width": 9000, "height": 100}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(error_code(&r), "invalid_dimensions");
    assert!(!has_image(&r));

    let r = call(&client, "validate_postscript", json!({"source": big})).await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(structured(&r)["ok"], false);
    assert_eq!(error_code(&r), "source_too_large");
    client.cancel().await.ok();
}

/// Arguments that do not deserialize (rmcp rejects them before the tool
/// body runs) still get the structured failure envelope (issue #4).
#[tokio::test]
async fn argument_type_errors_get_the_structured_envelope() {
    let client = start().await;
    let output_schemas: std::collections::HashMap<String, serde_json::Map<String, Value>> =
        tools(&client)
            .await
            .into_iter()
            .filter_map(|t| Some((t.name.to_string(), (*t.output_schema?).clone())))
            .collect();
    let cases = [
        (
            "render_postscript",
            json!({"source": "showpage", "width": -5}),
        ),
        (
            "render_postscript",
            json!({"source": "showpage", "dpi": "high"}),
        ),
        (
            "render_postscript",
            json!({"source": "showpage", "halftone": "yes"}),
        ),
        (
            "render_postscript",
            json!({"source": "showpage", "halftone": 1}),
        ),
        ("render_postscript", json!({"format": "png"})),
        ("validate_postscript", json!({"source": 42})),
        ("chalkboard_math", json!({"latex": "x", "dpi": "high"})),
        ("handwrite", json!({"text": "hi", "size": "big"})),
        ("handwrite", json!({"text": "hi", "width": -5})),
        ("handwrite", json!({"size": 36})),
        ("review_sheet", json!({"entries": [{"source": 42}]})),
        (
            "review_sheet",
            json!({"entries": [{"source": "showpage"}], "sizes": 1}),
        ),
        ("review_sheet", json!({"width": 100})),
        ("describe_art_capabilities", json!({"names": "pgcard"})),
        ("describe_art_capabilities", json!({"offset": -1})),
        (
            "render_postscript",
            json!({"source": "showpage", "width": "x".repeat(100_000)}),
        ),
        ("start_render", json!({"format": "png"})),
        ("start_render", json!({"source": "showpage", "dpi": "high"})),
        ("render_status", json!({})),
        ("render_result", json!({"job_id": 7})),
        ("cancel_render", json!({"job_id": null})),
    ];
    for (tool, args) in cases {
        let r = call(&client, tool, args.clone()).await;
        assert_eq!(r.is_error, Some(true), "{tool} {args}");
        assert!(!has_image(&r), "{tool} {args}");
        let s = structured(&r);
        assert_eq!(s["ok"], false, "{tool} {args}: {s}");
        assert_eq!(error_code(&r), "invalid_request", "{tool} {args}");
        assert_eq!(s["renderer"]["engine"], "pscat");
        let message = s["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("invalid arguments:"), "{message}");
        assert!(message.chars().count() < 400, "detail not truncated");
        assert!(text_of(&r).contains("invalid_request"), "{}", text_of(&r));
        assert_conforms(&output_schemas[tool], s);
    }
    client.cancel().await.ok();
}

/// Half-grey on the left, black bottom-right, red top-right, white elsewhere.
const GREYS: &str = "\
0.5 setgray 0 0 100 100 rectfill
0 setgray 100 0 100 50 rectfill
1 0 0 setrgbcolor 100 50 100 50 rectfill
showpage
";

fn first_png(r: &CallToolResult) -> Vec<u8> {
    let image = r
        .content
        .iter()
        .find_map(|c| c.as_image())
        .expect("an image block");
    base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .unwrap()
}

/// `halftone: true` screens the PNG in the worker: every pixel is pure black
/// or white, grey becomes a dot pattern of about the same ink coverage, and
/// the same program without the option keeps its grey and red.
#[tokio::test]
async fn halftone_screens_the_png_to_black_and_white() {
    let client = start().await;
    let args = json!({"source": GREYS, "width": 200, "height": 100});
    let plain = call(&client, "render_postscript", args.clone()).await;
    assert_eq!(plain.is_error, Some(false), "{}", text_of(&plain));
    let (w, h, plain_px) = png_pixels(&first_png(&plain));
    assert_eq!((w, h), (200, 100));
    assert!(
        plain_px
            .iter()
            .any(|p| p[0] == p[1] && p[1] == p[2] && p[0] > 100 && p[0] < 160),
        "the normal render has mid grey"
    );
    assert!(plain_px.iter().any(|p| p[0] == 255 && p[1] == 0));

    let mut with = args.clone();
    with["halftone"] = json!(true);
    let r = call(&client, "render_postscript", with).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let bytes = first_png(&r);
    let (w, h, px) = png_pixels(&bytes);
    assert_eq!((w, h), (200, 100), "same size as the normal render");
    let black = [0, 0, 0, 255];
    let white = [255, 255, 255, 255];
    assert!(
        px.iter().all(|p| *p == black || *p == white),
        "only pure black and white pixels"
    );
    // Left half is 50% grey: roughly half the pixels carry ink.
    let left: Vec<_> = (0..h)
        .flat_map(|y| (0..100).map(move |x| (y * w + x) as usize))
        .map(|i| px[i])
        .collect();
    let ink = left.iter().filter(|p| **p == black).count() as f64 / left.len() as f64;
    assert!(
        (0.35..0.65).contains(&ink),
        "grey keeps its tone: ink {ink}"
    );
    // Solid black stays black (bottom right: PS y 0..50 is PNG rows 50..100).
    let bottom_right: Vec<_> = (50..100)
        .flat_map(|y| (110..190).map(move |x| (y * w + x) as usize))
        .map(|i| px[i])
        .collect();
    assert!(bottom_right.iter().filter(|p| **p == black).count() * 100 > bottom_right.len() * 95);
    assert!(
        px.iter().all(|p| p[0] == p[1] && p[1] == p[2]),
        "red is screened to grey tones, no colour"
    );
    assert_ne!(
        bytes,
        first_png(&plain),
        "different bytes from the normal PNG"
    );

    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["format"], "png");
    assert_eq!(s["artifacts"][0]["width_px"], 200);
    assert_eq!(s["artifacts"][0]["bytes"], bytes.len());
    let tools = tools(&client).await;
    let render = tools
        .iter()
        .find(|t| t.name == "render_postscript")
        .unwrap();
    assert_conforms(render.output_schema.as_ref().unwrap(), s);
    let schema = serde_json::to_value(&*render.input_schema).unwrap();
    assert!(
        schema["properties"]["halftone"]["type"]
            .to_string()
            .contains("boolean")
    );
    assert_eq!(schema["properties"]["halftone"]["default"], false);

    // `halftone: false` is the normal render, byte for byte.
    let mut off = args;
    off["halftone"] = json!(false);
    let r = call(&client, "render_postscript", off).await;
    assert_eq!(first_png(&r), first_png(&plain));
    client.cancel().await.ok();
}

/// Every page of a multi-page program is screened.
#[tokio::test]
async fn halftone_screens_every_page() {
    let client = start().await;
    let r = call(
        &client,
        "render_postscript",
        json!({
            "source": "0.5 setgray 0 0 50 50 rectfill showpage 0.8 setgray 0 0 50 50 rectfill showpage",
            "width": 50, "height": 50, "halftone": true
        }),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let images: Vec<_> = r.content.iter().filter_map(|c| c.as_image()).collect();
    assert_eq!(images.len(), 2);
    let mut ink = Vec::new();
    for image in images {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        let (_, _, px) = png_pixels(&bytes);
        assert!(
            px.iter()
                .all(|p| *p == [0, 0, 0, 255] || *p == [255, 255, 255, 255])
        );
        ink.push(px.iter().filter(|p| p[0] == 0).count());
    }
    assert!(ink[1] < ink[0], "the lighter page has less ink: {ink:?}");
    assert_eq!(structured(&r)["page_count"], 2);
    client.cancel().await.ok();
}

/// `halftone` is a raster option: with svg or pdf the call is refused before
/// anything runs, with the structured envelope.
#[tokio::test]
async fn halftone_with_svg_or_pdf_is_invalid_request() {
    let client = start().await;
    for format in ["svg", "pdf", "SVG"] {
        let r = call(
            &client,
            "render_postscript",
            json!({"source": SHAPES, "format": format, "halftone": true}),
        )
        .await;
        assert_eq!(r.is_error, Some(true), "{format}");
        assert_eq!(error_code(&r), "invalid_request", "{format}");
        assert_eq!(structured(&r)["ok"], false);
        assert!(!has_image(&r) && resources(&r).is_empty(), "{format}");
        let message = structured(&r)["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("halftone") && message.contains("png"),
            "{message}"
        );
    }
    // `halftone: false` with svg is just an svg render.
    let r = call(
        &client,
        "render_postscript",
        json!({"source": SHAPES, "format": "svg", "halftone": false}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

#[tokio::test]
async fn validates_programs() {
    let client = start().await;
    let validate_tool = tools(&client)
        .await
        .into_iter()
        .find(|t| t.name == "validate_postscript")
        .unwrap();
    let schema = validate_tool.output_schema.as_ref().unwrap();

    let r = call(&client, "validate_postscript", json!({"source": SHAPES})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert!(!has_image(&r));
    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["valid"], true, "{s}");
    assert_eq!(s["syntax"]["ok"], true);
    assert_eq!(s["page_count"], 1);
    assert_conforms(schema, s);

    let r = call(
        &client,
        "validate_postscript",
        json!({"source": "1 2 3 gsave"}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_eq!(s["valid"], true, "{s}");
    let checks: Vec<&str> = s["lint"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["check"].as_str().unwrap())
        .collect();
    assert!(checks.contains(&"stack-leak"), "{checks:?}");
    assert!(checks.contains(&"gsave-imbalance"), "{checks:?}");

    let r = call(
        &client,
        "validate_postscript",
        json!({"source": "1 2 nosuchop"}),
    )
    .await;
    assert_eq!(
        r.is_error,
        Some(false),
        "an invalid program is a validation result"
    );
    assert!(!has_image(&r));
    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["valid"], false);
    assert_eq!(s["execution_error"]["name"], "undefined", "{s}");
    assert!(text_of(&r).contains("undefined"));
    assert_conforms(schema, s);
    client.cancel().await.ok();
}

#[tokio::test]
async fn reports_renderer_info() {
    let client = start().await;
    let r = call(&client, "get_renderer_info", json!({})).await;
    assert_eq!(r.is_error, Some(false));
    let s = structured(&r);
    let formats: Vec<(&str, &str)> = s["formats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["name"].as_str().unwrap(),
                f["mime_type"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        formats,
        [
            ("png", "image/png"),
            ("svg", "image/svg+xml"),
            ("pdf", "application/pdf")
        ]
    );
    assert_eq!(s["renderer"]["engine"], "pscat");
    // Both come from Cargo.lock's pscat entry (build.rs), whatever the pin.
    let lock = include_str!("../Cargo.lock");
    let entry = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"pscat\"\n"))
        .expect("pscat in Cargo.lock");
    let field = |k: &str| {
        entry
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{k} = \"")))
            .map(|v| v.trim_end_matches('"').to_string())
            .unwrap()
    };
    assert_eq!(s["renderer"]["version"], field("version"));
    let rev = s["renderer"]["revision"].as_str().expect("git revision");
    assert_eq!(rev.len(), 40, "{rev}");
    assert!(field("source").ends_with(&format!("#{rev}")));
    assert_eq!(s["server_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(s["limits"]["max_source_bytes"], 256 * 1024);
    assert_eq!(s["limits"]["max_side_px"], 8000);
    assert!(s["limits"]["max_concurrent_renders"].as_u64().unwrap() >= 1);
    assert!(!s["capabilities"].as_array().unwrap().is_empty());
    client.cancel().await.ok();
}

/// What every `describe_art_capabilities` reply stays under, in both its
/// structured and its text form.
const REPLY_CEILING: usize = 24 * 1024;

/// Assert the reply is within the ceiling in both forms; returns the
/// (structured, text) sizes.
fn assert_reply_size(r: &CallToolResult) -> (usize, usize) {
    let structured_bytes = structured(r).to_string().len();
    let text_bytes = text_of(r).len();
    assert!(
        structured_bytes < REPLY_CEILING,
        "structuredContent is {structured_bytes} bytes"
    );
    assert!(text_bytes < REPLY_CEILING, "text is {text_bytes} bytes");
    (structured_bytes, text_bytes)
}

async fn capabilities_tool(client: &Client) -> Tool {
    tools(client)
        .await
        .into_iter()
        .find(|t| t.name == "describe_art_capabilities")
        .unwrap()
}

/// The kinds the tool accepts, from its input schema.
fn kind_enum(tool: &Tool) -> Vec<String> {
    tool.input_schema["properties"]["kind"]["enum"]
        .as_array()
        .expect("kind enum")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// With no arguments the tool returns a compact index of every kind, read
/// in a worker, as structured content that matches the declared schema.
#[tokio::test]
async fn describes_art_capabilities_as_an_index() {
    let client = start().await;
    let tool = capabilities_tool(&client).await;
    let schema = tool.output_schema.as_ref().unwrap();

    let r = call(&client, "describe_art_capabilities", json!({})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert!(!has_image(&r));
    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["view"], "index");
    assert_conforms(schema, s);
    let (structured_bytes, text_bytes) = assert_reply_size(&r);
    eprintln!("index reply: {structured_bytes} structured bytes, {text_bytes} text bytes");
    assert_eq!(s["renderer"]["engine"], "pscat");
    assert_eq!(s["pscat_version"], s["renderer"]["version"]);
    assert_eq!(s["catalog_signature"].as_str().unwrap().len(), 16);
    assert!(s.get("entries").is_none() && s.get("summaries").is_none());

    // The `kind` argument accepts exactly the kinds the catalog has. A PSCat
    // bump that adds (or drops) a kind fails here, so no kind is left
    // unqueryable.
    let accepted: std::collections::BTreeSet<String> = kind_enum(&tool).into_iter().collect();
    let index = s["index"].as_array().unwrap();
    let in_catalog: std::collections::BTreeSet<String> = index
        .iter()
        .map(|k| k["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        in_catalog, accepted,
        "kinds in the catalog vs the `kind` enum in the input schema; update \
         KINDS in src/catalog.rs and the enum in src/mcp.rs"
    );
    let constant: std::collections::BTreeSet<String> = pscat_mcp::catalog::KINDS
        .iter()
        .map(|k| k.to_string())
        .collect();
    assert_eq!(constant, accepted, "catalog::KINDS vs the schema enum");
    let fonts = index.iter().find(|k| k["kind"] == "font").unwrap();
    assert!(
        fonts["names"]
            .as_array()
            .unwrap()
            .contains(&json!("Helvetica")),
        "{fonts}"
    );
    for k in index {
        assert_eq!(k["count"], k["names"].as_array().unwrap().len());
    }

    // The text carries the same names and says how to get more.
    let text = text_of(&r);
    for k in index {
        for name in k["names"].as_array().unwrap() {
            assert!(text.contains(name.as_str().unwrap()), "{name}");
        }
    }
    assert!(text.contains("names (up to 20)"), "{text}");
    // A client that shows the model only structuredContent gets the same hint.
    assert!(s["details"].as_str().unwrap().contains("names (up to 20)"));
    assert!(!text.contains("structuredContent"), "{text}");

    // The catalog is kept, so a second call is the same without a new worker.
    let again = call(&client, "describe_art_capabilities", json!({})).await;
    assert_eq!(structured(&again), s);
    client.cancel().await.ok();
}

/// A client may leave `arguments` out altogether.
#[tokio::test]
async fn describe_art_capabilities_works_without_an_arguments_field() {
    let client = start().await;
    let r = tokio::time::timeout(
        CALL_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("describe_art_capabilities")),
    )
    .await
    .expect("timed out")
    .expect("call");
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert_eq!(structured(&r)["view"], "index");
    client.cancel().await.ok();
}

/// `kind` lists names with one-line descriptions, a bounded page at a time;
/// paging covers exactly the names the index shows, and no reply is large.
#[tokio::test]
async fn describe_art_capabilities_lists_a_kind_in_pages() {
    let client = start().await;
    let tool = capabilities_tool(&client).await;
    let schema = tool.output_schema.as_ref().unwrap();
    let index = call(&client, "describe_art_capabilities", json!({})).await;
    let index = structured(&index)["index"].as_array().unwrap().clone();

    let mut largest = (0, 0);
    for k in &index {
        let kind = k["kind"].as_str().unwrap();
        let expected: Vec<&str> = k["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        let mut got: Vec<String> = Vec::new();
        let mut offset = 0;
        // A small `limit` on the largest kinds forces several pages.
        let limit = if expected.len() > 100 { 60 } else { 100 };
        loop {
            let r = call(
                &client,
                "describe_art_capabilities",
                json!({"kind": kind, "offset": offset, "limit": limit}),
            )
            .await;
            assert_eq!(r.is_error, Some(false), "{kind}: {}", text_of(&r));
            let s = structured(&r);
            assert_conforms(schema, s);
            assert_eq!(s["view"], "kind");
            assert_eq!(s["kind"], kind);
            assert_eq!(s["total"], expected.len());
            assert_eq!(s["offset"], offset);
            assert!(
                s.get("entries").is_none(),
                "kind alone returns no full entries"
            );
            let (a, b) = assert_reply_size(&r);
            largest = (largest.0.max(a), largest.1.max(b));
            let text = text_of(&r);
            for item in s["summaries"].as_array().unwrap() {
                let name = item["name"].as_str().unwrap();
                assert!(text.contains(name), "{name} in the text");
                assert!(item["description"].as_str().unwrap().chars().count() <= 101);
                got.push(name.to_string());
            }
            match s["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(got, expected, "{kind}");
    }

    // The default page of the largest kind (no offset or limit) is also bounded.
    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"kind": "procedure"}),
    )
    .await;
    let (a, b) = assert_reply_size(&r);
    eprintln!("procedure page (default limit): {a} structured bytes, {b} text bytes");
    assert!(structured(&r)["summaries"].as_array().unwrap().len() <= 100);
    eprintln!(
        "largest kind page seen: {} structured bytes, {} text bytes",
        largest.0, largest.1
    );
    client.cancel().await.ok();
}

/// `names` returns full entries, reports unknown names without failing, and
/// stays under the ceiling even for the largest entries.
#[tokio::test]
async fn describe_art_capabilities_returns_named_entries() {
    let client = start().await;
    let tool = capabilities_tool(&client).await;
    let schema = tool.output_schema.as_ref().unwrap();

    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"names": ["pgcard", "Helvetica", "no-such-name"]}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_conforms(schema, s);
    assert_eq!(s["view"], "names");
    assert_eq!(s["not_found"], json!(["no-such-name"]));
    assert_eq!(s["omitted"], json!([]));
    let entries = s["entries"].as_array().unwrap();
    let kinds: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(kinds, [("pgcard", "template"), ("Helvetica", "font")]);
    let card = &entries[0];
    assert_eq!(card["source"], "lib/pagekit.ps");
    assert!(
        card["load"]
            .as_str()
            .unwrap()
            .contains("(lib/artkit.ps) run")
    );
    assert!(card["parameters"].as_array().unwrap().len() >= 3);
    assert_eq!(entries[1]["availability"], "builtin");
    // The text carries the same entries.
    let text = text_of(&r);
    assert!(text.contains(card["load"].as_str().unwrap()), "{text}");
    assert!(text.contains("parameter Title"), "{text}");
    assert!(text.contains("Not found: no-such-name"), "{text}");
    assert!(!text.contains("structuredContent"), "{text}");
    assert_reply_size(&r);

    // `kind` narrows `names`.
    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"names": ["pgcard", "Helvetica"], "kind": "font"}),
    )
    .await;
    let s = structured(&r);
    assert_eq!(s["entries"].as_array().unwrap().len(), 1);
    assert_eq!(s["not_found"], json!(["pgcard"]));

    // The 20 names with the largest entries in the pinned PSCat: the reply
    // keeps whole entries that fit and lists the rest as omitted.
    let heavy = [
        "pkwet",
        "tw-type",
        "tw-setup",
        "rel-page",
        "dkstroke",
        "pkbrushedge",
        "pkfan",
        "pktrowel",
        "pkdab",
        "pkfeather",
        "pkbroad",
        "dycrayon",
        "pktrowelplane",
        "dcchalk",
        "tw-set",
        "pkoil",
        "mkalign",
        "jerseylayout",
        "hatch",
        "dcboard",
    ];
    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"names": heavy}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_conforms(schema, s);
    let (a, b) = assert_reply_size(&r);
    eprintln!("20 largest names: {a} structured bytes, {b} text bytes");
    let accounted = s["entries"].as_array().unwrap().len()
        + s["omitted"].as_array().unwrap().len()
        + s["not_found"].as_array().unwrap().len();
    assert_eq!(accounted, heavy.len(), "{s}");
    assert!(!s["entries"].as_array().unwrap().is_empty());
    client.cancel().await.ok();
}

/// Arguments that parse but make no sense are `invalid_request` with the
/// envelope, and start no worker.
#[tokio::test]
async fn describe_art_capabilities_rejects_bad_arguments() {
    let client = start().await;
    let schema = capabilities_tool(&client).await.output_schema.unwrap();
    let too_many: Vec<String> = (0..21).map(|i| format!("n{i}")).collect();
    for args in [
        json!({"kind": "fonts"}),
        json!({"names": []}),
        json!({"names": too_many}),
        json!({"offset": 5}),
        json!({"kind": "font", "limit": 0}),
        json!({"kind": "font", "limit": 101}),
        json!({"kind": "font", "names": ["Helvetica"], "offset": 1}),
    ] {
        let r = call(&client, "describe_art_capabilities", args.clone()).await;
        assert_eq!(r.is_error, Some(true), "{args}");
        let s = structured(&r);
        assert_eq!(s["ok"], false, "{args}: {s}");
        assert_eq!(error_code(&r), "invalid_request", "{args}");
        assert!(s.get("view").is_none(), "{args}");
        assert_conforms(&schema, s);
    }
    client.cancel().await.ok();
}

/// Unknown arguments are ignored rather than echoed or executed, and the
/// catalog is not affected by them.
#[tokio::test]
async fn describe_art_capabilities_ignores_extra_arguments() {
    let client = start().await;
    let plain = call(&client, "describe_art_capabilities", json!({})).await;
    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"source": "(x) print", "out_path": "x.png"}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert_eq!(structured(&r), structured(&plain));
    client.cancel().await.ok();
}

/// Font packs and font override directories next to the executable are
/// reported by name, but their locations never leave the server.
#[tokio::test]
async fn describe_art_capabilities_does_not_report_server_paths() {
    // Under target/, on the same volume as the build, and removed even if
    // the test panics.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pscat-mcp-caps-{}", uuid::Uuid::new_v4()));
    let _scratch = Scratch(dir.clone());
    let pack = dir.join("fonts/packs/testpack");
    std::fs::create_dir_all(&pack).unwrap();
    std::fs::create_dir_all(dir.join("fonts/catalog")).unwrap();
    std::fs::write(
        pack.join("install.json"),
        r#"{"pack":"testpack","tag":"fonts-test","pack_sha256":"abc123","source":"test"}"#,
    )
    .unwrap();
    // The worker looks for packs beside the executable, so the server runs
    // from `dir`. A hard link keeps the copy cheap.
    let original = env!("CARGO_BIN_EXE_pscat-mcp-server");
    let exe = dir.join(std::path::Path::new(original).file_name().unwrap());
    if std::fs::hard_link(original, &exe).is_err() {
        std::fs::copy(original, &exe).unwrap();
    }

    let client = start_with(&exe).await;
    let r = call(&client, "describe_art_capabilities", json!({})).await;
    client.cancel().await.ok();
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    let packs = &s["font_packs"];
    // The test setup took effect...
    assert_eq!(packs["installed"].as_array().unwrap().len(), 1, "{packs}");
    assert_eq!(packs["installed"][0]["name"], "testpack");
    assert_eq!(packs["installed"][0]["status"], "user-selected");
    assert_eq!(packs["overrides"].as_array().unwrap().len(), 1, "{packs}");
    // ...and none of it says where.
    let all = format!("{s}\n{}", text_of(&r));
    let unique = dir.file_name().unwrap().to_str().unwrap();
    assert!(!all.contains(unique), "server path leaked");
    assert!(!all.contains("\"path\""), "path key leaked");
    assert!(
        !all.contains(env!("CARGO_MANIFEST_DIR")),
        "checkout path leaked"
    );
}

/// The `load` line the catalog gives for a template works in the render
/// sandbox, so the names it reports are usable by `render_postscript`.
#[tokio::test]
async fn catalog_load_lines_work_in_a_render() {
    let client = start().await;
    let r = call(
        &client,
        "describe_art_capabilities",
        json!({"names": ["pgcard"], "kind": "template"}),
    )
    .await;
    let card = structured(&r)["entries"][0].clone();
    assert_eq!(card["name"], "pgcard");
    let load = card["load"].as_str().unwrap();
    let source = format!(
        "{load}\n0 0 400 300 << /Title (Hello) /Body (From the catalog) >> pgcard pop\nshowpage\n"
    );
    let r = call(
        &client,
        "render_postscript",
        json!({"source": source, "width": 400, "height": 300}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let ContentBlock::Image(img) = r
        .content
        .iter()
        .find(|c| matches!(c, ContentBlock::Image(_)))
        .unwrap()
    else {
        unreachable!()
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&img.data)
        .unwrap();
    let (w, h, _) = png_pixel(&bytes, 0, 0);
    assert_eq!((w, h), (400, 300));
    let mut colors = std::collections::HashSet::new();
    for (x, y) in (0..w)
        .step_by(20)
        .flat_map(|x| (0..h).step_by(20).map(move |y| (x, y)))
    {
        colors.insert(png_pixel(&bytes, x, y).2);
    }
    assert!(
        colors.len() > 1,
        "the template drew something: {}",
        colors.len()
    );
    client.cancel().await.ok();
}
