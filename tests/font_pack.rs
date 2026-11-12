//! The configured font-pack directory (`PSCAT_MCP_FONT_PACK_DIR`): the
//! worker finds packs in it and nowhere else, programs still cannot read
//! files, the host path is never reported, and with the setting unset
//! nothing changes.
//!
//! The pack is `tests/data/font-pack`: a stand-in `cjk-thai` pack holding a
//! 13 KB subset of Noto Sans JP. Each test copies it into its own scratch
//! directory first (a symlink test needs to add to it).

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use pscat_mcp::backend::{RenderBackend, SubprocessBackend};
use pscat_mcp::config::LimitsConfig;
use pscat_mcp::worker::{
    InterpLimits, JobMode, OutputFormat, WorkerFailure, WorkerJob, WorkerResult,
};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

mod common;

const SERVER: &str = env!("CARGO_BIN_EXE_pscat-mcp-server");
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(what: &str) -> Scratch {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("fontpack-{what}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, so a path the test hands a program is the one the
        // server resolves (macOS /var is /private/var).
        Scratch(std::fs::canonicalize(dir).unwrap())
    }

    /// A copy of the checked-in test pack tree, as a data root.
    fn with_pack(what: &str) -> Scratch {
        let s = Scratch::new(what);
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/font-pack"),
            &s.0,
        );
        s
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The last path component, unique to this scratch directory.
    fn unique(&self) -> String {
        self.0.file_name().unwrap().to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn limits() -> InterpLimits {
    InterpLimits {
        max_steps: Some(100_000_000),
        max_time_ms: Some(20_000),
        max_memory_bytes: Some(256 * 1024 * 1024),
        max_page_pixels: 16_000_000,
        max_pages: 10,
        max_output_bytes: 8 * 1024 * 1024,
        max_program_output_bytes: 16 * 1024,
    }
}

fn backend(pack_dir: Option<&Path>) -> SubprocessBackend {
    let b = SubprocessBackend::new(PathBuf::from(SERVER), LimitsConfig::default());
    match pack_dir {
        Some(d) => b.with_font_pack_dir(d),
        None => b,
    }
}

async fn run_job(pack_dir: Option<&Path>, mode: JobMode, source: &str) -> WorkerResult {
    backend(pack_dir)
        .execute(
            WorkerJob {
                mode,
                source: source.into(),
                format: OutputFormat::Png,
                page_points: Some((100, 100)),
                dpi: 72.0,
                halftone: false,
                limits: limits(),
                generate: None,
                decided_height: None,
                sheet: None,
            },
            Duration::from_secs(30),
        )
        .await
        .expect("worker result")
}

async fn run(pack_dir: Option<&Path>, source: &str) -> WorkerResult {
    run_job(pack_dir, JobMode::Render, source).await
}

/// The error name a program ended with, or `""` if it succeeded.
fn error_name(r: &WorkerResult) -> String {
    match &r.failure {
        None => String::new(),
        Some(WorkerFailure::PostScript) => r.ps_error.as_ref().expect("ps error").name.clone(),
        Some(other) => panic!("not a PostScript error: {other:?}"),
    }
}

/// `stringwidth` of "i" at 1000 points in `/NotoSansJP-Regular`: the face's
/// own width when it is found, the Helvetica stand-in's when it is not.
const WIDTH_OF_I: &str =
    "/NotoSansJP-Regular findfont 1000 scalefont setfont (i) stringwidth pop round cvi =";

// ---------------------------------------------------------------------
// The worker, through the backend.
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_configured_pack_supplies_the_face_and_unset_does_not() {
    let pack = Scratch::with_pack("face");
    let with = run(Some(pack.path()), WIDTH_OF_I).await;
    let without = run(None, WIDTH_OF_I).await;
    assert_eq!(error_name(&with), "", "{with:?}");
    assert_eq!(error_name(&without), "", "{without:?}");
    let (with, without) = (with.program_output.trim(), without.program_output.trim());
    assert!(!with.is_empty() && !without.is_empty());
    assert_ne!(with, without, "the pack face was not used");
    // The stand-in a missing pack face gets is Helvetica: 222 for "i".
    assert_eq!(without, "222");
}

#[tokio::test]
async fn the_catalog_lists_the_pack_without_its_path() {
    let pack = Scratch::with_pack("catalog");
    let with = run_job(Some(pack.path()), JobMode::Capabilities, "")
        .await
        .capabilities
        .expect("catalog");
    let installed: Vec<&str> = with["font_packs"]["installed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(installed, ["cjk-thai"], "{}", with["font_packs"]);
    assert!(
        with["font_packs"]["overrides"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let text = with.to_string();
    assert!(
        !text.contains(&pack.unique()),
        "the host path is in the catalog"
    );
    assert!(!text.contains(env!("CARGO_TARGET_TMPDIR")), "host path");

    // Unset: no packs, no overrides.
    let without = run_job(None, JobMode::Capabilities, "")
        .await
        .capabilities
        .expect("catalog");
    assert!(
        without["font_packs"]["installed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        without["font_packs"]["overrides"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Programs cannot read the directory, by any route.
#[tokio::test]
async fn programs_cannot_read_the_pack_directory() {
    let pack = Scratch::with_pack("sandbox");
    let outside = Scratch::new("outside");
    std::fs::write(outside.path().join("secret.txt"), "TOP-SECRET").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), pack.path().join("escape")).unwrap();
    // A planted lib/ must not shadow PSCat's compiled-in copy.
    std::fs::create_dir_all(pack.path().join("lib")).unwrap();
    std::fs::write(
        pack.path().join("lib/artkit.ps"),
        "(PLANTED-LIBRARY) = quit\n",
    )
    .unwrap();
    let root = pack.path().display().to_string();
    let read = |p: &str| format!("({p}) (r) file 100 string readstring pop =");

    // Absolute names into the directory, a file and a symlink out of it.
    for name in [
        format!("{root}/fonts/packs/cjk-thai/install.json"),
        format!("{root}/escape/secret.txt"),
        format!("{root}/fonts/packs/cjk-thai/../../../escape/secret.txt"),
        // `..` out of the directory, absolute and relative.
        format!("{root}/../{}/escape/secret.txt", pack.unique()),
        "../secret.txt".to_string(),
        "fonts/../../etc/passwd".to_string(),
    ] {
        let r = run(Some(pack.path()), &read(&name)).await;
        assert_eq!(error_name(&r), "invalidfileaccess", "{name}: {r:?}");
        assert!(!r.program_output.contains("TOP-SECRET"));
    }

    // Names the search order would resolve inside PSCAT_HOME are skipped:
    // nothing is read, and none of it is the file's content.
    for name in [
        "fonts/packs/cjk-thai/install.json",
        "escape/secret.txt",
        "install.json",
    ] {
        let r = run(Some(pack.path()), &read(name)).await;
        assert_ne!(error_name(&r), "", "{name} was readable: {r:?}");
        assert!(!r.program_output.contains("TOP-SECRET"), "{r:?}");
        assert!(!r.program_output.contains("cjk-thai"), "{r:?}");
    }

    // Listing the directory finds nothing.
    let r = run(
        Some(pack.path()),
        &format!("({root}/*) {{ = }} 256 string filenameforall (done) ="),
    )
    .await;
    assert_eq!(error_name(&r), "", "{r:?}");
    assert_eq!(r.program_output.trim(), "done");

    // Writing is refused as it always was.
    let r = run(
        Some(pack.path()),
        &format!("({root}/fonts/new.txt) (w) file dup (x) writestring closefile"),
    )
    .await;
    assert_eq!(error_name(&r), "invalidfileaccess", "{r:?}");
    assert!(!pack.path().join("fonts/new.txt").exists());

    // The compiled-in library still loads, not the planted one.
    let r = run(Some(pack.path()), "(lib/artkit.ps) run (library loaded) =").await;
    assert_eq!(error_name(&r), "", "{r:?}");
    assert_eq!(r.program_output.trim(), "library loaded");
}

/// A fake worker reports its environment: with the pack directory set it
/// gets `PSCAT_HOME` and nothing else; without, nothing of PSCat's but the
/// checkout switch it always had. The server's own variables never pass.
#[cfg(unix)]
#[tokio::test]
async fn the_workers_environment_is_only_what_the_server_sets() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Scratch::new("fake-worker");
    let script = dir.path().join("worker.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo \"home=${HOME:-unset} xdg=${XDG_DATA_HOME:-unset} cfg=${XDG_CONFIG_HOME:-unset} \
         root=${PSCAT_ROOT:-unset} lib=${PSCAT_LIB:-unset} home_dir=${PSCAT_HOME:-unset} \
         checkout=${PSCAT_NO_CHECKOUT:-unset}\" >&2\n\
         env | grep -c '^PSCAT_' >&2\n\
         exit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = |pack_dir: Option<&Path>| {
        let backend = {
            let b = SubprocessBackend::new(script.clone(), LimitsConfig::default());
            match pack_dir {
                Some(d) => b.with_font_pack_dir(d),
                None => b,
            }
        };
        async move {
            let out = backend
                .execute(
                    WorkerJob {
                        mode: JobMode::Render,
                        source: "showpage".into(),
                        format: OutputFormat::Png,
                        page_points: None,
                        dpi: 72.0,
                        halftone: false,
                        limits: limits(),
                        generate: None,
                        decided_height: None,
                        sheet: None,
                    },
                    Duration::from_secs(30),
                )
                .await;
            match out {
                Err(pscat_mcp::backend::BackendError::Crashed(m)) => m,
                other => panic!("expected a crash, got {other:?}"),
            }
        }
    };
    let unset = report(None).await;
    assert!(
        unset.contains(
            "home=unset xdg=unset cfg=unset root=unset lib=unset home_dir=unset checkout=1"
        ),
        "{unset}"
    );
    // Only PSCAT_NO_CHECKOUT.
    assert!(unset.trim_end().ends_with("\n1"), "{unset}");

    let set = report(Some(dir.path())).await;
    let want = format!(
        "home=unset xdg=unset cfg=unset root=unset lib=unset home_dir={} checkout=1",
        dir.path().display()
    );
    assert!(set.contains(&want), "{set}");
    // PSCAT_NO_CHECKOUT and PSCAT_HOME.
    assert!(set.trim_end().ends_with("\n2"), "{set}");
}

// ---------------------------------------------------------------------
// The server, over stdio.
// ---------------------------------------------------------------------

type Client = common::Client;

async fn start(env: &[(&str, &str)]) -> Client {
    common::start_stdio(SERVER, env).await
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

/// The packs `describe_art_capabilities` lists, by name.
async fn installed_packs(client: &Client) -> Vec<String> {
    let r = call_tool(client, "describe_art_capabilities", json!({})).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    structured(&r)["font_packs"]["installed"]
        .as_array()
        .expect("installed")
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn both_info_tools_report_the_visible_packs_and_never_the_path() {
    let pack = Scratch::with_pack("info");
    let client = start(&[("PSCAT_MCP_FONT_PACK_DIR", pack.path().to_str().unwrap())]).await;

    let mut replies = Vec::new();
    let caps = call_tool(&client, "describe_art_capabilities", json!({})).await;
    assert_eq!(caps.is_error, Some(false), "{}", text_of(&caps));
    let installed: Vec<&str> = structured(&caps)["font_packs"]["installed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(installed, ["cjk-thai"]);
    assert!(text_of(&caps).contains("Font packs installed: cjk-thai"));
    replies.push(caps);

    // The pack's face is in the catalog, and its entry carries no path.
    replies.push(
        call_tool(
            &client,
            "describe_art_capabilities",
            json!({"kind": "font"}),
        )
        .await,
    );
    let face = call_tool(
        &client,
        "describe_art_capabilities",
        json!({"names": ["NotoSansJP-Regular"]}),
    )
    .await;
    assert_eq!(face.is_error, Some(false), "{}", text_of(&face));
    assert!(
        structured(&face)["entries"]
            .as_array()
            .is_some_and(|e| !e.is_empty()),
        "{}",
        structured(&face)
    );
    replies.push(face);

    let info = call_tool(&client, "get_renderer_info", json!({})).await;
    assert_eq!(info.is_error, Some(false), "{}", text_of(&info));
    assert_eq!(
        structured(&info)["font_packs"]["installed"],
        json!(["cjk-thai"])
    );
    replies.push(info);

    for r in &replies {
        let all = format!("{}\n{}", structured(r), text_of(r));
        assert!(!all.contains(&pack.unique()), "host path leaked: {all}");
        assert!(
            !all.contains(env!("CARGO_TARGET_TMPDIR")),
            "host path leaked"
        );
    }
    client.cancel().await.ok();
}

/// With nothing configured, `get_renderer_info` has no `font_packs` and no
/// extra line, and no pack is visible.
#[tokio::test]
async fn unset_changes_nothing_that_is_reported() {
    let client = start(&[]).await;
    let info = call_tool(&client, "get_renderer_info", json!({})).await;
    assert_eq!(info.is_error, Some(false));
    let s = structured(&info);
    assert!(s.get("font_packs").is_none(), "{s}");
    assert_eq!(s["capabilities"].as_array().unwrap().len(), 4, "{s}");
    assert!(installed_packs(&client).await.is_empty());
    client.cancel().await.ok();
}

/// The server's `HOME`, `XDG_DATA_HOME`, `PSCAT_HOME` and `PSCAT_ROOT` do not
/// reach the worker: a pack in each of the places PSCat would look is not
/// seen, whether or not a font-pack directory is configured.
#[tokio::test]
async fn the_servers_own_environment_is_not_inherited() {
    let home = Scratch::new("inherit");
    let h = home.path();
    // Every per-user place PSCat searches, on macOS and elsewhere.
    for root in [
        h.to_path_buf(),
        h.join("Library/Application Support/pscat"),
        h.join(".local/share/pscat"),
        h.join("xdg/pscat"),
        h.join("root"),
    ] {
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/font-pack"),
            &root,
        );
    }
    let empty = Scratch::new("empty-pack-dir");
    let inherited = [
        ("HOME", h.to_str().unwrap().to_string()),
        ("XDG_DATA_HOME", h.join("xdg").display().to_string()),
        ("PSCAT_HOME", h.display().to_string()),
        ("PSCAT_ROOT", h.join("root").display().to_string()),
    ];
    let env_of = |extra: Option<&str>| {
        let mut env: Vec<(&str, String)> = inherited.to_vec();
        if let Some(d) = extra {
            env.push(("PSCAT_MCP_FONT_PACK_DIR", d.to_string()));
        }
        env
    };
    for extra in [None, Some(empty.path().to_str().unwrap())] {
        let env = env_of(extra);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let client = start(&env).await;
        assert!(
            installed_packs(&client).await.is_empty(),
            "a pack was found through the server's environment ({extra:?})"
        );
        client.cancel().await.ok();
    }
}

/// The PNG of a one-cell `review_sheet` labelled `label`.
async fn sheet(client: &Client, label: &str) -> Vec<u8> {
    let args = json!({
        "entries": [{"source": "0.8 0.2 0.2 setrgbcolor 0 0 100 100 rectfill", "label": label,
                     "width": 100, "height": 100}],
        "sizes": false
    });
    let r = call_tool(client, "review_sheet", args).await;
    assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    r.content
        .iter()
        .find_map(|c| c.as_image())
        .map(|i| {
            base64::engine::general_purpose::STANDARD
                .decode(&i.data)
                .unwrap()
        })
        .expect("an image")
}

/// A Japanese label is drawn in Japanese with the pack and as `?` marks
/// without it, and the pack changes nothing else about the sheet.
#[tokio::test]
async fn a_japanese_label_renders_with_the_pack() {
    let pack = Scratch::with_pack("sheet");
    let with = start(&[("PSCAT_MCP_FONT_PACK_DIR", pack.path().to_str().unwrap())]).await;
    let without = start(&[]).await;
    let (ja_with, q_with) = (sheet(&with, "日本語").await, sheet(&with, "???").await);
    let (ja_without, q_without) = (
        sheet(&without, "日本語").await,
        sheet(&without, "???").await,
    );
    // Without the pack the label is question marks, exactly.
    assert_eq!(ja_without, q_without, "Japanese was drawn without the pack");
    // With it, the label is not question marks, and it is not the sheet
    // that had none.
    assert_ne!(ja_with, q_with, "the pack did not change the label");
    assert_ne!(ja_with, ja_without);
    // A Latin label is untouched by the pack.
    assert_eq!(
        sheet(&with, "plain label").await,
        sheet(&without, "plain label").await
    );
    with.cancel().await.ok();
    without.cancel().await.ok();
}

// ---------------------------------------------------------------------
// Startup.
// ---------------------------------------------------------------------

#[test]
fn a_missing_or_non_directory_path_refuses_to_start() {
    let scratch = Scratch::new("startup");
    let file = scratch.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let missing = scratch.path().join("nope");
    for (path, why) in [(&missing, "No such file"), (&file, "is not a directory")] {
        let out = std::process::Command::new(SERVER)
            .arg("serve-stdio")
            .env("PSCAT_MCP_FONT_PACK_DIR", path)
            .env("PSCAT_MCP_LOG_FORMAT", "pretty")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the server");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "started with {path:?}: {err}");
        assert!(err.contains("PSCAT_MCP_FONT_PACK_DIR"), "{err}");
        assert!(err.contains(why), "{err}");
    }
    // The flag does the same.
    let out = std::process::Command::new(SERVER)
        .args(["serve-stdio", "--font-pack-dir"])
        .arg(&missing)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--font-pack-dir"));
}
