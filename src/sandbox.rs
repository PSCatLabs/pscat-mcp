//! Process isolation between the server and its render workers (Linux
//! only): the worker's system-call filter ([`install`]) and the server's
//! non-dumpable flag ([`harden_server`]).
//!
//! The worker shares the server's network namespace, and on Cloud Run the
//! metadata server (169.254.169.254, which hands out the service account's
//! access token) is reachable from every process in it. PostScript has no
//! network operators, but a code-execution bug in the interpreter would run
//! with the worker's system calls. So on Linux the worker installs a seccomp
//! filter before it reads its job, and the calls below fail with `EPERM`:
//!
//! * `socket`, `socketpair` (every address family: the worker talks to the
//!   server over the stdin/stdout pipes it inherits and needs no socket, not
//!   even `AF_UNIX`), and `connect`, `bind`, `listen`, `accept`, `accept4`
//!   as defence in depth;
//! * `io_uring_setup`, `io_uring_enter`, `io_uring_register`: io_uring can
//!   open and connect sockets itself, and its operations are not seen by
//!   seccomp;
//! * `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`: the
//!   worker runs as the server's user, so where the kernel allows it these
//!   could read or write the server's memory or copy its file descriptors
//!   (its sockets) into the worker;
//! * `bpf`.
//!
//! Every system call of a foreign architecture fails too (32-bit calls on a
//! 64-bit kernel), and on x86_64 every x32 call, since those carry the same
//! architecture tag with other numbers.
//!
//! The filter also sets `PR_SET_NO_NEW_PRIVS`, which lets an unprivileged
//! process install it. It is kept by threads the worker starts later and
//! across `execve`, and cannot be removed. A denied call returns `EPERM`
//! rather than killing the worker: glibc, for example, may try the nscd
//! socket for a user lookup and falls back to the files when that fails. No
//! log line is written for a denied call; the caller sees the error.
//!
//! If the filter cannot be installed the worker does not run the job: there
//! is no opt-out. On other systems (macOS, for development) [`install`] does
//! nothing.
//!
//! Seccomp sees system-call numbers, not paths, so it cannot stop the worker
//! opening `/proc/<server pid>/mem` and reading or writing the server's
//! memory (tokens included), which the kernel allows a process of the same
//! user when Yama's `ptrace_scope` is 0 (as in Docker Desktop). The server
//! closes that by making itself non-dumpable ([`harden_server`]): the kernel
//! then requires `CAP_SYS_PTRACE` for that access, which the container
//! drops. It also means the server never writes a core dump.

/// What the worker's system-call filter is, as `get_renderer_info` reports
/// it (`sandbox.worker_filter`) and as a worker names it in its job
/// response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkerFilter {
    /// The seccomp filter described above is in force (Linux).
    Seccomp,
    /// There is no filter: this system has none (macOS, for development).
    None,
    /// No worker has run yet, so the server does not know. Never sent by a
    /// worker; `get_renderer_info` reports it until the first job ends.
    Unknown,
}

impl WorkerFilter {
    /// The fixed token a worker puts in its job response, and the value
    /// `get_renderer_info` reports.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerFilter::Seccomp => "seccomp",
            WorkerFilter::None => "none",
            WorkerFilter::Unknown => "unknown",
        }
    }

    /// The filter a worker's token names. Only the two tokens a worker can
    /// send are accepted; anything else is `None`.
    pub fn from_token(token: &str) -> Option<WorkerFilter> {
        match token {
            "seccomp" => Some(WorkerFilter::Seccomp),
            "none" => Some(WorkerFilter::None),
            _ => None,
        }
    }
}

/// Install the worker's system-call filter in the calling process.
///
/// Must be called while the process has a single thread: a seccomp filter
/// covers the calling thread and the threads it starts afterwards, not
/// threads that already exist. The `render-worker` subcommand calls it
/// first thing, before reading its job; PSCat's band threads start later
/// and inherit it.
///
/// On Linux the filter is checked after installing (a socket must be
/// refused, an ordinary call must still work), so an `Ok` means it is in
/// force and the answer is [`WorkerFilter::Seccomp`]. Elsewhere this does
/// nothing and the answer is [`WorkerFilter::None`].
pub fn install() -> Result<WorkerFilter, String> {
    imp::install()
}

/// Make the server process non-dumpable (`PR_SET_DUMPABLE` 0), so a
/// process of the same user, a compromised worker included, can neither
/// open its `/proc/<pid>/mem` (nor `environ`, `fd`, ...) nor `ptrace` it
/// without `CAP_SYS_PTRACE`; it also turns off the server's core dumps.
///
/// Called by `serve-http` and `serve-stdio` before the runtime starts and
/// before any worker exists. Workers are not affected: `execve` makes the
/// worker dumpable again, which only exposes the worker to the server.
/// Elsewhere than Linux this does nothing and returns `Ok`
/// ([`server_dumpable`] then stays `true`).
pub fn harden_server() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain prctl calls.
        unsafe {
            if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
                return Err(format!(
                    "prctl(PR_SET_DUMPABLE, 0) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) != 0 {
                return Err("the server is still dumpable after PR_SET_DUMPABLE 0".into());
            }
        }
    }
    Ok(())
}

/// Whether this process is dumpable, read from the kernel (`PR_GET_DUMPABLE`)
/// each time, so it is what is in force and not what was asked for.
/// `false` after [`harden_server`] on Linux. Always `true` elsewhere: there
/// is no such flag, and [`harden_server`] does nothing.
pub fn server_dumpable() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a plain prctl query.
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) != 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use libc::{c_long, sock_filter, sock_fprog};

    /// `AUDIT_ARCH_*` from `<linux/audit.h>` (not in the `libc` crate).
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!(
        "the worker's seccomp filter knows x86_64 and aarch64 only; add this \
         architecture's AUDIT_ARCH value and check its system-call numbers"
    );

    /// x86_64: system-call numbers with this bit set are x32 calls.
    #[cfg(target_arch = "x86_64")]
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;

    /// The denied calls; numbers come from `libc` for the target, so they
    /// are right for each architecture.
    const DENIED: &[c_long] = &[
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_bpf,
    ];

    const DENY: u32 = libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA);

    const fn stmt(code: u32, k: u32) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    const fn jump(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    /// The BPF program: check the architecture, then the call number.
    pub(super) fn program() -> Vec<sock_filter> {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};
        let arch_offset = std::mem::offset_of!(libc::seccomp_data, arch) as u32;
        let nr_offset = std::mem::offset_of!(libc::seccomp_data, nr) as u32;

        let mut checks: Vec<(u32, u32)> = Vec::new(); // (BPF jump op, value)
        #[cfg(target_arch = "x86_64")]
        checks.push((libc::BPF_JGE, X32_SYSCALL_BIT));
        for &nr in DENIED {
            checks.push((
                BPF_JEQ,
                u32::try_from(nr).expect("system-call numbers are small"),
            ));
        }

        let mut p = Vec::with_capacity(checks.len() + 6);
        p.push(stmt(BPF_LD | BPF_W | BPF_ABS, arch_offset));
        // Foreign architecture: skip to the final deny.
        let after_arch = checks.len() + 2; // the nr load, the checks, the allow
        p.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            AUDIT_ARCH,
            0,
            u8::try_from(after_arch).expect("short filter"),
        ));
        p.push(stmt(BPF_LD | BPF_W | BPF_ABS, nr_offset));
        let n = checks.len();
        for (i, (op, value)) in checks.into_iter().enumerate() {
            // A match jumps over the remaining checks and the allow.
            let to_deny = u8::try_from(n - i).expect("short filter");
            p.push(jump(BPF_JMP | op | BPF_K, value, to_deny, 0));
        }
        p.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW));
        p.push(stmt(BPF_RET | BPF_K, DENY));
        p
    }

    fn os_error(what: &str) -> String {
        format!("{what} failed: {}", std::io::Error::last_os_error())
    }

    pub(super) fn install() -> Result<super::WorkerFilter, String> {
        let mut filter = program();
        let prog = sock_fprog {
            len: u16::try_from(filter.len()).expect("short filter"),
            filter: filter.as_mut_ptr(),
        };
        // SAFETY: plain prctl calls; `prog` points at `filter`, which
        // outlives the call (the kernel copies the program).
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(os_error("prctl(PR_SET_NO_NEW_PRIVS)"));
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &prog as *const sock_fprog,
            ) != 0
            {
                return Err(os_error("prctl(PR_SET_SECCOMP)"));
            }
        }
        verify()?;
        Ok(super::WorkerFilter::Seccomp)
    }

    /// Check that the filter is in force: a socket is refused with EPERM
    /// and an allowed call still works.
    fn verify() -> Result<(), String> {
        // SAFETY: plain system calls on no memory of ours; a descriptor
        // that should not exist is closed.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd >= 0 {
                libc::close(fd);
                return Err("the filter is installed but a socket was still created".into());
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EPERM) {
                return Err(format!(
                    "the filter is installed but socket() failed with {err}, not EPERM"
                ));
            }
            if libc::fcntl(2, libc::F_GETFD) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            {
                return Err("the filter denies an ordinary call (fcntl); is this \
                            binary running under emulation of another architecture?"
                    .into());
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub(super) fn install() -> Result<super::WorkerFilter, String> {
        Ok(super::WorkerFilter::None)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::imp::program;

    #[test]
    fn the_program_ends_with_allow_then_deny_and_every_jump_lands_inside() {
        let p = program();
        let len = p.len();
        assert_eq!(p[len - 2].k, libc::SECCOMP_RET_ALLOW);
        assert_eq!(
            p[len - 1].k & !libc::SECCOMP_RET_DATA,
            libc::SECCOMP_RET_ERRNO
        );
        assert_eq!(p[len - 1].k & libc::SECCOMP_RET_DATA, libc::EPERM as u32);
        for (i, ins) in p.iter().enumerate() {
            if u32::from(ins.code) & 0x07 == libc::BPF_JMP {
                // A conditional jump's targets are relative to the next one.
                for off in [ins.jt, ins.jf] {
                    assert!(i + 1 + usize::from(off) < len, "jump out of range at {i}");
                }
                // Every match lands on the deny.
                if ins.jt != 0 {
                    assert_eq!(i + 1 + usize::from(ins.jt), len - 1, "at {i}");
                }
            }
        }
        // The architecture mismatch also lands on the deny.
        assert_eq!(1 + 1 + usize::from(p[1].jf), len - 1);
    }
}

#[cfg(test)]
mod token_tests {
    use super::WorkerFilter;

    #[test]
    fn a_worker_can_send_only_seccomp_or_none() {
        assert_eq!(
            WorkerFilter::from_token("seccomp"),
            Some(WorkerFilter::Seccomp)
        );
        assert_eq!(WorkerFilter::from_token("none"), Some(WorkerFilter::None));
        for other in ["unknown", "Seccomp", "", "seccomp ", "a worker's own text"] {
            assert_eq!(WorkerFilter::from_token(other), None, "{other:?}");
        }
        for f in [WorkerFilter::Seccomp, WorkerFilter::None] {
            assert_eq!(WorkerFilter::from_token(f.as_str()), Some(f));
        }
        assert_eq!(
            serde_json::to_string(&WorkerFilter::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
