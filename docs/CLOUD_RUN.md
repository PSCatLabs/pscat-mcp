# Deploying to Cloud Run

Infrastructure (Terraform, IAM, monitoring) belongs in `PSCatLabs/pscat-cloud`.
This document lists what that deployment must get right for this image.

## Image

* The `Container image` GitHub Actions workflow builds `linux/amd64`, runs
  `scripts/container-smoke.sh` against that exact image, and publishes it to
  `ghcr.io/pscatlabs/pscat-mcp` on pushes to `main` and on `v*` tags. Tags:
  * `sha-<commit>`
  * `main`
  * `latest`
  * `<version>` (from a `v<version>` git tag)
  * `pscat-<pscat version>`

  Pull requests build and test the image but never push.
* Deploy by digest or by `sha-` / version tag, not by `latest`.
* Cloud Run pulls from Artifact Registry. To use the GHCR image, create an
  Artifact Registry *remote repository* whose upstream is `https://ghcr.io`
  and deploy
  `LOCATION-docker.pkg.dev/PROJECT/REPO/pscatlabs/pscat-mcp:TAG`. The
  alternative is to copy the image into a standard Artifact Registry
  repository (`docker pull`, `docker tag`, `docker push`). That setup
  belongs in `pscat-cloud`.
* A new GHCR package is private. Do one of these:
  * make it public: *Package settings → Change visibility*, matching the
    public repository;
  * keep it private, and give the remote repository a GitHub token with
    `read:packages`, stored in Secret Manager.
* To build locally for Cloud Run, run
  `docker buildx build --platform linux/amd64 .`. The local smoke test
  builds for the host architecture.
* The container listens on `0.0.0.0:$PORT` (Cloud Run sets `PORT`), runs as
  uid 65532, and needs no writable filesystem or volumes.
* Startup and liveness probe: `GET /healthz` (unauthenticated, does no work).

## Required configuration

| Setting | Why |
|---|---|
| `PSCAT_MCP_AUTH_TOKEN_FILE=/secrets/tokens` with a Secret Manager volume | the server refuses to start without a token; a file keeps it out of `env` listings |
| `PSCAT_MCP_ALLOWED_HOSTS=<service>.run.app,<custom domain>` | the default allows only loopback hosts, so every `/mcp` request would get 403 (the server logs a warning at startup). `*` is acceptable only when a load balancer already validates `Host` |
| Cloud Run ingress / IAM | the bearer token is the application's own check. Cloud Run IAM (`roles/run.invoker`) can be layered in front for service-to-service callers, but MCP clients such as ChatGPT or Claude cannot mint Google ID tokens, so public MCP access needs `--allow-unauthenticated` at the Cloud Run level plus this server's token or OAuth |

## Sizing

* **Concurrency.** Each render is one worker process using up to
  `PSCAT_MCP_MAX_MEMORY_BYTES` of heap (512 MiB by default) plus the page
  canvas. Set Cloud Run `--concurrency` to cover a few times
  `PSCAT_MCP_MAX_CONCURRENT_RENDERS`: `initialize`, `tools/list` and health
  checks are cheap, and extra renders queue for up to
  `PSCAT_MCP_QUEUE_TIMEOUT_MS`, then get `server_busy`.
* **Memory.** About `max_concurrent_renders × (max_memory + 300 MiB)` plus
  50 MiB for the server. With the defaults and 2 renders: 2 GiB. Lower
  `PSCAT_MCP_MAX_MEMORY_BYTES` for smaller instances.
* **CPU.** One vCPU per concurrent render. Rendering is CPU-bound and
  single-threaded.
* **Request timeout.** The Cloud Run request timeout must exceed
  `PSCAT_MCP_RENDER_TIMEOUT_MS` + `PSCAT_MCP_QUEUE_TIMEOUT_MS` (30 s by
  default). 60 s is a good default.
* **CPU allocation.** Keep "CPU only during requests" (the default). The
  server does no background work.

## Shutdown

On SIGTERM the server stops accepting connections and waits up to
`PSCAT_MCP_SHUTDOWN_GRACE_SECS` (default 8; Cloud Run allows 10) for
in-flight renders, then cancels whatever remains. Worker processes die with
the instance's container.

## Sandbox

Use the **first-generation execution environment** (gVisor) for public
traffic. It adds a kernel-level sandbox around the worker processes, beyond
the rlimits and PSCat's own confinement. Second-generation instances run on
a regular Linux kernel, where the worker's `RLIMIT_AS` applies but nothing
else isolates its system calls.

## Logging and metrics

Logs are JSON on stderr: one line per request with `request_id`, method,
path, status and latency, and one line per render with source byte count,
steps, elapsed ms, pages and failure flag. PostScript source and tokens are
never logged, except in the opt-in capture mode (`PSCAT_MCP_CAPTURE_FILE`),
which only starts on a loopback bind and so never in a deployment. No per-user or per-program labels are emitted, so cardinality
stays low.
