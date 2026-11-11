//! The PSCat plugin's skills as MCP resources and prompts (`src/guide.rs`):
//! the vendored copy matches the pinned PSCat revision, and an rmcp client
//! over stdio lists and reads them. `tests/http.rs` repeats the protocol
//! checks over Streamable HTTP.
//!
//! Also checks that the guidance works on this server: every runnable
//! `postscript` block in the guides renders, and every library the guides
//! name loads in the worker.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pscat_mcp::guide;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, ErrorCode, GetPromptRequestParams, ReadResourceRequestParams,
    ResourceContents,
};
use rmcp::service::{RunningService, ServiceError};
use serde_json::{Value, json};

mod common;

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The response-size guidance for anything one request returns.
const MAX_REPLY_BYTES: usize = 24 * 1024;
/// The prompts the server offers.
const PROMPTS: &[&str] = &["chart", "diagram", "generative_art"];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn vendored(path: &str) -> String {
    let p = repo().join("guide/skills").join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

async fn start() -> Client {
    common::start_stdio(env!("CARGO_BIN_EXE_pscat-mcp-server"), &[]).await
}

fn mcp_code(e: ServiceError) -> i32 {
    match e {
        ServiceError::McpError(d) => d.code.0,
        other => panic!("not an MCP error: {other}"),
    }
}

// ---------------------------------------------------------------- the copy

fn lock_commit() -> String {
    let lock = std::fs::read_to_string(repo().join("Cargo.lock")).unwrap();
    let entry = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"pscat\"\n"))
        .expect("pscat in Cargo.lock");
    let source = entry
        .lines()
        .find_map(|l| l.strip_prefix("source = \""))
        .expect("pscat source");
    source
        .trim_end_matches('"')
        .rsplit('#')
        .next()
        .unwrap()
        .to_string()
}

fn toml_tag() -> String {
    let toml = std::fs::read_to_string(repo().join("Cargo.toml")).unwrap();
    let line = toml
        .lines()
        .find(|l| l.starts_with("pscat = { git = "))
        .expect("pscat dependency line");
    line.split("tag = \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string()
}

/// The pinned PSCat checkout. Cargo keeps a git dependency in
/// `$CARGO_HOME/git/checkouts/<name>-<hash>/<first 7 of the commit>`, so it
/// is looked up there first; `cargo metadata` is the fallback, because it
/// can wait a long time on the shared package-cache lock while another
/// cargo runs.
fn pscat_checkout() -> PathBuf {
    let commit = lock_commit();
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    if let Some(home) = home
        && let Ok(entries) = std::fs::read_dir(home.join("git/checkouts"))
    {
        for e in entries.flatten() {
            let dir = e.path().join(&commit[..7]);
            if e.file_name().to_string_lossy().starts_with("pscat-")
                && dir.join("plugin/skills").is_dir()
                && std::fs::read_to_string(dir.join("Cargo.toml"))
                    .is_ok_and(|t| t.contains("name = \"pscat\""))
            {
                return dir;
            }
        }
    }
    let out = std::process::Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(repo())
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Value = serde_json::from_slice(&out.stdout).expect("metadata JSON");
    let pkg = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "pscat")
        .expect("pscat package");
    assert!(
        pkg["source"].as_str().is_some_and(|s| s.ends_with(&commit)),
        "cargo resolves pscat to {}, not {commit}",
        pkg["source"]
    );
    Path::new(pkg["manifest_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf()
}

fn files_under(dir: &Path) -> BTreeSet<String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn source_record_matches_the_pin() {
    let src = guide::source();
    assert_eq!(
        src.commit,
        lock_commit(),
        "guide/ is from another PSCat commit than Cargo.lock; run scripts/vendor-guide.py"
    );
    assert_eq!(src.tag, toml_tag());
    assert_eq!(src.path, "plugin/skills");
    assert_eq!(src.license, "MIT OR Apache-2.0");
    let listed: BTreeSet<String> = src.files.iter().cloned().collect();
    assert_eq!(listed, files_under(&repo().join("guide/skills")));
}

/// Byte for byte the pinned revision's `plugin/skills/` and licence files.
#[test]
fn vendored_files_are_the_pinned_revision() {
    let checkout = pscat_checkout();
    let theirs = checkout.join("plugin/skills");
    let ours = repo().join("guide/skills");
    let names = files_under(&theirs);
    assert_eq!(
        names,
        files_under(&ours),
        "file sets differ; run scripts/vendor-guide.py"
    );
    for name in &names {
        assert_eq!(
            std::fs::read(theirs.join(name)).unwrap(),
            std::fs::read(ours.join(name)).unwrap(),
            "{name} differs from {}; run scripts/vendor-guide.py",
            theirs.display()
        );
    }
    for lic in ["LICENSE-MIT", "LICENSE-APACHE"] {
        assert_eq!(
            std::fs::read(checkout.join(lic)).unwrap(),
            std::fs::read(repo().join("guide").join(lic)).unwrap(),
            "guide/{lic}"
        );
    }
    // The binary embeds the same files.
    let embedded: BTreeSet<String> = guide::files().iter().map(|(p, _)| p.to_string()).collect();
    assert_eq!(embedded, names);
    for (path, text) in guide::files() {
        assert_eq!(*text, vendored(path), "embedded {path}");
    }
}

// ------------------------------------------------------ resources over stdio

#[tokio::test]
async fn initialize_advertises_prompts_and_resources() {
    let client = start().await;
    let info = client.peer_info().expect("server info");
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());
    let instructions = info.instructions.as_deref().unwrap_or("");
    assert!(instructions.contains("pscat://guide/"), "{instructions}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn lists_and_reads_every_guide_document() {
    let client = start().await;
    let resources = client.list_all_resources().await.expect("resources/list");
    let uris: BTreeSet<String> = resources.iter().map(|r| r.uri.clone()).collect();
    let want: BTreeSet<String> = guide::source()
        .files
        .iter()
        .map(|f| format!("pscat://guide/{f}"))
        .collect();
    assert_eq!(uris, want);

    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    for r in &resources {
        assert_eq!(r.mime_type.as_deref(), Some("text/markdown"), "{}", r.uri);
        let desc = r.description.as_deref().unwrap_or("");
        assert!(desc.contains("PSCat"), "{}: {desc}", r.uri);
        assert!(
            !desc.contains("Use when"),
            "{}: descriptions stay neutral",
            r.uri
        );

        let read = client
            .read_resource(ReadResourceRequestParams::new(r.uri.clone()))
            .await
            .unwrap_or_else(|e| panic!("read {}: {e}", r.uri));
        let raw = serde_json::to_string(&read).unwrap();
        assert!(
            raw.len() <= MAX_REPLY_BYTES,
            "{} is {} bytes",
            r.uri,
            raw.len()
        );
        assert_eq!(read.contents.len(), 1, "{}", r.uri);
        let ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } = &read.contents[0]
        else {
            panic!("{} is not text", r.uri)
        };
        assert_eq!(uri, &r.uri);
        assert_eq!(mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(r.size, Some(text.len() as u64), "{}", r.uri);

        // The server's note, then PSCat's text unchanged.
        let path = r.uri.strip_prefix("pscat://guide/").unwrap();
        let original = vendored(path);
        assert!(text.ends_with(&original), "{} is not verbatim", r.uri);
        let note = &text[..text.len() - original.len()];
        assert!(note.starts_with("> **Note from pscat-mcp**"), "{note}");
        assert!(note.contains(&guide::source().commit[..10]), "{note}");
        assert!(note.contains(&guide::source().tag), "{note}");
        assert!(note.contains("MIT OR Apache-2.0"));
        assert!(note.contains("Jerome Banks and the pscat contributors"));
        assert!(note.contains("no `out_path`"), "{note}");
        // The note lists exactly this server's tools.
        let line = note
            .lines()
            .find(|l| l.starts_with("> - The tools are "))
            .expect("tools line");
        let listed = line
            .trim_start_matches("> - The tools are ")
            .split(". ")
            .next()
            .unwrap();
        let mut names: Vec<&str> = listed
            .split(", ")
            .map(|t| t.trim_matches(|c| c == '`' || c == '.'))
            .collect();
        names.sort_unstable();
        let mut have: Vec<&str> = tools.iter().map(String::as_str).collect();
        have.sort_unstable();
        assert_eq!(names, have, "{line}");
        for missing in ["start_render", "eval_postscript"] {
            if !have.contains(&missing) {
                assert!(line.contains(&format!("`{missing}`")), "{line}");
            }
        }
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn unknown_resources_and_prompts_are_mcp_errors() {
    let client = start().await;
    for uri in [
        "pscat://guide/nope.md",
        "pscat://guide/pscat/SKILL.md/../SKILL.md",
        "pscat://guide/",
        "file:///etc/passwd",
        "urn:pscat:render:x:page:1",
    ] {
        let e = client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .expect_err(uri);
        assert_eq!(mcp_code(e), ErrorCode::RESOURCE_NOT_FOUND.0, "{uri}");
    }
    let e = client
        .get_prompt(GetPromptRequestParams::new("no_such_prompt"))
        .await
        .expect_err("unknown prompt");
    assert_eq!(mcp_code(e), ErrorCode::INVALID_PARAMS.0);
    for (name, args) in [
        ("diagram", json!({"format": "gif"})),
        ("diagram", json!({"colour": "red"})),
        ("generative_art", json!({"seed": "seven"})),
        ("chart", json!({"kind": "radar"})),
        ("chart", json!({"data": "1 ".repeat(5000)})),
    ] {
        let e = client
            .get_prompt(
                GetPromptRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
            )
            .await
            .expect_err(name);
        assert_eq!(mcp_code(e), ErrorCode::INVALID_PARAMS.0, "{name} {args}");
    }
    // The server still serves after the errors.
    client
        .read_resource(ReadResourceRequestParams::new(
            "pscat://guide/psviz/SKILL.md",
        ))
        .await
        .expect("read after errors");
    client.cancel().await.ok();
}

// -------------------------------------------------------- prompts over stdio

#[tokio::test]
async fn lists_and_gets_prompts() {
    let client = start().await;
    let prompts = client.list_all_prompts().await.expect("prompts/list");
    let mut names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, PROMPTS);
    for p in &prompts {
        assert!(p.title.is_some() && p.description.is_some(), "{}", p.name);
        // Every argument is optional, so a prompt works with none (Claude
        // Code passes one whitespace-separated token per argument).
        for a in p.arguments.iter().flatten() {
            assert_eq!(a.required, Some(false), "{} {}", p.name, a.name);
        }
    }

    let cases = [
        (
            "diagram",
            json!({}),
            "pscat/SKILL.md",
            vec!["render_postscript", "ask me"],
        ),
        (
            "diagram",
            json!({"subject": "a three-step flowchart", "format": "SVG"}),
            "pscat/SKILL.md",
            vec!["a three-step flowchart", "format svg"],
        ),
        (
            "generative_art",
            json!({"subject": "a moonlit garden", "palette": "nocturne", "seed": "11"}),
            "psart/SKILL.md",
            vec![
                "(lib/artkit.ps) run",
                "/nocturne pal",
                "`11 srand`",
                "a moonlit garden",
            ],
        ),
        (
            "chart",
            json!({"kind": "bar", "data": "Q1 8, Q2 15, Q3 12, Q4 19"}),
            "psviz/SKILL.md",
            vec!["bar chart with PSCat's lib/dataviz.ps", "Q1 8, Q2 15"],
        ),
        (
            "chart",
            json!({"kind": "polar"}),
            "psviz/SKILL.md",
            vec!["lib/graph.ps", "ask me"],
        ),
    ];
    for (name, args, doc, needles) in cases {
        let mut params = GetPromptRequestParams::new(name);
        if let Value::Object(m) = &args
            && !m.is_empty()
        {
            params = params.with_arguments(m.clone());
        }
        let got = client
            .get_prompt(params)
            .await
            .unwrap_or_else(|e| panic!("{name} {args}: {e}"));
        let raw = serde_json::to_string(&got).unwrap();
        assert!(
            raw.len() <= MAX_REPLY_BYTES,
            "{name} is {} bytes",
            raw.len()
        );
        assert_eq!(got.messages.len(), 1);
        let text = &got.messages[0].content.as_text().expect("text").text;
        for n in needles {
            assert!(text.contains(n), "{name} {args} lacks {n:?}");
        }
        // The document as resources/read serves it, note included.
        let read = client
            .read_resource(ReadResourceRequestParams::new(format!(
                "pscat://guide/{doc}"
            )))
            .await
            .unwrap();
        let ResourceContents::TextResourceContents { text: served, .. } = &read.contents[0] else {
            panic!("text")
        };
        assert!(text.ends_with(served.as_str()), "{name} carries {doc}");
        assert!(text.contains(&format!("pscat://guide/{doc}")));
    }
    client.cancel().await.ok();
}

// ------------------------------------------- the guidance works on this server

/// Runnable ```postscript blocks (not `postscript partial`), as PSCat's own
/// `tests/skill_blocks.rs` picks them.
fn runnable_blocks() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, text) in guide::files() {
        let mut open: Option<(usize, bool, usize, String)> = None;
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            match &mut open {
                None => {
                    if let Some(info) = trimmed.strip_prefix("```") {
                        let mut words = info.split_whitespace();
                        let lang = words.next().unwrap_or("");
                        let partial = words.any(|w| w == "partial");
                        open = Some((
                            indent,
                            lang.eq_ignore_ascii_case("postscript") && !partial,
                            i + 2,
                            String::new(),
                        ));
                    }
                }
                Some((ind, runnable, first, code)) => {
                    if trimmed.starts_with("```") {
                        if *runnable {
                            out.push((format!("{path}:{first}"), std::mem::take(code)));
                        }
                        open = None;
                    } else {
                        let cut = (*ind).min(line.len() - line.trim_start().len());
                        code.push_str(&line[cut..]);
                        code.push('\n');
                    }
                }
            }
        }
    }
    out
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let params = CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().unwrap().clone());
    let r = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .expect("timed out")
        .expect("call");
    serde_json::to_value(&r).unwrap()
}

#[tokio::test]
async fn every_runnable_guide_block_renders_here() {
    let blocks = runnable_blocks();
    assert!(blocks.len() >= 10, "found only {} blocks", blocks.len());
    let client = start().await;
    for (where_, code) in &blocks {
        let r = call(&client, "render_postscript", json!({ "source": code })).await;
        assert_ne!(
            r["isError"], true,
            "{where_}: {}",
            r["structuredContent"]["error"]
        );
        assert_eq!(r["structuredContent"]["ok"], true, "{where_}");
        assert!(
            r["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "image"),
            "{where_}: no page"
        );
    }
    client.cancel().await.ok();
}

/// Every `lib/....ps` the guides name, the style packs of `lib/styles/` and
/// the kits psart lists by name, loaded in the worker.
#[tokio::test]
async fn every_library_the_guides_name_loads_here() {
    let mut libs = BTreeSet::new();
    for (_, text) in guide::files() {
        let mut rest = *text;
        while let Some(i) = rest.find("lib/") {
            let tail = &rest[i..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || "_/.".contains(c)))
                .unwrap_or(tail.len());
            let name = tail[..end].trim_end_matches('.');
            if name.ends_with(".ps") {
                libs.insert(name.to_string());
            }
            rest = &tail[4..];
        }
    }
    for kit in [
        "surfacekit",
        "etching",
        "stipplekit",
        "hatchkit",
        "halftonekit",
        "printkit",
        "fillkit",
        "headline",
        "lettering",
        "apparel",
        "pagekit",
        "styles/steampunk",
        "styles/psychedelic",
        "styles/scifi",
        "styles/toon",
    ] {
        libs.insert(format!("lib/{kit}.ps"));
    }
    assert!(libs.contains("lib/artkit.ps") && libs.contains("lib/dataviz.ps"));
    let client = start().await;
    for lib in &libs {
        // Style packs load after artkit.
        let source = if lib.starts_with("lib/styles/") {
            format!("(lib/artkit.ps) run ({lib}) run\n")
        } else {
            format!("({lib}) run\n")
        };
        let r = call(&client, "validate_postscript", json!({ "source": source })).await;
        let s = &r["structuredContent"];
        assert_eq!(s["ok"], true, "{lib}: {}", s["error"]);
    }
    client.cancel().await.ok();
}
