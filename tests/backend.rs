//! The subprocess backend against the real `render-worker` binary.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pscat_mcp::backend::{BackendError, RenderBackend, SubprocessBackend};
use pscat_mcp::config::LimitsConfig;
use pscat_mcp::worker::{
    InterpLimits, JobMode, OutputFormat, WorkerFailure, WorkerJob, WorkerResult,
};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pscat-mcp-server"))
}

fn backend() -> SubprocessBackend {
    SubprocessBackend::new(exe(), LimitsConfig::default())
}

/// A backend whose workers carry a unique `--tag`, so tests can look for them.
fn tagged() -> (SubprocessBackend, String) {
    let tag = format!("pscat-mcp-test-{}", uuid::Uuid::new_v4());
    (backend().with_tag(tag.clone()), tag)
}

fn limits() -> InterpLimits {
    InterpLimits {
        max_steps: Some(100_000_000),
        max_time_ms: Some(10_000),
        max_memory_bytes: Some(256 * 1024 * 1024),
        max_page_pixels: 16_000_000,
        max_pages: 10,
        max_output_bytes: 8 * 1024 * 1024,
        max_program_output_bytes: 16 * 1024,
    }
}

fn no_limits() -> InterpLimits {
    InterpLimits {
        max_steps: None,
        max_time_ms: None,
        max_memory_bytes: None,
        ..limits()
    }
}

fn job(
    source: &str,
    format: OutputFormat,
    page: Option<(u32, u32)>,
    limits: InterpLimits,
) -> WorkerJob {
    WorkerJob {
        mode: JobMode::Render,
        source: source.into(),
        format,
        page_points: page,
        dpi: 72.0,
        halftone: false,
        limits,
        generate: None,
        decided_height: None,
        sheet: None,
    }
}

async fn run(source: &str, limits: InterpLimits) -> WorkerResult {
    backend()
        .execute(
            job(source, OutputFormat::Png, Some((100, 100)), limits),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result")
}

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

const SCENE: &str = "\
1 0 0 setrgbcolor 10 10 80 60 rectfill
0 0 1 setrgbcolor 150 50 30 0 360 arc fill
0 1 0 setrgbcolor 4 setlinewidth 10 90 moveto 190 90 lineto stroke
showpage
";

fn decode(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("valid base64")
}

#[tokio::test]
async fn renders_png_with_the_right_pixels() {
    let result = backend()
        .execute(
            job(SCENE, OutputFormat::Png, Some((200, 100)), limits()),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result");
    assert_eq!(result.failure, None, "{:?}", result.ps_error);
    assert_eq!(result.page_count, 1);
    assert_eq!(result.artifacts.len(), 1);
    let art = &result.artifacts[0];
    assert_eq!((art.width_px, art.height_px), (200, 100));
    let bytes = decode(&art.data_base64);
    assert_eq!(bytes.len(), art.byte_len);

    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .expect("PNG buffer size fits in memory")
    ];
    let info = reader.next_frame(&mut buf).expect("png frame");
    assert_eq!((info.width, info.height), (200, 100));
    let channels = info.color_type.samples();
    assert!(
        channels >= 3,
        "colour PNG expected, got {:?}",
        info.color_type
    );
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    // PostScript y grows upwards; PNG rows run top to bottom.
    let pixel = |x: u32, y_ps: u32| -> [u8; 3] {
        let row = 100 - 1 - y_ps;
        let i = (row as usize * info.line_size) + x as usize * channels;
        [buf[i], buf[i + 1], buf[i + 2]]
    };
    assert_eq!(pixel(50, 40), [255, 0, 0], "inside the red rectangle");
    assert_eq!(pixel(150, 50), [0, 0, 255], "centre of the blue circle");
    assert_eq!(pixel(100, 90), [0, 255, 0], "on the green line");
    assert_eq!(pixel(1, 1), [255, 255, 255], "bottom-left corner");
    assert_eq!(pixel(198, 2), [255, 255, 255], "bottom-right corner");
    assert_eq!(pixel(105, 50), [255, 255, 255], "between the shapes");
}

#[tokio::test]
async fn renders_svg_and_pdf() {
    let svg = backend()
        .execute(
            job(SCENE, OutputFormat::Svg, Some((200, 100)), limits()),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result");
    assert_eq!(svg.failure, None, "{:?}", svg.ps_error);
    assert_eq!(svg.artifacts.len(), 1);
    let text = String::from_utf8(decode(&svg.artifacts[0].data_base64)).expect("utf-8 svg");
    let start = text.trim_start();
    assert!(
        start.starts_with("<svg") || start.starts_with("<?xml"),
        "{start:.80}"
    );
    assert!(text.contains("svg"));

    let pdf = backend()
        .execute(
            job(SCENE, OutputFormat::Pdf, Some((200, 100)), limits()),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result");
    assert_eq!(pdf.failure, None, "{:?}", pdf.ps_error);
    assert_eq!(pdf.artifacts.len(), 1);
    assert_eq!(pdf.artifacts[0].page, None);
    assert!(decode(&pdf.artifacts[0].data_base64).starts_with(b"%PDF-"));
}

#[tokio::test]
async fn a_runaway_worker_is_killed_at_the_deadline() {
    let (backend, tag) = tagged();
    let started = Instant::now();
    let out = backend
        .execute(
            job("{ } loop", OutputFormat::Png, Some((100, 100)), no_limits()),
            Duration::from_secs(1),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(matches!(out, Err(BackendError::Timeout)), "{out:?}");
    assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    let left = processes_with(&tag);
    assert!(left.is_empty(), "worker still running: {left:?}");
}

#[tokio::test]
async fn dropping_the_future_kills_the_worker() {
    let (backend, tag) = tagged();
    let fut = backend.execute(
        job("{ } loop", OutputFormat::Png, Some((100, 100)), no_limits()),
        Duration::from_secs(30),
    );
    let cancelled = tokio::time::timeout(Duration::from_millis(300), fut).await;
    assert!(cancelled.is_err(), "the loop cannot finish");
    // The worker really was running before the cancel... and is gone after.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let left = processes_with(&tag);
        if left.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "worker still running: {left:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_worker_is_running_while_execute_is_pending() {
    // Guards the two tests above: pgrep must see a live tagged worker.
    let (backend, tag) = tagged();
    let fut = backend.execute(
        job("{ } loop", OutputFormat::Png, Some((100, 100)), no_limits()),
        Duration::from_secs(30),
    );
    tokio::pin!(fut);
    tokio::select! {
        _ = &mut fut => panic!("the loop cannot finish"),
        _ = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    assert!(
        !processes_with(&tag).is_empty(),
        "tagged worker not found by pgrep"
    );
}

#[tokio::test]
async fn step_limit_stops_the_program_inside_the_worker() {
    let result = run(
        "{ } loop",
        InterpLimits {
            max_steps: Some(1_000_000),
            ..no_limits()
        },
    )
    .await;
    assert_eq!(result.failure, Some(WorkerFailure::PostScript));
    let err = result.ps_error.expect("ps error");
    assert_eq!(err.limit.as_deref(), Some("steps"), "{err:?}");
    assert!(result.artifacts.is_empty());
}

#[tokio::test]
async fn reading_host_files_is_refused() {
    let result = run("(/etc/passwd) (r) file 100 string readstring", limits()).await;
    assert_eq!(result.failure, Some(WorkerFailure::PostScript));
    let err = result.ps_error.expect("ps error");
    eprintln!("read: {err:?}");
    assert_eq!(err.name, "invalidfileaccess", "{err:?}");
}

#[tokio::test]
async fn writing_files_is_refused() {
    let path = format!("/tmp/pscat_mcp_x_{}", uuid::Uuid::new_v4());
    let source = format!("({path}) (w) file dup (hello) writestring closefile");
    let result = run(&source, limits()).await;
    assert_eq!(result.failure, Some(WorkerFailure::PostScript));
    let err = result.ps_error.expect("ps error");
    eprintln!("write: {err:?}");
    assert_eq!(err.name, "invalidfileaccess", "{err:?}");
    assert!(!std::path::Path::new(&path).exists(), "{path} was created");
}

#[tokio::test]
async fn output_over_the_cap_is_refused() {
    let result = backend()
        .execute(
            job(
                SCENE,
                OutputFormat::Png,
                Some((200, 100)),
                InterpLimits {
                    max_output_bytes: 100,
                    ..limits()
                },
            ),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result");
    assert!(
        matches!(
            result.failure,
            Some(WorkerFailure::OutputTooLarge { max: 100, .. })
        ),
        "{:?}",
        result.failure
    );
    assert!(result.artifacts.is_empty());
}

/// Many small marks: a large SVG (each rect becomes its own path).
const MANY_MARKS: &str =
    "1 srand 0 1 30000 { pop rand 500 mod rand 500 mod 3 3 rectfill } for showpage";

#[tokio::test]
async fn large_output_flows_through_the_pipe() {
    // Well past a pipe buffer: RLIMIT_FSIZE=0 must not affect pipes.
    let result = backend()
        .execute(
            job(MANY_MARKS, OutputFormat::Svg, Some((500, 500)), limits()),
            Duration::from_secs(20),
        )
        .await
        .expect("worker result");
    assert_eq!(result.failure, None, "{:?}", result.ps_error);
    let bytes = result.artifacts[0].byte_len;
    eprintln!("svg bytes: {bytes}");
    assert!(bytes > 1_500_000, "{bytes}");
}

#[tokio::test]
async fn stdout_over_the_backend_cap_kills_the_worker() {
    // A backend configured for tiny output, handed a job whose own limits
    // let the worker write far more: the parent's cap is the backstop.
    let config = LimitsConfig {
        max_output_bytes: 1000,
        max_program_output_bytes: 0,
        ..LimitsConfig::default()
    };
    let backend = SubprocessBackend::new(exe(), config);
    let out = backend
        .execute(
            job(MANY_MARKS, OutputFormat::Svg, Some((500, 500)), limits()),
            Duration::from_secs(20),
        )
        .await;
    assert!(
        matches!(out, Err(BackendError::OutputOverflow(_))),
        "{out:?}"
    );
}

#[tokio::test]
async fn memory_limit_is_enforced_by_the_worker_binary() {
    let result = run(
        "[ 0 1 10000000 { } for ]",
        InterpLimits {
            max_memory_bytes: Some(32 * 1024 * 1024),
            ..limits()
        },
    )
    .await;
    assert_eq!(result.failure, Some(WorkerFailure::PostScript));
    let err = result.ps_error.expect("ps error");
    eprintln!("memory: {err:?}");
    assert_eq!(err.limit.as_deref(), Some("memory"), "{err:?}");
}

#[tokio::test]
async fn a_missing_executable_is_a_spawn_error() {
    let backend = SubprocessBackend::new(
        PathBuf::from("/nonexistent/pscat-worker"),
        LimitsConfig::default(),
    );
    let out = backend
        .execute(
            job(SCENE, OutputFormat::Png, None, limits()),
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(out, Err(BackendError::Spawn(_))), "{out:?}");
}

#[tokio::test]
async fn a_worker_that_is_not_a_worker_is_a_protocol_or_crash_error() {
    // `true` exits 0 with no output: not a result.
    let backend = SubprocessBackend::new(PathBuf::from("/usr/bin/true"), LimitsConfig::default());
    let out = backend
        .execute(
            job(SCENE, OutputFormat::Png, None, limits()),
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(out, Err(BackendError::Protocol(_))), "{out:?}");
    // `false` exits 1.
    let backend = SubprocessBackend::new(PathBuf::from("/usr/bin/false"), LimitsConfig::default());
    let out = backend
        .execute(
            job(SCENE, OutputFormat::Png, None, limits()),
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(out, Err(BackendError::Crashed(_))), "{out:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn the_worker_runs_with_os_limits_in_its_own_session() {
    use std::os::unix::fs::PermissionsExt as _;
    // A stand-in worker that reports its environment on stderr and fails,
    // so the report comes back in the Crashed message.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fake-worker-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("worker.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo \"core=$(ulimit -c) cpu=$(ulimit -t) fsize=$(ulimit -f) nofile=$(ulimit -n)\" >&2\n\
         echo \"pid=$$ pgid=$(/bin/ps -o pgid= -p $$ | tr -d ' ') pwd=$(pwd)\" >&2\n\
         echo \"checkout=$PSCAT_NO_CHECKOUT home=${HOME:-unset} args=$*\" >&2\n\
         exit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = SubprocessBackend::new(script, LimitsConfig::default()).with_tag("t1");
    let out = backend
        .execute(
            job(SCENE, OutputFormat::Png, None, limits()),
            Duration::from_millis(1500),
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    let Err(BackendError::Crashed(message)) = out else {
        panic!("expected a crash, got {out:?}");
    };
    eprintln!("{message}");
    assert!(message.contains("status 7"), "{message}");
    // ceil(1.5 s) + 2 s of slack.
    assert!(
        message.contains("core=0 cpu=4 fsize=0 nofile=64"),
        "{message}"
    );
    let pid = message
        .split("pid=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap();
    assert!(
        message.contains(&format!("pgid={pid} ")),
        "not a group leader: {message}"
    );
    assert!(message.contains("pwd=/\n"), "{message}");
    assert!(
        message.contains("checkout=1 home=unset args=render-worker --tag t1"),
        "{message}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_worker_killed_by_a_signal_is_a_crash() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fake-worker-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("worker.sh");
    std::fs::write(&script, "#!/bin/sh\necho dying >&2\nkill -KILL $$\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = SubprocessBackend::new(script, LimitsConfig::default())
        .execute(
            job(SCENE, OutputFormat::Png, None, limits()),
            Duration::from_secs(5),
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    let Err(BackendError::Crashed(message)) = out else {
        panic!("expected a crash, got {out:?}");
    };
    assert!(message.contains("signal 9"), "{message}");
    assert!(message.contains("dying"), "{message}");
}

/// Capabilities mode runs no program: it returns PSCat's catalog, with no
/// host paths, inside the same sandboxed worker process.
#[tokio::test]
async fn the_worker_returns_the_art_catalog() {
    let job = WorkerJob {
        mode: JobMode::Capabilities,
        source: String::new(),
        format: OutputFormat::Png,
        page_points: None,
        dpi: 72.0,
        halftone: false,
        limits: limits(),
        generate: None,
        decided_height: None,
        sheet: None,
    };
    let started = Instant::now();
    let result = backend()
        .execute(job, Duration::from_secs(20))
        .await
        .expect("worker result");
    assert_eq!(result.failure, None);
    assert!(result.artifacts.is_empty());
    assert_eq!(result.page_count, 0);
    let catalog = result.capabilities.expect("catalog");
    assert!(!catalog["capabilities"].as_array().unwrap().is_empty());
    assert!(!catalog.to_string().contains("\"path\""), "no host paths");
    // Cheap enough to run per call without a cache.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// The halftone flag in the job makes the worker screen every PNG page, and a
/// job from an older server without the field still renders normally.
#[tokio::test]
async fn the_worker_halftones_png_pages_when_asked() {
    let source = "0.5 setgray 0 0 60 60 rectfill showpage 0.5 setgray 0 0 60 60 rectfill showpage";
    let mut plain = job(source, OutputFormat::Png, Some((60, 60)), limits());
    let mut json = serde_json::to_value(&plain).unwrap();
    json.as_object_mut().unwrap().remove("halftone");
    plain = serde_json::from_value(json).expect("a job without halftone parses");
    assert!(!plain.halftone);
    // 144 dpi: both PNGs must record the same resolution (pHYs chunk).
    plain.dpi = 144.0;
    plain.page_points = Some((30, 30));
    let mut screened = plain.clone();
    screened.halftone = true;

    let plain = backend()
        .execute(plain, Duration::from_secs(20))
        .await
        .expect("worker result");
    let screened = backend()
        .execute(screened, Duration::from_secs(20))
        .await
        .expect("worker result");
    assert_eq!(screened.failure, None, "{:?}", screened.ps_error);
    assert_eq!(screened.artifacts.len(), 2);
    for (a, b) in plain.artifacts.iter().zip(&screened.artifacts) {
        assert_eq!((a.width_px, a.height_px), (b.width_px, b.height_px));
        assert_eq!((a.width_px, a.height_px), (60, 60));
        let (plain_png, screened_png) = (decode(&a.data_base64), decode(&b.data_base64));
        assert_ne!(plain_png, screened_png);
        let phys = |png: &[u8]| {
            let at = png
                .windows(4)
                .position(|w| w == b"pHYs")
                .expect("pHYs chunk");
            png[at + 4..at + 13].to_vec()
        };
        assert_eq!(
            phys(&plain_png),
            phys(&screened_png),
            "same resolution record"
        );
        let mut reader = png::Decoder::new(std::io::Cursor::new(decode(&b.data_base64)))
            .read_info()
            .expect("png header");
        let mut buf = vec![0; reader.output_buffer_size().expect("buffer size")];
        let info = reader.next_frame(&mut buf).expect("png frame");
        let channels = info.color_type.samples();
        for px in buf[..info.buffer_size()].chunks_exact(channels) {
            let rgb = &px[..channels.min(3)];
            assert!(
                rgb.iter().all(|c| *c == 0) || rgb.iter().all(|c| *c == 255),
                "{px:?}"
            );
        }
    }
}

/// A `handwrite` generate job for `text`, with the given page and budget.
fn note_job(
    text: &str,
    page_points: Option<(u32, u32)>,
    decided_height: Option<pscat_mcp::generate::DecidedHeight>,
) -> WorkerJob {
    use pscat_mcp::generate::GenerateJob;
    use pscat_mcp::handwrite::HandwriteParams;
    let params: HandwriteParams =
        serde_json::from_value(serde_json::json!({"text": text})).expect("handwrite parameters");
    WorkerJob {
        mode: JobMode::Generate,
        source: String::new(),
        format: OutputFormat::Png,
        page_points,
        dpi: 72.0,
        halftone: false,
        limits: limits(),
        generate: Some(GenerateJob::Handwrite(params)),
        decided_height,
        sheet: None,
    }
}

fn budget(width_points: u32, max_pixels: u64) -> pscat_mcp::generate::DecidedHeight {
    pscat_mcp::generate::DecidedHeight {
        width_points,
        max_pixels,
        max_side_px: 8000,
    }
}

async fn run_job(job: WorkerJob) -> WorkerResult {
    backend()
        .execute(job, Duration::from_secs(20))
        .await
        .expect("worker result")
}

/// The worker lays the note out, sizes the page within the budget, and
/// counts the layout pass's steps in the result.
#[tokio::test]
async fn a_decided_height_is_laid_out_and_drawn_within_the_budget() {
    let r = run_job(note_job("hello\nthere", None, Some(budget(612, 1_000_000)))).await;
    assert_eq!(r.failure, None, "{:?}", r.ps_error);
    assert_eq!(r.artifacts.len(), 1);
    assert_eq!(
        (r.artifacts[0].width_px, r.artifacts[0].height_px),
        (612, 200)
    );
    assert!(r.steps_used > 0);
}

/// An over-tall note is refused with the structured failure, from the
/// layout, before a page is allocated or drawn.
#[tokio::test]
async fn a_note_over_the_budget_is_refused_before_it_is_drawn() {
    use pscat_mcp::generate::GenerateErrorCode;
    let text = "line\n".repeat(50);
    // 612 x 2700 = 1.65 M pixels is the note; the budget is 1 M.
    let r = run_job(note_job(&text, None, Some(budget(612, 1_000_000)))).await;
    let Some(WorkerFailure::Generate(f)) = r.failure else {
        panic!("expected a generator failure, got {:?}", r.failure);
    };
    assert_eq!(f.code, GenerateErrorCode::TooManyPixels);
    assert!(f.message.contains("50 lines"), "{}", f.message);
    assert!(r.artifacts.is_empty());
}

/// The page must meet the plan: a width that differs from the budget's, or
/// a job that states both a page and a budget (or neither), is a bad job.
#[tokio::test]
async fn a_generated_page_that_misses_the_plan_is_a_bad_job() {
    for job in [
        note_job("hi", None, Some(budget(400, 1_000_000))),
        note_job("hi", Some((612, 146)), Some(budget(612, 1_000_000))),
        note_job("hi", Some((612, 146)), None),
        note_job("hi", None, None),
    ] {
        let r = run_job(job).await;
        assert!(
            matches!(r.failure, Some(WorkerFailure::BadJob { .. })),
            "{:?}",
            r.failure
        );
        assert!(r.artifacts.is_empty());
    }
}

/// A `review_sheet` job for `entries`, with `budget`.
fn sheet_job(
    entries: serde_json::Value,
    budget: Option<pscat_mcp::sheet::SheetBudget>,
) -> WorkerJob {
    use pscat_mcp::generate::GenerateJob;
    let params = serde_json::from_value(serde_json::json!({ "entries": entries }))
        .expect("sheet parameters");
    WorkerJob {
        mode: JobMode::Generate,
        source: String::new(),
        format: OutputFormat::Png,
        page_points: None,
        dpi: 72.0,
        halftone: false,
        limits: limits(),
        generate: Some(GenerateJob::ReviewSheet(params)),
        decided_height: None,
        sheet: budget,
    }
}

fn sheet_budget(max_pixels: u64) -> pscat_mcp::sheet::SheetBudget {
    pscat_mcp::sheet::SheetBudget {
        max_cells: 32,
        max_source_bytes: 1 << 20,
        max_entry_source_bytes: 1 << 20,
        max_cell_pixels: 16_000_000,
        max_pixels,
        max_side_px: 8000,
        time_ms: 10_000,
    }
}

/// The worker composes a sheet in one job: one PNG artifact and a report
/// of every cell.
#[tokio::test]
async fn a_sheet_is_one_worker_job() {
    let entries = serde_json::json!([
        {"source": "0 0 10 10 rectfill showpage", "width": 20, "height": 20, "dpi": [72, 144]},
        {"source": "nosuchop", "width": 20, "height": 20}
    ]);
    let r = backend()
        .execute(
            sheet_job(entries, Some(sheet_budget(1_000_000))),
            Duration::from_secs(30),
        )
        .await
        .expect("worker result");
    assert!(r.failure.is_none(), "{:?}", r.failure);
    assert_eq!(r.artifacts.len(), 1);
    let report = r.sheet.expect("a sheet report");
    assert_eq!(
        (r.artifacts[0].width_px, r.artifacts[0].height_px),
        (report.width_px, report.height_px)
    );
    assert_eq!(report.cells.len(), 3);
    assert_eq!(report.failed_cells, 1);
    assert_eq!(
        (report.cells[1].width_px, report.cells[1].height_px),
        (40, 40)
    );
    assert!(!report.cells[2].ok);
}

/// The worker checks the exact sheet against the budget before it runs a
/// cell: a sheet over it comes back at once, although its cell would loop
/// until the deadline.
#[tokio::test]
async fn the_worker_checks_the_exact_sheet_before_any_cell() {
    let entries = serde_json::json!([
        {"source": "{ 1 pop } loop", "width": 100, "height": 100}
    ]);
    let started = Instant::now();
    let r = backend()
        .execute(
            sheet_job(entries.clone(), Some(sheet_budget(5_000))),
            Duration::from_secs(30),
        )
        .await
        .expect("worker result");
    assert!(started.elapsed() < Duration::from_secs(5));
    match r.failure {
        Some(WorkerFailure::Generate(f)) => {
            assert_eq!(f.detail, "sheet_pixels", "{}", f.message);
            assert!(
                f.message.contains("with its labels and captions"),
                "{}",
                f.message
            );
        }
        other => panic!("expected a sheet refusal, got {other:?}"),
    }
    assert_eq!(r.steps_used, 0);
    // Without a budget the job is refused, not guessed at.
    let r = backend()
        .execute(sheet_job(entries, None), Duration::from_secs(30))
        .await
        .expect("worker result");
    assert!(
        matches!(r.failure, Some(WorkerFailure::BadJob { .. })),
        "{:?}",
        r.failure
    );
}
