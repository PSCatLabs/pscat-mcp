//! `handwrite` end to end: a real rmcp client on the real server
//! binary over stdio, real PSCat in the worker.

use std::time::Duration;

use base64::Engine as _;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
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
    let params = CallToolRequestParams::new("handwrite").with_arguments(args);
    tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .expect("handwrite timed out")
        .expect("handwrite failed at the protocol level")
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

/// The base64 of the first image block, to compare two notes byte for byte.
fn image_data(r: &CallToolResult) -> String {
    r.content
        .iter()
        .find_map(|c| c.as_image().map(|i| i.data.to_string()))
        .expect("an image block")
}

/// Pixels close to the default ink, (0.13, 0.14, 0.34) = (33, 36, 87).
fn default_ink(p: &[u8; 4]) -> bool {
    p[0].abs_diff(33) < 60 && p[1].abs_diff(36) < 60 && p[2].abs_diff(87) < 70
}

/// The most pale blue-grey pixels in one row: a ruled line is hundreds, the
/// antialiased edge of ink a few.
fn longest_rule(w: u32, px: &[[u8; 4]]) -> usize {
    px.chunks(w as usize)
        .map(|row| {
            row.iter()
                .filter(|p| p[2] > p[0].saturating_add(15) && p[0] > 150 && p[0] < 245)
                .count()
        })
        .max()
        .unwrap_or(0)
}

#[tokio::test]
async fn the_tool_is_listed_with_the_plugins_arguments() {
    let client = start(&[]).await;
    let tools = client.list_all_tools().await.expect("list_tools");
    let t = tools
        .iter()
        .find(|t| t.name == "handwrite")
        .expect("handwrite listed");
    let props = t.input_schema["properties"].as_object().unwrap();
    let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "dpi", "ink", "jitter", "paper", "seed", "size", "text", "width"
        ]
    );
    assert_eq!(t.input_schema["required"], json!(["text"]));
    assert_eq!(props["paper"]["enum"], json!(["plain", "ruled"]));
    let d = t.description.as_deref().unwrap();
    assert!(d.contains("not for imitating a specific person's"), "{d}");
    assert!(d.contains("text up to 65536 bytes"), "{d}");
    assert!(!d.contains("out_path"), "{d}");
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    assert_eq!(
        info.structured_content.unwrap()["limits"]["max_handwrite_bytes"],
        65_536
    );
    client.cancel().await.ok();
}

/// The default note: 612 points wide, 146 tall for one line, white paper and
/// dark blue ink.
#[tokio::test]
async fn writes_a_note_in_the_default_ink() {
    let client = start(&[]).await;
    let r = call(&client, json!({"text": "Hello there, friend!"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let s = structured(&r);
    assert_eq!(s["ok"], true);
    assert_eq!(s["format"], "png");
    assert_eq!(s["page_count"], 1);
    assert_eq!(s["artifacts"][0]["width_px"], 612);
    assert_eq!(s["artifacts"][0]["height_px"], 146);
    let (w, h, px) = png_of(&r);
    assert_eq!((w, h), (612, 146));
    assert_eq!(px[0], [255, 255, 255, 255], "plain white paper");
    let ink = px.iter().filter(|p| default_ink(p)).count();
    assert!(ink > 120, "only {ink} ink pixels");
    assert!(longest_rule(w, &px) < 60, "plain paper has no rules");

    // Two lines: 54 points taller. Capitals are written as lowercase.
    let r = call(&client, json!({"text": "ONE\ntwo"})).await;
    assert_eq!(png_of(&r).1, 146 + 54);
    // The same words in capitals or not make the same note.
    let a = call(&client, json!({"text": "hello"})).await;
    let b = call(&client, json!({"text": "HELLO"})).await;
    assert_eq!(image_data(&a), image_data(&b));
    client.cancel().await.ok();
}

#[tokio::test]
async fn ink_size_width_dpi_and_paper_are_applied() {
    let client = start(&[]).await;
    let r = call(
        &client,
        json!({"text": "red ink", "ink": "0.9,0.1,0.1", "width": 300, "size": 24, "dpi": 144,
               "paper": "ruled", "jitter": 0, "seed": -7}),
    )
    .await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    let (w, h, px) = png_of(&r);
    // 300 pt at 144 dpi = 600 px; 2*54 + 1.05*24 = 133.2 -> 134 pt = 268 px.
    assert_eq!((w, h), (600, 268));
    let red = px
        .iter()
        .filter(|p| p[0] > 190 && p[1] < 110 && p[2] < 110)
        .count();
    let blue_ink = px.iter().filter(|p| default_ink(p)).count();
    assert!(red > 200, "only {red} red ink pixels");
    assert!(blue_ink < 30, "the default ink was used: {blue_ink} pixels");
    // Ruled paper: long pale-blue lines, and the red margin rule's column.
    assert!(longest_rule(w, &px) > 300, "no ruled lines");
    let margin_col = (0..h as usize)
        .filter(|y| {
            let p = px[y * w as usize + (54.0 * 0.8 * 2.0) as usize];
            p[0] > 220 && p[1] < 215 && p[2] < 215
        })
        .count();
    assert!(margin_col > 150, "margin rule: {margin_col} pixels");

    // Plain paper, same arguments: no rules.
    let r = call(
        &client,
        json!({"text": "red ink", "width": 300, "size": 24, "dpi": 144, "paper": "plain"}),
    )
    .await;
    let (w, _, px) = png_of(&r);
    assert!(longest_rule(w, &px) < 60, "plain paper has no rules");
    client.cancel().await.ok();
}

#[tokio::test]
async fn the_same_seed_gives_the_same_bytes() {
    let client = start(&[]).await;
    let args = json!({"text": "same again", "seed": 42, "jitter": 30, "paper": "ruled"});
    let a = call(&client, args.clone()).await;
    let b = call(&client, args).await;
    assert_eq!(image_data(&a), image_data(&b));
    let other = call(
        &client,
        json!({"text": "same again", "seed": 43, "jitter": 30, "paper": "ruled"}),
    )
    .await;
    assert_ne!(image_data(&a), image_data(&other));
    // The default seed is 1985.
    let d = call(&client, json!({"text": "same again"})).await;
    let e = call(&client, json!({"text": "same again", "seed": 1985})).await;
    assert_eq!(image_data(&d), image_data(&e));
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_whole_size_is_the_same_as_a_real_one() {
    let client = start(&[]).await;
    let a = call(&client, json!({"text": "size", "size": 36})).await;
    let b = call(&client, json!({"text": "size", "size": 36.0})).await;
    assert_eq!(image_data(&a), image_data(&b));
    client.cancel().await.ok();
}

#[tokio::test]
async fn text_over_the_limit_is_refused_before_a_worker_starts() {
    let client = start(&[("PSCAT_MCP_MAX_HANDWRITE_BYTES", "64")]).await;
    let r = call(&client, json!({"text": "a ".repeat(40)})).await;
    let s = assert_fails(&r, "source_too_large");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("80 bytes") && m.contains("64 bytes"), "{m}");
    let ok = call(&client, json!({"text": "a ".repeat(32)})).await;
    assert_eq!(ok.is_error, Some(false), "{}", text_of(&ok));
    let info = client
        .call_tool(CallToolRequestParams::new("get_renderer_info"))
        .await
        .unwrap();
    assert_eq!(
        info.structured_content.unwrap()["limits"]["max_handwrite_bytes"],
        64
    );
    client.cancel().await.ok();
}

/// A note too tall for the pixel cap is refused after its height is known
/// and before it is drawn, with a message that says why. The server then
/// serves the next request.
#[tokio::test]
async fn a_note_too_tall_for_the_pixel_cap_is_a_structured_error() {
    let client = start(&[]).await;
    // 4000 points wide at 72 dpi: 16 M pixels allow 4000 rows, and 100 lines
    // are 5492 points tall.
    let tall = "line\n".repeat(100);
    let started = std::time::Instant::now();
    let r = call(&client, json!({"text": tall, "width": 4000})).await;
    let s = assert_fails(&r, "too_many_pixels");
    assert!(started.elapsed() < Duration::from_secs(15));
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("100 lines") && m.contains("4000x5492"), "{m}");
    assert!(m.contains("16000000 pixels"), "{m}");
    assert_eq!(s["error"]["input"]["code"], "height");

    // The side limit comes first when the note is over it too: at the
    // default width, 160 lines are 8732 pixels.
    let r = call(&client, json!({"text": "line\n".repeat(160)})).await;
    let s = assert_fails(&r, "invalid_dimensions");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("612x8732") && m.contains("8000 pixels a side"),
        "{m}"
    );

    // The same note at a lower size fits.
    let r = call(&client, json!({"text": "line\n".repeat(160), "size": 20})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));

    // A tall note at a low resolution is under the pixel cap but a page is
    // at most 8000 points.
    let r = call(&client, json!({"text": "line\n".repeat(160), "dpi": 36})).await;
    let s = assert_fails(&r, "invalid_dimensions");
    assert!(
        s["error"]["message"]
            .as_str()
            .unwrap()
            .contains("8000 points"),
        "{s}"
    );

    // The server is still serving.
    let r = call(&client, json!({"text": "still here"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

/// With the text limit raised to 1 MiB, a text can be a million lines. It is
/// refused with the same structured error: not an out-of-memory failure,
/// not a deadline kill.
#[tokio::test]
async fn a_text_of_half_a_million_lines_is_refused_not_killed() {
    let client = start(&[("PSCAT_MCP_MAX_HANDWRITE_BYTES", "1048576")]).await;
    let text = "a\n".repeat(500_000);
    let r = call(&client, json!({"text": text, "width": 8000})).await;
    let s = assert_fails(&r, "invalid_dimensions");
    let m = s["error"]["message"].as_str().unwrap();
    assert!(m.contains("500000 lines"), "{m}");
    let r = call(&client, json!({"text": "after the flood"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

#[tokio::test]
async fn the_measuring_pass_runs_under_the_interpreter_limits() {
    let client = start(&[("PSCAT_MCP_MAX_STEPS", "2000")]).await;
    let r = call(&client, json!({"text": "this will not get far at all"})).await;
    let s = assert_fails(&r, "limit_exceeded");
    assert_eq!(s["error"]["postscript"]["limit"], "steps", "{s}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn argument_errors_use_the_envelope() {
    let client = start(&[]).await;
    for (args, code, needle) in [
        (
            json!({"text": "hi", "size": "big"}),
            "invalid_request",
            "invalid arguments",
        ),
        (json!({"size": 36}), "invalid_request", "invalid arguments"),
        (
            json!({"text": "hi", "size": 5000}),
            "invalid_request",
            "size must be",
        ),
        (
            json!({"text": "hi", "jitter": -1}),
            "invalid_request",
            "jitter must be",
        ),
        (
            json!({"text": "hi", "seed": 5_000_000_000_i64}),
            "invalid_request",
            "seed must be",
        ),
        (
            json!({"text": "hi", "paper": "lined"}),
            "invalid_request",
            "plain or ruled",
        ),
        (
            json!({"text": "hi", "ink": "255,0,0"}),
            "invalid_request",
            "ink must be R,G,B",
        ),
        (
            json!({"text": "hi", "ink": "1,2,3 ] def"}),
            "invalid_request",
            "ink must be R,G,B",
        ),
        (
            json!({"text": "hi", "dpi": 5}),
            "invalid_dimensions",
            "dpi must be",
        ),
        (
            json!({"text": "hi", "dpi": 5000}),
            "invalid_dimensions",
            "dpi must be",
        ),
        (
            json!({"text": "hi", "width": 0}),
            "invalid_dimensions",
            "width must be",
        ),
        (
            json!({"text": "hi", "width": 9000}),
            "invalid_dimensions",
            "width must be",
        ),
        (
            json!({"text": "hi", "width": 8000, "dpi": 72.5}),
            "invalid_dimensions",
            "8000 pixels a side",
        ),
        (
            json!({"text": "hi", "width": 8000, "dpi": 1200}),
            "invalid_dimensions",
            "pixels a side",
        ),
    ] {
        let r = call(&client, args.clone()).await;
        let s = assert_fails(&r, code);
        let m = s["error"]["message"].as_str().unwrap();
        assert!(m.contains(needle), "{args}: {m}");
    }
    client.cancel().await.ok();
}

/// Text that is empty, only whitespace, or outside the script's characters
/// gives a note or a structured error, never a crash; the next call works.
#[tokio::test]
async fn unusual_text_never_crashes() {
    let client = start(&[]).await;
    for text in ["", "   ", "\n\n\n", "\r\n\t\r\n"] {
        let r = call(&client, json!({"text": text})).await;
        let s = assert_fails(&r, "invalid_request");
        assert!(
            s["error"]["message"].as_str().unwrap().contains("empty"),
            "{s}"
        );
    }
    for text in ["你好，世界", "🙂🙂🙂", "ñ ü é", "—:;()[]{}"] {
        let r = call(&client, json!({"text": text})).await;
        let s = assert_fails(&r, "invalid_request");
        assert!(
            s["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no character"),
            "{s}"
        );
    }
    // A mix writes what it can and leaves a gap for the rest.
    for text in [
        "hi 你好 there",
        "hello 🙂 world",
        "a\u{0}b",
        "tabs\tand\x0bform\x0cfeeds\x1b",
        "(unbalanced \\ ) ( parens \\(",
        "-leading dash",
        "%%EOF showpage quit",
        &"unbroken".repeat(2000),
        &"word ".repeat(200),
        "ALL CAPS, AND DIGITS 0123456789 .,-'!?",
    ] {
        let r = call(&client, json!({"text": text})).await;
        assert_eq!(r.is_error, Some(false), "{:?}: {}", text, text_of(&r));
        let (w, _, px) = png_of(&r);
        assert_eq!(w, 612);
        assert!(px.iter().any(default_ink), "{text:?} drew nothing");
    }
    let r = call(&client, json!({"text": "fine"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

/// A word wider than the column overflows rather than being split; a very
/// narrow page puts one word on each line.
#[tokio::test]
async fn narrow_pages_wrap_a_word_to_a_line() {
    let client = start(&[]).await;
    let r = call(&client, json!({"text": "one two three four", "width": 100})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    assert_eq!(png_of(&r).1, 146 + 3 * 54);
    client.cancel().await.ok();
}

/// The ends of every range give a note or a structured error, never a crash.
#[tokio::test]
async fn extreme_arguments_never_crash() {
    let client = start(&[]).await;
    for args in [
        json!({"text": "zero", "size": 0}),
        json!({"text": "tiny", "size": 0.001}),
        json!({"text": "huge", "size": 2000}),
        json!({"text": "narrow", "width": 1}),
        json!({"text": "wild", "jitter": 10000}),
        json!({"text": "calm", "jitter": 0}),
        json!({"text": "low", "dpi": 9}),
        json!({"text": "high", "dpi": 1200, "width": 6}),
        json!({"text": "seeds", "seed": 2147483647_i64}),
        json!({"text": "seeds", "seed": -2147483648_i64}),
        json!({"text": "ink", "ink": "0,0,0"}),
        json!({"text": "ink", "ink": "1,1,1"}),
    ] {
        let r = call(&client, args.clone()).await;
        if r.is_error == Some(true) {
            let code = error_code(&r).to_string();
            assert!(
                [
                    "invalid_request",
                    "invalid_dimensions",
                    "too_many_pixels",
                    "postscript_error"
                ]
                .contains(&code.as_str()),
                "{args}: {code}"
            );
        } else {
            let _ = png_of(&r);
        }
    }
    let r = call(&client, json!({"text": "after"})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    client.cancel().await.ok();
}

/// A limit hit inside the layout of a huge text is reported without the text.
#[tokio::test]
async fn a_limit_hit_in_the_layout_of_a_huge_word_keeps_the_reply_small() {
    let client = start(&[
        ("PSCAT_MCP_MAX_STEPS", "1000000"),
        ("PSCAT_MCP_MAX_HANDWRITE_BYTES", "1048576"),
    ])
    .await;
    let r = call(&client, json!({"text": "x".repeat(1_000_000)})).await;
    let s = assert_fails(&r, "limit_exceeded");
    let structured_len = serde_json::to_string(&s).unwrap().len();
    let text_len = text_of(&r).len();
    assert!(structured_len < 24 * 1024, "structured: {structured_len}");
    assert!(text_len < 24 * 1024, "text: {text_len}");
    client.cancel().await.ok();
}
