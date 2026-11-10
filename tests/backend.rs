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
        limits,
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
    let mut buf = vec![0; reader.output_buffer_size()];
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
