//! `review_sheet` end to end: a real rmcp client on the real server binary
//! over stdio, real PSCat in the worker; and the service directly, to count
//! what was executed.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pscat_mcp::config::{LimitsConfig, WorkerConfig};
use pscat_mcp::generate::GenerateJob;
use pscat_mcp::render::{ErrorCode, GenerateRequest, RenderService};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

mod common;

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A client on a server started with the extra `PSCAT_MCP_*` variables.
async fn start(env: &[(&str, &str)]) -> Client {
    common::start_stdio(env!("CARGO_BIN_EXE_pscat-mcp-server"), env).await
}

async fn call_tool(client: &Client, name: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
    tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed at the protocol level")
}

async fn call(client: &Client, args: Value) -> CallToolResult {
    call_tool(client, "review_sheet", args).await
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured content")
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
    assert_eq!(s["error"]["code"], code, "{s}");
    assert_eq!(s["renderer"]["engine"], "pscat");
    assert!(text_of(r).contains(code), "{}", text_of(r));
    s
}

fn assert_ok(r: &CallToolResult) -> &Value {
    assert_eq!(r.is_error, Some(false), "{}", text_of(r));
    let s = structured(r);
    assert_eq!(s["ok"], true, "{s}");
    s
}

/// A decoded PNG: width, height and RGBA pixels.
struct Png {
    w: u32,
    h: u32,
    px: Vec<[u8; 4]>,
}

impl Png {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.px[(y * self.w + x) as usize]
    }

    /// The `w`x`h` region with its top-left at `(x, y)`.
    fn region(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<[u8; 4]> {
        (y..y + h)
            .flat_map(|yy| (x..x + w).map(move |xx| (xx, yy)))
            .map(|(xx, yy)| self.at(xx, yy))
            .collect()
    }
}

fn decode(bytes: &[u8]) -> Png {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
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
    Png {
        w: info.width,
        h: info.height,
        px,
    }
}

/// The first PNG image block, decoded.
fn png_of(r: &CallToolResult) -> Png {
    let image = r
        .content
        .iter()
        .find_map(|c| c.as_image())
        .expect("an image block");
    assert_eq!(image.mime_type, "image/png");
    decode(
        &base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap(),
    )
}

/// `structuredContent.cells[i]`.
fn cell(s: &Value, i: usize) -> &Value {
    &s["cells"][i]
}

/// The top-left of cell `i` on the sheet.
fn xy(s: &Value, i: usize) -> (u32, u32) {
    let c = cell(s, i);
    (
        c["x"].as_u64().unwrap() as u32,
        c["y"].as_u64().unwrap() as u32,
    )
}

/// `schema` with a `$ref` into `root`'s `$defs` followed, and `anyOf` with
/// a `null` branch (an `Option`) unwrapped.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let name = r.rsplit('/').next().unwrap();
        return resolve(root, &root["$defs"][name]);
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array)
        && let Some(other) = any.iter().find(|s| s.get("type") != Some(&json!("null")))
    {
        return resolve(root, other);
    }
    schema
}

/// Every key of `value`, recursively through objects and arrays, is
/// declared by `schema`, and every string with an `enum` is one of it.
fn assert_conforms(root: &Value, schema: &Value, value: &Value, at: &str) {
    let schema = resolve(root, schema);
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        assert!(e.contains(value), "{at}: {value} not in {e:?}");
    }
    match value {
        Value::Object(o) => {
            let props = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{at}: no properties in {schema}"));
            for (k, v) in o {
                let sub = props
                    .get(k)
                    .unwrap_or_else(|| panic!("{at}.{k} is not in the output schema"));
                assert_conforms(root, sub, v, &format!("{at}.{k}"));
            }
            for req in schema["required"].as_array().into_iter().flatten() {
                assert!(o.contains_key(req.as_str().unwrap()), "{at}: {req} missing");
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                assert_conforms(root, &schema["items"], v, &format!("{at}[{i}]"));
            }
        }
        _ => {}
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
/// The hairline PSCat draws around each cell.
const HAIRLINE: [u8; 4] = [190, 190, 190, 255];

/// A program that fills its whole page with one colour and draws a black
/// 10-point square at the origin.
fn filled(rgb: &str) -> String {
    format!("{rgb} setrgbcolor clippath fill 0 setgray 0 0 10 10 rectfill showpage")
}

#[tokio::test]
async fn the_tool_is_listed_with_the_plugins_arguments() {
    let client = start(&[("PSCAT_MCP_MAX_SHEET_CELLS", "12")]).await;
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools
        .iter()
        .find(|t| t.name == "review_sheet")
        .expect("review_sheet listed");
    let schema = Value::Object((*t.input_schema).clone());
    let text = schema.to_string();
    assert!(
        !text.contains("$ref") && !text.contains("$defs"),
        "{schema}"
    );
    let props = schema["properties"].as_object().unwrap();
    let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["dpi", "entries", "height", "labels", "sizes", "width"]
    );
    assert_eq!(schema["required"], json!(["entries"]));
    assert_eq!(props["entries"]["maxItems"], 12, "{schema}");
    let entry = &props["entries"]["items"];
    let mut keys: Vec<&str> = entry["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["dpi", "height", "label", "source", "width"]);
    assert_eq!(entry["required"], json!(["source"]));
    assert_eq!(entry["properties"]["label"]["maxLength"], 200);
    let item = &props["dpi"]["items"]["anyOf"];
    assert_eq!(item[0]["type"], "number");
    assert_eq!(item[1]["properties"]["zoom"]["maximum"], 16);
    let d = t.description.as_deref().unwrap();
    assert!(d.contains("at most 12 cells"), "{d}");
    assert!(d.contains("for the whole sheet"), "{d}");
    assert!(!d.contains("out_path"), "{d}");
    let out = t.output_schema.as_ref().expect("output schema");
    for key in ["ok", "cells", "width_px", "failed_cells", "error"] {
        assert!(out["properties"].get(key).is_some(), "{key}");
    }
    let info = call_tool(&client, "get_renderer_info", json!({})).await;
    let limits = &structured(&info)["limits"];
    assert_eq!(limits["max_sheet_cells"], 12);
    assert_eq!(limits["max_sheet_source_bytes"], 262_144);
    assert_eq!(limits["max_sheet_pixels"], 16_000_000);
    assert_eq!(limits["max_sheet_cell_pixels"], 16_000_000);
    assert_eq!(limits["sheet_timeout_ms"], 20_000);
    client.cancel().await.ok();
}

/// Two programs at two sizes each: four cells, each at its true pixel size
/// and in its own colour, placed where `structuredContent` says.
#[tokio::test]
async fn a_two_by_two_sheet_of_real_programs() {
    let client = start(&[]).await;
    let r = call(
        &client,
        json!({
            "entries": [
                {"source": filled("1 0 0"), "label": "red"},
                {"source": filled("0 0 1"), "label": "blue"}
            ],
            "width": 100, "height": 50, "dpi": [72, 144]
        }),
    )
    .await;
    let s = assert_ok(&r).clone();
    assert_eq!(s["format"], "png");
    assert_eq!(s["failed_cells"], 0);
    let cells = s["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 4);
    let sizes: Vec<(u64, u64, u64, u64)> = cells
        .iter()
        .map(|c| {
            (
                c["row"].as_u64().unwrap(),
                c["column"].as_u64().unwrap(),
                c["width_px"].as_u64().unwrap(),
                c["height_px"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sizes,
        [
            (0, 0, 100, 50),
            (0, 1, 200, 100),
            (1, 0, 100, 50),
            (1, 1, 200, 100)
        ]
    );
    assert!(cells.iter().all(|c| c["ok"] == true && c["zoom"] == 1));
    let png = png_of(&r);
    assert_eq!(png.w, s["width_px"].as_u64().unwrap() as u32);
    assert_eq!(png.h, s["height_px"].as_u64().unwrap() as u32);
    for (i, colour) in [(0, RED), (1, RED), (2, BLUE), (3, BLUE)] {
        let (x, y) = xy(&s, i);
        let (w, h) = (
            cell(&s, i)["width_px"].as_u64().unwrap() as u32,
            cell(&s, i)["height_px"].as_u64().unwrap() as u32,
        );
        // The colour reaches every edge of the cell; the black square is in
        // its bottom-left corner, 10 points = 10 or 20 pixels; the hairline
        // is just outside.
        assert_eq!(png.at(x + w - 1, y), colour, "cell {i} top right");
        assert_eq!(png.at(x + w / 2, y + h / 2), colour, "cell {i} centre");
        let k = w / 100;
        assert_eq!(png.at(x, y + h - 1), BLACK, "cell {i} bottom left");
        assert_eq!(png.at(x + 10 * k - 1, y + h - 10 * k), BLACK, "cell {i}");
        assert_eq!(png.at(x + 10 * k, y + h - 1), colour, "cell {i}");
        assert_eq!(png.at(x - 1, y), HAIRLINE, "cell {i} hairline");
        assert_eq!(png.at(x + w, y + h), HAIRLINE, "cell {i} hairline");
    }
    // The corners of the sheet are white margin.
    assert_eq!(png.at(0, 0), WHITE);
    assert_eq!(png.at(png.w - 1, png.h - 1), WHITE);
    // Every cell has a line in the text, which carries what the structured
    // content carries.
    let text = text_of(&r);
    assert!(text.contains("4 cells (0 failed)"), "{text}");
    assert!(
        text.contains("row 1 column 1: 144 dpi, 200x100 px"),
        "{text}"
    );
    client.cancel().await.ok();
}

/// A cell is byte for byte the page `render_postscript` draws for the same
/// program, size and dpi.
#[tokio::test]
async fn a_cell_is_the_render_of_its_program() {
    let client = start(&[]).await;
    let program = "/Helvetica findfont 14 scalefont setfont 0.2 0.5 0.3 setrgbcolor \
                   5 20 moveto (Sheet 1) show 1 0 0 setrgbcolor 3 setlinewidth \
                   10 10 moveto 70 45 lineto stroke showpage";
    let r = call(
        &client,
        json!({"entries": [{"source": program, "width": 80, "height": 50,
                            "dpi": [72, 144, 100]}]}),
    )
    .await;
    let s = assert_ok(&r).clone();
    let sheet = png_of(&r);
    for (i, dpi) in [(0, 72), (1, 144), (2, 100)] {
        let page = call_tool(
            &client,
            "render_postscript",
            json!({"source": program, "width": 80, "height": 50, "dpi": dpi}),
        )
        .await;
        let page = png_of(&page);
        let (x, y) = xy(&s, i);
        assert_eq!(
            (page.w, page.h),
            (
                cell(&s, i)["width_px"].as_u64().unwrap() as u32,
                cell(&s, i)["height_px"].as_u64().unwrap() as u32
            )
        );
        assert!(
            sheet.region(x, y, page.w, page.h) == page.px,
            "the {dpi} dpi cell differs from the render"
        );
    }
    client.cancel().await.ok();
}

/// A zoomed cell is the render enlarged with nearest-neighbour sampling:
/// each source pixel a uniform block, the edge between black and white
/// still a hard one.
#[tokio::test]
async fn zoom_is_nearest_neighbour() {
    let client = start(&[]).await;
    // 8x8 pixels at 72 dpi: the left three columns black.
    let r = call(
        &client,
        json!({"entries": [{"source": "0 0 3 8 rectfill showpage", "width": 8, "height": 8,
                            "dpi": [72, {"dpi": 72, "zoom": 4}]}],
               "labels": false}),
    )
    .await;
    let s = assert_ok(&r).clone();
    assert_eq!(cell(&s, 1)["zoom"], 4);
    assert_eq!(cell(&s, 1)["width_px"], 8);
    let png = png_of(&r);
    let (x0, y0) = xy(&s, 0);
    let (x1, y1) = xy(&s, 1);
    for sy in 0..8 {
        for sx in 0..8 {
            let src = png.at(x0 + sx, y0 + sy);
            assert_eq!(src, if sx < 3 { BLACK } else { WHITE });
            for dy in 0..4 {
                for dx in 0..4 {
                    assert_eq!(png.at(x1 + sx * 4 + dx, y1 + sy * 4 + dy), src);
                }
            }
        }
    }
    // The hairline is around the shown size: 32 pixels.
    assert_eq!(png.at(x1 + 32, y1 + 32), HAIRLINE);
    assert!(
        text_of(&r).contains("8x8 px, shown at 4x"),
        "{}",
        text_of(&r)
    );
    client.cancel().await.ok();
}

/// Without labels and sizes the sheet is the cells, their hairlines and
/// the margins and gaps alone; with them it is taller, by the strips.
#[tokio::test]
async fn labels_and_sizes_can_be_turned_off() {
    let client = start(&[]).await;
    let args = |labels: bool, sizes: bool| {
        json!({"entries": [{"source": filled("1 0 0"), "label": "a label"},
                           {"source": filled("0 0 1")}],
               "width": 100, "height": 40, "dpi": [72, 36],
               "labels": labels, "sizes": sizes})
    };
    let bare = call(&client, args(false, false)).await;
    let s = assert_ok(&bare).clone();
    // Columns 102 and 52 wide with a 14 px gap, 12 px margins; rows 42 tall
    // with an 18 px gap.
    assert_eq!(
        (s["width_px"].as_u64(), s["height_px"].as_u64()),
        (Some(12 + 102 + 14 + 52 + 12), Some(12 + 42 + 18 + 42 + 12))
    );
    assert_eq!(xy(&s, 0), (13, 13));
    let png = png_of(&bare);
    // Nothing but white between the rows: no caption, no label.
    for x in 0..png.w {
        for y in 12 + 42..12 + 42 + 18 {
            assert_eq!(png.at(x, y), WHITE, "({x}, {y})");
        }
    }
    let captions = call(&client, args(false, true)).await;
    let c = assert_ok(&captions).clone();
    let labels = call(&client, args(true, false)).await;
    let l = assert_ok(&labels).clone();
    let both = call(&client, args(true, true)).await;
    let b = assert_ok(&both).clone();
    let height = |v: &Value| v["height_px"].as_u64().unwrap();
    // A 12 px caption strip is 16 px plus a 5 px gap; a 16 px label strip
    // is 21 px plus 5.
    assert_eq!(height(&c), height(&s) + 2 * (16 + 5));
    assert_eq!(height(&l), height(&s) + 2 * (21 + 5));
    assert_eq!(height(&b), height(&s) + 2 * (16 + 5 + 21 + 5));
    // Some dark text under the first cell when captions are on.
    let png = png_of(&captions);
    let (x, y) = xy(&c, 0);
    let ink = (y + 42..y + 42 + 16)
        .flat_map(|yy| (x..x + 100).map(move |xx| (xx, yy)))
        .filter(|&(xx, yy)| png.at(xx, yy)[0] < 100)
        .count();
    assert!(ink > 20, "caption ink: {ink}");
    client.cancel().await.ok();
}

/// A failing program is drawn as far as it got, with its error in its
/// status; the other cells are fine and so is the call.
#[tokio::test]
async fn a_failing_cell_is_drawn_as_far_as_it_got() {
    let client = start(&[]).await;
    let r = call(
        &client,
        json!({"entries": [
                  {"source": filled("0 0 1"), "label": "fine"},
                  {"source": "1 0 0 setrgbcolor 0 0 50 50 rectfill nosuchop 0 1 0 setrgbcolor clippath fill",
                   "label": "broken"}
               ],
               "width": 100, "height": 100}),
    )
    .await;
    let s = assert_ok(&r).clone();
    assert_eq!(s["failed_cells"], 1);
    assert_eq!(cell(&s, 0)["ok"], true);
    assert!(cell(&s, 0).get("error").is_none());
    let bad = cell(&s, 1);
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["error"]["code"], "postscript_error");
    let m = bad["error"]["message"].as_str().unwrap();
    assert!(m.contains("undefined") && m.contains("nosuchop"), "{m}");
    let png = png_of(&r);
    let (x, y) = xy(&s, 1);
    // The red square was drawn; the green fill after the error was not.
    assert_eq!(png.at(x + 10, y + 99), RED);
    assert_eq!(png.at(x + 90, y + 10), WHITE);
    let (x, y) = xy(&s, 0);
    assert_eq!(png.at(x + 90, y + 10), BLUE);
    // The whole envelope, the failed cell's error code included, is what
    // the declared output schema says.
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools.iter().find(|t| t.name == "review_sheet").unwrap();
    let schema = Value::Object((**t.output_schema.as_ref().unwrap()).clone());
    assert_conforms(&schema, &schema, &s, "structuredContent");
    let text = text_of(&r);
    assert!(text.contains("2 cells (1 failed)"), "{text}");
    assert!(
        text.contains("row 1 column 0: 72 dpi, 100x100 px") && text.contains("nosuchop"),
        "{text}"
    );
    client.cancel().await.ok();
}

/// A runaway cell stops at its own step limit; the cells after it render.
#[tokio::test]
async fn a_runaway_cell_hits_its_step_limit_and_the_rest_render() {
    let client = start(&[("PSCAT_MCP_MAX_STEPS", "200000")]).await;
    let r = call(
        &client,
        json!({"entries": [
                  {"source": "1 0 0 setrgbcolor 0 0 20 20 rectfill { 1 pop } loop"},
                  {"source": filled("0 0 1")}
               ],
               "width": 40, "height": 40, "dpi": [72, 144]}),
    )
    .await;
    let s = assert_ok(&r).clone();
    assert_eq!(s["failed_cells"], 2);
    for i in 0..2 {
        let e = &cell(&s, i)["error"];
        assert_eq!(e["code"], "limit_exceeded", "{s}");
        assert_eq!(e["limit"], "steps", "{s}");
    }
    let png = png_of(&r);
    for i in 2..4 {
        assert_eq!(cell(&s, i)["ok"], true, "{s}");
        let (x, y) = xy(&s, i);
        assert_eq!(png.at(x + 30, y + 2), BLUE);
    }
    // The runaway's own drawing before the loop is on its cell.
    let (x, y) = xy(&s, 0);
    assert_eq!(png.at(x + 5, y + 35), RED);
    client.cancel().await.ok();
}

/// A runaway cell stops at its share of the sheet's time, so the cells
/// after it still get theirs, and the sheet comes back within its deadline.
#[tokio::test]
async fn a_runaway_cell_hits_its_time_share_and_the_rest_render() {
    let client = start(&[("PSCAT_MCP_MAX_SHEET_TIMEOUT_MS", "4000")]).await;
    let started = Instant::now();
    let r = call(
        &client,
        json!({"entries": [
                  {"source": "{ 1 pop } loop", "label": "runaway"},
                  {"source": filled("0 0 1"), "label": "fine"},
                  {"source": "{ 1 pop } loop", "label": "runaway too"},
                  {"source": filled("1 0 0"), "label": "also fine"}
               ],
               "width": 30, "height": 30}),
    )
    .await;
    let elapsed = started.elapsed();
    let s = assert_ok(&r).clone();
    // The sheet's own deadline is 4 s (and past it the call is an error, so
    // `assert_ok` above already covers it); the margin is for a loaded runner
    // starting five workers.
    assert!(elapsed < Duration::from_secs(4 + 20), "{elapsed:?}");
    assert_eq!(s["failed_cells"], 2, "{s}");
    for i in [0, 2] {
        let e = &cell(&s, i)["error"];
        assert_eq!(e["code"], "limit_exceeded", "{s}");
        assert_eq!(e["limit"], "time", "{s}");
        assert!(
            e["message"].as_str().unwrap().contains("ms for this cell"),
            "{s}"
        );
    }
    let png = png_of(&r);
    for (i, colour) in [(1, BLUE), (3, RED)] {
        assert_eq!(cell(&s, i)["ok"], true, "{s}");
        let (x, y) = xy(&s, i);
        assert_eq!(png.at(x + 25, y + 2), colour);
    }
    client.cancel().await.ok();
}

/// The sheet over the inline output limit is refused after it is drawn,
/// with a message that says what to change.
#[tokio::test]
async fn a_sheet_over_the_output_limit_is_refused() {
    let client = start(&[("PSCAT_MCP_MAX_OUTPUT_BYTES", "2000")]).await;
    // Noise does not compress.
    let noise = "0 1 199 { /y exch def 0 1 199 { y 2 copy mul 7919 mod 7919 div \
                 setgray 1 1 rectfill } for } for showpage";
    let r = call(
        &client,
        json!({"entries": [{"source": noise, "width": 200, "height": 200}]}),
    )
    .await;
    let s = assert_fails(&r, "output_too_large");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("at most 2000 bytes"), "{m}");
    assert_eq!(s["error"]["input"]["code"], "sheet_bytes");
    client.cancel().await.ok();
}

#[tokio::test]
async fn every_budget_is_refused_before_execution() {
    let client = start(&[
        ("PSCAT_MCP_MAX_SHEET_CELLS", "4"),
        ("PSCAT_MCP_MAX_SHEET_SOURCE_BYTES", "100"),
        ("PSCAT_MCP_MAX_SOURCE_BYTES", "60"),
        ("PSCAT_MCP_MAX_SHEET_CELL_PIXELS", "50000"),
        ("PSCAT_MCP_MAX_SHEET_PIXELS", "60000"),
    ])
    .await;
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools.iter().find(|t| t.name == "review_sheet").unwrap();
    let schema = Value::Object((**t.output_schema.as_ref().unwrap()).clone());
    let entry = |w: u32, h: u32, dpi: Value| json!({"source": "showpage", "width": w, "height": h, "dpi": dpi});
    for (args, code, input, needle) in [
        (
            json!({"entries": [entry(10, 10, json!([72, 72, 72])), entry(10, 10, json!([72, 72]))]}),
            "invalid_request",
            "cells",
            "5 cells is over this server's limit of 4",
        ),
        (
            json!({"entries": [{"source": "%".repeat(55)}, {"source": "%".repeat(55)}]}),
            "source_too_large",
            "sources",
            "110 bytes in all; the limit is 100",
        ),
        (
            json!({"entries": [{"source": "%".repeat(61)}]}),
            "source_too_large",
            "source",
            "entries[0].source is 61 bytes",
        ),
        (
            json!({"entries": [entry(8000, 10, json!([73]))]}),
            "invalid_dimensions",
            "cell_side",
            "a cell is at most 8000 pixels on a side",
        ),
        (
            json!({"entries": [entry(200, 200, json!([72, 72]))]}),
            "too_many_pixels",
            "cell_pixels",
            "over 50000 pixels in all",
        ),
        (
            json!({"entries": [entry(100, 100, json!([{"dpi": 72, "zoom": 3}]))]}),
            "too_many_pixels",
            "sheet_pixels",
            "this server draws at most 60000 pixels",
        ),
        (
            json!({"entries": [entry(2700, 1, json!([72, 72, 72]))], "labels": false}),
            "invalid_dimensions",
            "sheet_side",
            "over the 8000px-per-side limit",
        ),
    ] {
        let started = Instant::now();
        let r = call(&client, args.clone()).await;
        let s = assert_fails(&r, code);
        assert_conforms(&schema, &schema, &s, "structuredContent");
        assert_eq!(s["error"]["input"]["code"], input, "{args}: {s}");
        let m = s["error"]["message"].as_str().unwrap();
        assert!(m.contains(needle), "{args}: {m}");
        // Rejected before any cell runs; the bound only catches a run.
        assert!(started.elapsed() < Duration::from_secs(15));
    }
    // A sheet within every budget is drawn.
    let r = call(
        &client,
        json!({"entries": [entry(50, 50, json!([72, 36]))]}),
    )
    .await;
    assert_ok(&r);
    client.cancel().await.ok();

    // A deadline too short for every cell to have its 100 ms: 1000 ms less
    // the 250 ms render margin is 750 ms, less 250 ms to draw the labels,
    // for 6 cells. (On a server of its own: one that short is not asked to
    // draw anything.)
    let client = start(&[("PSCAT_MCP_MAX_SHEET_TIMEOUT_MS", "1000")]).await;
    let r = call(
        &client,
        json!({"entries": [entry(10, 10, json!([72, 72, 72, 72, 72, 72]))]}),
    )
    .await;
    let s = assert_fails(&r, "invalid_request");
    assert_eq!(s["error"]["input"]["code"], "deadline", "{s}");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("6 cells cannot each have 100 ms"), "{m}");
    client.cancel().await.ok();
}

/// No refused sheet starts a worker: the service's counters.
#[tokio::test]
async fn refused_sheets_execute_nothing() {
    let limits = LimitsConfig {
        max_sheet_cells: 2,
        max_sheet_pixels: Some(20_000),
        ..LimitsConfig::default()
    };
    let service: Arc<RenderService> = pscat_mcp::build_service(
        limits,
        &WorkerConfig {
            worker_exe: Some(env!("CARGO_BIN_EXE_pscat-mcp-server").into()),
        },
    )
    .expect("service");
    let sheet = |v: Value| GenerateRequest {
        job: GenerateJob::ReviewSheet(serde_json::from_value(v).unwrap()),
        format: None,
    };
    for (v, code) in [
        (
            json!({"entries": [{"source": "showpage", "dpi": [9, 9, 9]}]}),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"entries": [{"source": "showpage", "width": 200, "height": 200}]}),
            ErrorCode::TooManyPixels,
        ),
        (json!({"entries": []}), ErrorCode::InvalidRequest),
        (
            json!({"entries": [{"source": "showpage", "dpi": [{"dpi": 9, "zoom": 40}]}]}),
            ErrorCode::InvalidRequest,
        ),
    ] {
        let err = service.generate(sheet(v)).await.unwrap_err();
        assert_eq!(err.code, code, "{err}");
    }
    assert_eq!(service.counters.executed.load(Ordering::SeqCst), 0);
    assert_eq!(service.counters.rejected.load(Ordering::SeqCst), 4);
    let ok = service
        .generate(sheet(
            json!({"entries": [{"source": "showpage", "width": 30, "height": 30}]}),
        ))
        .await
        .unwrap();
    assert_eq!(ok.sheet.unwrap().cells.len(), 1);
    assert_eq!(service.counters.executed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn argument_errors_use_the_envelope() {
    let client = start(&[]).await;
    for (args, code, needle) in [
        (json!({}), "invalid_request", "invalid arguments"),
        (
            json!({"entries": "x"}),
            "invalid_request",
            "invalid arguments",
        ),
        (
            json!({"entries": [{"label": "no source"}]}),
            "invalid_request",
            "invalid arguments",
        ),
        (
            json!({"entries": [{"source": "showpage", "width": -1}]}),
            "invalid_request",
            "invalid arguments",
        ),
        (
            json!({"entries": [{"source": "showpage"}], "labels": "yes"}),
            "invalid_request",
            "invalid arguments",
        ),
        (
            json!({"entries": []}),
            "invalid_request",
            "entries must be a non-empty list",
        ),
        (
            json!({"entries": [{"source": "showpage", "dpi": 72}]}),
            "invalid_request",
            "entries[0].dpi must be a non-empty list of cells",
        ),
        (
            json!({"entries": [{"source": "showpage", "dpi": [5000]}]}),
            "invalid_dimensions",
            "a dpi must be a number from 9 to 1200",
        ),
        (
            json!({"entries": [{"source": "showpage", "dpi": [{"dpi": 9, "zoom": 32}]}]}),
            "invalid_request",
            "zoom must be a whole number from 2 to 16",
        ),
        (
            json!({"entries": [{"source": "showpage", "label": "x".repeat(201)}]}),
            "invalid_request",
            "label is over 200 characters",
        ),
        (
            json!({"entries": [{"source": "showpage"}], "width": 9000}),
            "invalid_dimensions",
            "width must be a whole number from 1 to 8000",
        ),
    ] {
        let r = call(&client, args.clone()).await;
        let s = assert_fails(&r, code);
        let m = s["error"]["message"].as_str().unwrap();
        assert!(m.contains(needle), "{args}: {m}");
    }
    // The server keeps serving.
    let r = call(
        &client,
        json!({"entries": [{"source": "showpage", "width": 20, "height": 20}]}),
    )
    .await;
    assert_ok(&r);
    client.cancel().await.ok();
}

/// The most a reply can carry, 32 failed cells whose reports quote long
/// names, under 200-character labels, stays under 24 KB as text and as
/// structured content.
#[tokio::test]
async fn the_reply_stays_small_with_every_cell_failing() {
    let client = start(&[]).await;
    // An undefined name of 6000 characters, quotes and backslashes among
    // them: the report quotes it.
    let source = "\u{e9}\"\\".repeat(2000);
    let entries: Vec<Value> = (0..8)
        .map(|i| {
            json!({"source": source, "label": format!("{i}{}", "\u{d55c}".repeat(199)),
                   "width": 20, "height": 20, "dpi": [72, 36, 18, {"dpi": 9, "zoom": 2}]})
        })
        .collect();
    let r = call(&client, json!({"entries": entries})).await;
    let s = assert_ok(&r).clone();
    assert_eq!(s["failed_cells"], 32, "{s}");
    let structured_len = serde_json::to_string(&s).unwrap().len();
    let text_len = text_of(&r).len();
    eprintln!("structured {structured_len} bytes, text {text_len} bytes");
    assert!(structured_len < 24 * 1024, "structured: {structured_len}");
    assert!(text_len < 24 * 1024, "text: {text_len}");
    let m = cell(&s, 31)["error"]["message"].as_str().unwrap();
    assert!(m.contains("undefined") && m.ends_with('…'), "{m}");
    client.cancel().await.ok();
}

/// Odd programs and labels give a sheet or a structured error, never a
/// crash, and the server serves the next call.
#[tokio::test]
async fn odd_inputs_never_crash() {
    let client = start(&[]).await;
    for args in [
        json!({"entries": [{"source": ""}], "width": 10, "height": 10}),
        json!({"entries": [{"source": "showpage showpage showpage", "label": ""}], "width": 10, "height": 10}),
        json!({"entries": [{"source": "<< /PageSize [300 300] >> setpagedevice 0 0 5 5 rectfill",
                            "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "(lib/artkit.ps) run showpage", "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "(/etc/passwd) (r) file", "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "showpage", "label": "a\u{0}b\u{1b}c (paren) \\ \u{1F600}",
                            "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "showpage", "label": "%%EOF ) show quit (", "width": 10,
                            "height": 10}]}),
        json!({"entries": [{"source": "{ dup } loop", "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "/a { a 1 } def a", "width": 10, "height": 10}]}),
        json!({"entries": [{"source": "showpage", "width": 1, "height": 1,
                            "dpi": [9, {"dpi": 9, "zoom": 16}]}]}),
    ] {
        let r = call(&client, args.clone()).await;
        if r.is_error == Some(true) {
            let code = structured(&r)["error"]["code"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                ["invalid_request", "postscript_error", "limit_exceeded"].contains(&code.as_str()),
                "{args}: {code}"
            );
        } else {
            let s = structured(&r);
            assert_eq!(s["ok"], true, "{args}");
            let _ = png_of(&r);
        }
    }
    let r = call(
        &client,
        json!({"entries": [{"source": "showpage", "width": 20, "height": 20}]}),
    )
    .await;
    assert_ok(&r);
    client.cancel().await.ok();
}

/// A backend that answers every job as a sheet that ran out of its own time
/// in the worker would: the worker's `timeout` with `error.input.code`
/// `deadline`. Such a sheet needs a cell PSCat cannot interrupt, so a real
/// worker does not produce it on demand.
struct OutOfTime;

impl pscat_mcp::backend::RenderBackend for OutOfTime {
    fn execute(
        &self,
        job: pscat_mcp::worker::WorkerJob,
        _deadline: Duration,
    ) -> pscat_mcp::backend::BackendFuture<'_> {
        assert!(job.sheet.is_some(), "a sheet job");
        // What the worker writes on its stdout for such a sheet.
        let result: pscat_mcp::worker::WorkerResult = serde_json::from_value(json!({
            "failure": {"kind": "generate", "code": "timeout", "detail": "deadline",
                        "message": "the sheet was not finished within this server's 18000 ms \
                                    for a sheet (3 of 4 cells drawn). Use fewer or cheaper cells"},
            "ps_error": null, "artifacts": [], "page_count": 0, "syntax": null, "lint": [],
            "program_output": "", "program_output_truncated": false, "steps_used": 0,
            "elapsed_ms": 0
        }))
        .expect("a worker result");
        Box::pin(async { Ok(result) })
    }
}

/// The worker's own sheet deadline is a `timeout` through the envelope,
/// valid against the declared output schema, and counted as a timed-out
/// job.
#[tokio::test]
async fn the_in_worker_deadline_is_a_timeout_in_the_envelope() {
    let service = Arc::new(RenderService::new(
        Arc::new(OutOfTime),
        LimitsConfig::default(),
    ));
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let server = pscat_mcp::mcp::PscatMcp::new(service.clone());
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client: Client = ().serve(client_io).await.expect("initialize");
    let r = call(
        &client,
        json!({"entries": [{"source": "showpage", "width": 10, "height": 10,
                            "dpi": [72, 72, 72, 72]}]}),
    )
    .await;
    let s = assert_fails(&r, "timeout");
    assert_eq!(s["error"]["input"]["code"], "deadline", "{s}");
    assert!(
        s["error"]["message"]
            .as_str()
            .unwrap()
            .contains("3 of 4 cells drawn"),
        "{s}"
    );
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools.iter().find(|t| t.name == "review_sheet").unwrap();
    let schema = Value::Object((**t.output_schema.as_ref().unwrap()).clone());
    assert_conforms(&schema, &schema, &s, "structuredContent");
    assert_eq!(service.counters.timed_out.load(Ordering::SeqCst), 1);
    assert_eq!(service.counters.executed.load(Ordering::SeqCst), 1);
    client.cancel().await.ok();
}

/// A sheet at the pixel limit holding one cell enlarged 16 times: at the
/// default limit (16,000,000 pixels) and at the most a sheet can be
/// (8000 x 8000, the canvas alone 256 MB). The cell is an exact enlargement.
#[tokio::test]
async fn a_sheet_at_the_pixel_limit_with_a_zoom_16_cell() {
    // 248 x 16 = 3968 pixels shown; with the hairline and the margins the
    // sheet is 3994 x 3994 = 15,952,036 pixels.
    for (env, side_pt, sheet_px) in [
        (vec![], 248u32, 3994u32),
        (
            vec![
                ("PSCAT_MCP_MAX_SHEET_PIXELS", "64000000"),
                ("PSCAT_MCP_MAX_SHEET_TIMEOUT_MS", "60000"),
            ],
            498,
            7994,
        ),
    ] {
        let client = start(&env).await;
        let r = call(
            &client,
            json!({"entries": [{"source": "1 0 0 setrgbcolor 0 0 1 1 rectfill \
                                           0 0 1 setrgbcolor 100 100 50 50 rectfill showpage",
                                "width": side_pt, "height": side_pt,
                                "dpi": [{"dpi": 72, "zoom": 16}]}],
                   "labels": false, "sizes": false}),
        )
        .await;
        let s = assert_ok(&r).clone();
        assert_eq!(s["width_px"], sheet_px, "{s}");
        assert_eq!(s["height_px"], sheet_px, "{s}");
        let png = png_of(&r);
        assert_eq!((png.w, png.h), (sheet_px, sheet_px));
        let (x, y) = xy(&s, 0);
        let h = side_pt * 16;
        // The one red pixel in the bottom-left corner is a 16x16 block.
        assert_eq!(png.at(x, y + h - 1), RED);
        assert_eq!(png.at(x + 15, y + h - 16), RED);
        assert_eq!(png.at(x + 16, y + h - 1), WHITE);
        assert_eq!(png.at(x, y + h - 17), WHITE);
        // The blue square, 50 points from 100: pixels 1600 to 2399.
        assert_eq!(png.at(x + 1600, y + h - 1601), BLUE);
        assert_eq!(png.at(x + 1599, y + h - 1601), WHITE);
        assert_eq!(png.at(x + h, y + h), HAIRLINE);
        client.cancel().await.ok();
    }
}

/// A cell's memory limit counts only what its own program allocates: the
/// sheet already drawn around it (here 61 MB) is not charged to it. So a
/// program that fits the limit alone fits it on a large sheet, one that
/// does not fails the same way, and the cell is drawn the same.
#[tokio::test]
async fn a_cells_memory_limit_is_the_same_alone_and_on_a_large_sheet() {
    // 64 MiB is 67,108,864 bytes.
    let client = start(&[("PSCAT_MCP_MAX_MEMORY_BYTES", "67108864")]).await;
    // An array element is about 40 bytes: 1,500,000 of them are about
    // 60 MB, within the limit; 1,800,000 (about 72 MB) are not.
    let fits = "/a 1500000 array def 1 0 0 setrgbcolor 0 0 10 10 rectfill showpage";
    let over = "1 0 0 setrgbcolor 0 0 10 10 rectfill /a 1800000 array def showpage";
    for (source, ok) in [(fits, true), (over, false)] {
        let entry = json!({"source": source, "width": 20, "height": 20});
        // Alone, as a page: the render's own verdict.
        let page = call_tool(
            &client,
            "render_postscript",
            json!({"source": source, "width": 20, "height": 20}),
        )
        .await;
        assert_eq!(page.is_error, Some(!ok), "{}", text_of(&page));
        if !ok {
            assert_eq!(structured(&page)["error"]["postscript"]["limit"], "memory");
        }
        // Alone on a sheet, and first on a sheet of 3966 x 3926 pixels
        // whose canvas is allocated before the cell runs.
        let small = call(
            &client,
            json!({"entries": [entry], "labels": false, "sizes": false}),
        )
        .await;
        let large = call(
            &client,
            json!({"entries": [entry, {"source": "", "width": 3900, "height": 3900}],
                   "labels": false, "sizes": false}),
        )
        .await;
        let mut drawn = Vec::new();
        for r in [&small, &large] {
            let s = assert_ok(r).clone();
            let c = cell(&s, 0);
            assert_eq!(c["ok"], ok, "{s}");
            if !ok {
                assert_eq!(c["error"]["limit"], "memory", "{s}");
            }
            let (x, y) = xy(&s, 0);
            drawn.push(png_of(r).region(x, y, 20, 20));
        }
        assert_eq!(structured(&large)["width_px"], 3926);
        assert!(drawn[0] == drawn[1], "the cell differs on the large sheet");
        assert_eq!(drawn[0][19 * 20], RED, "the cell's square");
    }
    client.cancel().await.ok();
}
