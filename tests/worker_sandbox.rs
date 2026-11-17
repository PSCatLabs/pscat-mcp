//! The worker's seccomp filter (Linux only; on other systems this file is
//! empty and the worker has no filter).
//!
//! * A child process (this test binary, started again for one test)
//!   installs the filter with the worker's own function and tries what a
//!   compromised interpreter would: sockets of every kind, a connection to
//!   the cloud metadata address, io_uring, ptrace. Each must fail with
//!   `EPERM`, while pipes, files, threads and memory keep working.
//! * A real `render-worker` started by the production backend runs under the
//!   filter (one more seccomp filter than its parent and `NoNewPrivs: 1` in
//!   `/proc/<pid>/status`) and holds no socket.
//!
//! * A child of a process that called `harden_server` (as `serve-http` and
//!   `serve-stdio` do) cannot open its parent's `/proc/<pid>/mem`
//!   (`EACCES`), and the real `serve-stdio` process becomes closed to its
//!   own parent the same way.
//!
//! The rest of the suite renders through real workers, so on Linux every
//! render and generator test also runs under the filter.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use pscat_mcp::backend::{RenderBackend, SubprocessBackend};
use pscat_mcp::config::LimitsConfig;
use pscat_mcp::worker::{InterpLimits, JobMode, OutputFormat, WorkerJob};

/// Set in the child's environment; the child test does nothing without it.
const CHILD_ENV: &str = "PSCAT_MCP_TEST_SANDBOX_CHILD";
/// Printed by the child once every check passed.
const CHILD_OK: &str = "sandbox child: all checks passed";

/// The `/proc/<pid>/status` value for `key` (for example `Seccomp`).
fn status_field(status: &str, key: &str) -> Option<String> {
    status.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k == key).then(|| v.trim().to_string())
    })
}

fn own_status() -> String {
    std::fs::read_to_string("/proc/thread-self/status").expect("status readable")
}

/// Whether `after` (a process or thread status) carries one more seccomp
/// filter than `before`, its parent's. A container runtime may already have
/// installed a filter (Docker's default profile does), so `Seccomp: 2`
/// alone proves nothing: compare `Seccomp_filters` (Linux 5.9+), or else
/// `NoNewPrivs`, which only the worker's filter sets here.
fn has_one_more_filter(before: &str, after: &str) -> bool {
    if status_field(after, "Seccomp").as_deref() != Some("2") {
        return false;
    }
    let count = |s: &str| status_field(s, "Seccomp_filters")?.parse::<u32>().ok();
    match (count(before), count(after)) {
        (Some(b), Some(a)) => a > b,
        _ => {
            status_field(before, "NoNewPrivs").as_deref() == Some("0")
                && status_field(after, "NoNewPrivs").as_deref() == Some("1")
        }
    }
}

fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

/// `ret` is -1 with errno EPERM.
#[track_caller]
fn assert_eperm(what: &str, ret: i64) {
    let e = errno();
    assert!(
        ret == -1 && e == Some(libc::EPERM),
        "{what}: expected EPERM, got {ret} (errno {e:?})"
    );
}

fn socket_eperm(domain: i32, ty: i32) {
    // SAFETY: plain system call.
    let fd = unsafe { libc::socket(domain, ty | libc::SOCK_CLOEXEC, 0) };
    if fd >= 0 {
        // SAFETY: closing the descriptor just created.
        unsafe { libc::close(fd) };
    }
    assert_eperm(&format!("socket({domain}, {ty})"), fd.into());
}

/// The child's checks, run after installing the filter.
fn child_checks() {
    let before = own_status();
    pscat_mcp::sandbox::install().expect("the filter installs");

    let status = own_status();
    assert!(
        has_one_more_filter(&before, &status),
        "no new filter:\n{before}\n{status}"
    );
    assert_eq!(status_field(&status, "NoNewPrivs").as_deref(), Some("1"));

    // Sockets of every family the worker could reach anything with.
    for domain in [
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_UNIX,
        libc::AF_NETLINK,
        libc::AF_PACKET,
    ] {
        socket_eperm(domain, libc::SOCK_STREAM);
        socket_eperm(domain, libc::SOCK_DGRAM);
    }
    let mut pair = [0; 2];
    // SAFETY: `pair` holds two descriptors.
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) };
    assert_eperm("socketpair", r.into());

    // connect() on stdin: without the filter that is ENOTSOCK, not EPERM.
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 80u16.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes([169, 254, 169, 254]).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: `addr` is a valid sockaddr_in of the given length.
    let r = unsafe {
        libc::connect(
            0,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eperm("connect", r.into());
    // SAFETY: as above.
    let r = unsafe {
        libc::bind(
            0,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eperm("bind", r.into());
    // SAFETY: plain system calls on stdin.
    assert_eperm("listen", unsafe { libc::listen(0, 1) }.into());
    // SAFETY: as above; null address pointers are allowed.
    let r = unsafe { libc::accept(0, std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eperm("accept", r.into());

    // What std (and so any Rust code in the worker) sees.
    let e = std::net::TcpStream::connect_timeout(
        &"169.254.169.254:80".parse().unwrap(),
        Duration::from_secs(2),
    )
    .expect_err("no TCP connection to the metadata server");
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
    let e = std::net::UdpSocket::bind("127.0.0.1:0").expect_err("no UDP socket");
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");

    // io_uring would open sockets outside seccomp's view.
    let mut params = [0u8; 120]; // struct io_uring_params
    // SAFETY: the kernel would write at most the struct's size into params.
    let r = unsafe { libc::syscall(libc::SYS_io_uring_setup, 4, params.as_mut_ptr()) };
    assert_eperm("io_uring_setup", r);

    // Reaching into another process (the server runs as the same user).
    // SAFETY: plain system call.
    let r = unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) };
    assert_eperm("ptrace", r);
    let mut buf = [0u8; 8];
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: both iovecs describe `buf`.
    let r = unsafe { libc::process_vm_readv(libc::getppid(), &local, 1, &remote, 1, 0) };
    assert_eperm("process_vm_readv", r as i64);
    // SAFETY: plain system call; no descriptor is produced on EPERM.
    let r = unsafe { libc::syscall(libc::SYS_pidfd_getfd, 0, 0, 0) };
    assert_eperm("pidfd_getfd", r);

    // What the worker needs still works: pipes, files, memory, threads.
    let mut fds = [0; 2];
    // SAFETY: `fds` holds two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: writing and reading our own pipe.
    unsafe {
        assert_eq!(libc::write(fds[1], b"ok".as_ptr().cast(), 2), 2);
        let mut got = [0u8; 2];
        assert_eq!(libc::read(fds[0], got.as_mut_ptr().cast(), 2), 2);
        assert_eq!(&got, b"ok");
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    assert!(std::fs::metadata("/").is_ok());
    let big = vec![7u8; 64 << 20];
    assert_eq!(big[big.len() - 1], 7);
    // A thread started now inherits the filter.
    std::thread::spawn(|| {
        socket_eperm(libc::AF_INET, libc::SOCK_STREAM);
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        assert_eq!(status_field(&status, "Seccomp").as_deref(), Some("2"));
    })
    .join()
    .expect("a thread runs under the filter");

    println!("{CHILD_OK}");
}

/// The child half: runs only in the process `blocked_calls_fail_with_eperm`
/// starts. libtest runs it on a thread of its own, so the filter covers that
/// thread and the threads it starts, which is all the checks use.
#[test]
fn sandbox_child() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_checks();
    }
}

#[test]
fn blocked_calls_fail_with_eperm() {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "sandbox_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("child runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(CHILD_OK),
        "child failed ({}):\n{stdout}\n{stderr}",
        out.status
    );
}

fn limits() -> InterpLimits {
    InterpLimits {
        max_steps: None,
        max_time_ms: None,
        max_memory_bytes: None,
        max_page_pixels: 16_000_000,
        max_pages: 10,
        max_output_bytes: 8 * 1024 * 1024,
        max_program_output_bytes: 16 * 1024,
    }
}

/// Process ids whose command line contains `tag`.
fn pids_with(tag: &str) -> Vec<u32> {
    let out = Command::new("pgrep")
        .arg("-f")
        .arg(tag)
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[tokio::test]
async fn a_real_worker_runs_under_the_filter_and_holds_no_socket() {
    let tag = format!("pscat-mcp-sandbox-test-{}", uuid::Uuid::new_v4());
    let backend = SubprocessBackend::new(
        PathBuf::from(env!("CARGO_BIN_EXE_pscat-mcp-server")),
        LimitsConfig::default(),
    )
    .with_tag(tag.clone());
    let job = WorkerJob {
        mode: JobMode::Render,
        source: "{ } loop".into(),
        format: OutputFormat::Png,
        page_points: Some((100, 100)),
        dpi: 72.0,
        halftone: false,
        limits: limits(),
        generate: None,
        decided_height: None,
        sheet: None,
    };
    let fut = backend.execute(job, Duration::from_secs(30));
    tokio::pin!(fut);

    // Wait until the worker has installed its filter (before it reads the
    // job). Its parent is this test process, so it starts with our filters.
    let ours = own_status();
    let deadline = Instant::now() + Duration::from_secs(20);
    let (pid, status) = loop {
        tokio::select! {
            _ = &mut fut => panic!("the loop cannot finish"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        let found = pids_with(&tag).into_iter().find_map(|pid| {
            let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            has_one_more_filter(&ours, &s).then_some((pid, s))
        });
        if let Some(found) = found {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "no tagged worker with a seccomp filter (pids {:?})",
            pids_with(&tag)
        );
    };
    assert_eq!(status_field(&status, "NoNewPrivs").as_deref(), Some("1"));

    // Nothing it inherited is a socket: just the three pipes.
    let mut kinds = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).expect("fd list readable") {
        let target = std::fs::read_link(entry.unwrap().path()).unwrap_or_default();
        kinds.push(target.to_string_lossy().into_owned());
    }
    assert!(
        kinds.iter().all(|k| !k.starts_with("socket:")),
        "worker holds a socket: {kinds:?}"
    );
    // Leaving the test drops the pending render, which kills the worker.
}

/// Set in a "server" child: `plain` or `hardened` (calls `harden_server`).
const SERVER_ENV: &str = "PSCAT_MCP_TEST_SERVER_CHILD";
/// Set in the server child's own child, which probes its parent's memory.
const PROBE_ENV: &str = "PSCAT_MCP_TEST_MEM_PROBE";

/// libtest prints `test <name> ... ` before a test's own output, so the
/// reports below are found anywhere in a line.
///
/// Try to open the parent's `/proc/<pid>/mem` for reading: `open-ok` or
/// `open-err <errno>`.
fn probe_parent_mem() -> String {
    let path = format!("/proc/{}/mem", std::os::unix::process::parent_id());
    match std::fs::File::open(path) {
        Ok(_) => "open-ok".into(),
        Err(e) => format!("open-err {}", e.raw_os_error().unwrap_or(-1)),
    }
}

/// The probe half: runs only in the grandchild.
#[test]
fn mem_probe_child() {
    if std::env::var_os(PROBE_ENV).is_some() {
        println!("probe: {}", probe_parent_mem());
    }
}

/// The server half: optionally hardens itself like `serve-*`, lets Yama
/// (where present) admit any tracer so only dumpability decides, then starts
/// a child of the same user that probes its memory.
#[test]
fn server_like_child() {
    let Some(mode) = std::env::var_os(SERVER_ENV) else {
        return;
    };
    if mode == "hardened" {
        pscat_mcp::sandbox::harden_server().expect("the server hardens");
    }
    // SAFETY: plain prctl; fails harmlessly (EINVAL) without Yama.
    unsafe {
        libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mem_probe_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove(SERVER_ENV)
        .env(PROBE_ENV, "1")
        .output()
        .expect("probe runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.split_once("probe: ").map(|(_, v)| v.trim()))
        .expect("the probe reports");
    // SAFETY: plain prctl.
    let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    println!("server: dumpable={dumpable} {line}");
}

/// What the probe saw when its parent was in `mode`.
fn probe_under(mode: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server_like_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SERVER_ENV, mode)
        .output()
        .expect("server child runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "server child failed: {stdout}");
    stdout
        .lines()
        .find_map(|l| l.split_once("server: ").map(|(_, v)| v.trim()))
        .expect("the server child reports")
        .to_string()
}

/// Whether this process has `CAP_SYS_PTRACE`, which overrides dumpability
/// (a test run as full root): the memory checks then prove nothing.
fn has_cap_sys_ptrace() -> bool {
    let caps = status_field(&own_status(), "CapEff")
        .and_then(|v| u64::from_str_radix(&v, 16).ok())
        .unwrap_or(0);
    caps & (1 << 19) != 0
}

#[test]
fn a_child_cannot_open_a_hardened_servers_memory() {
    // The control shows the hole the flag closes; whether it is open
    // depends on the kernel (Yama's ptrace_scope) and the user's
    // capabilities, so it is reported, not asserted.
    if has_cap_sys_ptrace() {
        eprintln!("skipped: this process has CAP_SYS_PTRACE");
        return;
    }
    // EACCES alone could come from Yama (ptrace_scope 1+, the Ubuntu
    // default), so the cause is asserted too: the parent is non-dumpable.
    let plain = probe_under("plain");
    eprintln!("a child of a dumpable parent: {plain}");
    assert!(plain.starts_with("dumpable=1 "), "{plain}");
    assert_eq!(
        probe_under("hardened"),
        format!("dumpable=0 open-err {}", libc::EACCES),
        "a child must not open a non-dumpable parent's memory"
    );
}

#[test]
fn the_real_server_makes_itself_non_dumpable() {
    if has_cap_sys_ptrace() {
        eprintln!("skipped: this process has CAP_SYS_PTRACE");
        return;
    }
    // serve-stdio waits on stdin, so it stays up until stdin closes.
    let mut server = Command::new(env!("CARGO_BIN_EXE_pscat-mcp-server"))
        .arg("serve-stdio")
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("server starts");
    use std::os::unix::fs::MetadataExt as _;
    let path = format!("/proc/{}/mem", server.id());
    // SAFETY: plain system call.
    let we_are_root = unsafe { libc::getuid() } == 0;
    // The kernel gives a non-dumpable process's /proc files to root, so
    // for a non-root test the owner shows the cause of the EACCES (rather
    // than Yama). As root, the owner is root either way; the EACCES then
    // comes from dumpability (root without CAP_SYS_PTRACE passes Yama's
    // check only for its descendants, which the server is).
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = loop {
        let owner = std::fs::metadata(&path).map(|m| m.uid()).ok();
        match std::fs::File::open(&path) {
            Err(e)
                if e.raw_os_error() == Some(libc::EACCES) && (we_are_root || owner == Some(0)) =>
            {
                break Ok(());
            }
            other if Instant::now() >= deadline => {
                break Err(format!("{other:?}, owner {owner:?}"));
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let _ = server.kill();
    let _ = server.wait();
    result.expect("the server's memory stays open to its parent");
}
