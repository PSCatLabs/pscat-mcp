# Architecture

PSCat MCP is a thin, stateless MCP server in front of the
[PSCat](https://github.com/PSCatLabs/pscat) PostScript interpreter. This
document explains how a request becomes a render, where the security
boundaries are, and where to extend it.

## Request path

```
 MCP client
    │  Streamable HTTP (/mcp)            stdio
    ▼                                      ▼
 http.rs ── auth.rs (bearer) ──┐        mcp::serve_stdio
    request id, tracing,       │           │
    body limit, Host/Origin    ▼           ▼
                         mcp.rs  (rmcp #[tool] handlers: translate args/results only)
                               │
                               ▼
                         render.rs  RenderService
                           • validate_request: size, format, dpi, page px, pixel cap
                           • semaphore: bounded concurrent renders + queue timeout
                           • maps worker/backend failures to stable ErrorCodes
                               │  WorkerJob (JSON)
                               ▼
                         backend.rs  SubprocessBackend
                           • spawn `pscat-mcp-server render-worker` per job
                           • env cleared, cwd "/", rlimits, own process group
                           • stdout/stderr caps, deadline → SIGKILL group + reap
                               │  stdin: job   stdout: WorkerResult (JSON)
                               ▼
                         worker.rs  (child process, no async runtime)
                           • pscat::Interp with Limits (steps/time/memory)
                           • reads confined to PSCat's compiled-in lib/
                           • no write roots, stdout captured, stderr silenced
                           • PNG / SVG / PDF encoding, page + byte caps
```

On HTTP the `/mcp` stack, outermost first, is: request id and tracing,
authentication, the body limit, the optional capture layer (`capture.rs`,
off unless `--capture-file` is set; it records each authenticated request
just before rmcp sees it, without tokens), then rmcp. `render.rs` logs one
`render started` line per job it hands to a worker; the server-layer evals
count those lines to assert how many renders a scenario executed.

Every layer below `mcp.rs` is transport independent. A REST API would be a
second caller of `RenderService` and reuse all validation, limits and error
codes.

## The renderer adapter

PSCat is used as a **Rust library** (`pscat` crate, git dependency pinned
to an exact commit, `default-features = false` so no window system is
linked). The supported API (`Interp`, `Page`, `PsError`, `Limits`) does all
rendering. Three `#[doc(hidden)]` PSCat modules are also used. All of them are
called from `src/pscat_extras.rs` and nowhere else (a PSCat bump that breaks
one is fixed in that file; upstream tracking issue
[PSCatLabs/pscat#1098](https://github.com/PSCatLabs/pscat/issues/1098)). They
are the first thing to re-check when bumping the pin:

* `pscat::lexer` — tokenizer pass for `validate_postscript`
  (PSCatLabs/pscat#1096).
* `pscat::gfx::PageSizeSource` — so a non-72-dpi default page still lets a
  program choose its size with `setpagedevice`, like `Interp::new()`
  (PSCatLabs/pscat#1097).
* `pscat::capabilities` — the art catalog behind `describe_art_capabilities`.
  It runs no PostScript. It lists fonts by scanning font directories, so the
  worker computes it (`JobMode::Capabilities`): the worker's environment is
  cleared and its working directory is `/`, which is what renders see, and the
  scan runs under the same deadline and rlimits as a render. It takes a
  render slot. `RenderService` keeps the parsed catalog after the first
  successful fetch (the worker's environment is fixed, so it is too; a failed
  fetch is not kept), so later calls start no worker. The font-pack status
  contains directories, which `pscat_extras` removes. The catalog is about
  280 KB, so `src/catalog.rs` filters it in the server into three bounded
  views (index, one kind's names with truncated descriptions, full entries
  for up to 20 names within a byte budget), each under 24 KB as structured
  content and as text.

The library runs **inside a child process**, not in the server, because:

1. PSCat checks its limits *between* interpreter steps. One long operator (a
   huge fill, a deep path) runs to completion, so only an external kill can
   guarantee a deadline.
2. PSCat's memory limit measures heap growth of the whole process through
   `pscat::TrackingAllocator`. With one program per process the number is
   exact; with concurrent in-process renders it would be meaningless.
3. `pscat::Interp` is `!Send`; a process per job sidesteps thread pinning.
4. OS limits (`setrlimit`) and a crash (stack overflow, abort) are contained
   to the job.

The cost is a process start per render (a few milliseconds; the worker is
the same binary). A pre-forked pool is a possible later optimisation.

## Execution isolation and limits

| Control | Enforced by | Default |
|---|---|---|
| Source size | server, before execution | 256 KiB |
| Format, dpi (9–1200), page ≤ 8000 px a side | server, before execution | — |
| Device pixels of the starting page | server, before execution | 16 M |
| Pages created by `setpagedevice` | PSCat (`set_max_page_pixels`) | 16 M |
| Interpreter steps | PSCat `Limits::max_steps` | 5·10⁹ |
| Interpreter time | PSCat `Limits::max_time` | timeout − 10% |
| Heap growth | PSCat `Limits::max_memory` (counting allocator) | 512 MiB |
| Wall-clock deadline | server: SIGKILL of the worker's process group | 20 s |
| CPU time | kernel `RLIMIT_CPU` | deadline + 2 s |
| Address space | kernel `RLIMIT_AS` (**Linux only**) | memory + 1 GiB |
| File writes | PSCat (no write roots) + kernel `RLIMIT_FSIZE=0` | none allowed |
| File reads | PSCat read roots: only compiled-in `lib/` | — |
| Open files | kernel `RLIMIT_NOFILE` | 64 |
| Output bytes | worker (artifact cap) + server (stdout cap, kill) | 8 MiB |
| Pages returned | worker | 10 |
| Program stdout returned | worker (truncates) | 16 KiB |
| Concurrent renders | server semaphore, queue timeout → `server_busy` | min(CPUs, 4) |
| Client cancel / disconnect | dropping the future kills the worker (`kill_on_drop`) | — |

PSCat has **no operators for processes, network or environment access**
(see PSCat's `SECURITY.md`). The worker's environment is cleared anyway
(only `PSCAT_NO_CHECKOUT=1` is set), so nothing secret is inherited.

Known gaps:

* `RLIMIT_AS` is applied on Linux only. On macOS the memory bound is PSCat's
  allocator limit plus the deadline; a single huge allocation inside one
  operator can overshoot before it is seen.
* If the server process itself is SIGKILLed, a running worker survives
  until its `RLIMIT_CPU` or its own time limit stops it (seconds). In a
  container the whole cgroup is torn down with the instance.
* The worker shares the container's network namespace. PSCat cannot open
  sockets, but defence in depth (seccomp, a network-less sandbox such as
  gVisor, which Cloud Run gen1 provides) is recommended for public use.

## Security boundaries

* **Authentication** (`auth.rs`): every `/mcp` request passes an
  `Authenticator` before rmcp sees it. The provided one checks
  `Authorization: Bearer <token>` against SHA-256 digests of configured
  tokens in constant time. Unauthenticated mode exists for local development
  and the server refuses to start with it on a non-loopback address.
  `/healthz` is unauthenticated and does no work.
* **DNS rebinding / browsers**: rmcp's `Host` allow-list (loopback by
  default; set `PSCAT_MCP_ALLOWED_HOSTS` for a deployment) and `Origin`
  validation.
* **Logging**: PostScript source is never logged, only its byte length;
  tokens are never logged. The one exception is the opt-in capture mode
  (`--capture-file`), which records request bodies and tokens never, and is
  refused unless the bind address is loopback.
* **Statelessness**: Streamable HTTP runs without sessions
  (`legacy_session_mode = false`, JSON responses), so any instance can serve
  any request and nothing is stored between requests.

### Adding OAuth later

The MCP authorization spec makes the server an OAuth 2.1 *resource
server*. To add it:

1. Implement `Authenticator` to validate access tokens (JWT signature and
   `aud`/`iss`/`exp`, or token introspection) issued by an external
   authorization server (e.g. Google Identity Platform, Auth0).
2. Serve `/.well-known/oauth-protected-resource` (RFC 9728) naming that
   authorization server, and return `WWW-Authenticate: Bearer
   resource_metadata="…"` on 401 so clients can discover it.
3. Pass the authenticated principal to handlers through request extensions
   if per-user quotas are needed.

No user database or proprietary OAuth flow is needed in this repository.

## Extension points

* **Another transport** (REST): call `RenderService::render`/`validate`.
* **Another backend** (a pool, a remote sandbox, a test double): implement
  `backend::RenderBackend`. Dropping the returned future must stop the job.
* **Artifact storage**: none is configured, so output larger than
  `PSCAT_MCP_MAX_OUTPUT_BYTES` fails with `output_too_large` instead of
  returning a link. A storage provider (e.g. GCS with signed URLs) would sit
  between `RenderService` and `mcp.rs`, returning MCP `resource_link`
  content only for objects it actually stored.
* **Authentication**: implement `auth::Authenticator`.
