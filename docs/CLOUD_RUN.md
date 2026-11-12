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
  `PSCAT_MCP_MAX_MEMORY_BYTES` for smaller instances. A `review_sheet` is one
  render slot and fits the same figure at the default sheet size: while a
  cell runs, its worker holds the cell's interpreter (as a render does: its
  page and up to `max_memory` of growth), plus the sheet's canvas (4 bytes a
  pixel, 64 MB at the default 16,000,000 sheet pixels) and the label strips
  (about 10 MB at most); once the cells are done, it holds the canvas and
  up to two more copies of it while the PNG is encoded. A cell's memory
  limit counts only its own program's heap growth, so the canvas never
  makes a cell fail. Raising `PSCAT_MCP_MAX_SHEET_PIXELS` (at most
  64,000,000) adds 4 bytes a sheet pixel above 16,000,000 to the figure (at
  64,000,000: about 200 MB more per sheet). The worker's address-space
  limit on Linux, `max_memory + 1 GiB` by default, covers it either way.
  With async jobs on (`PSCAT_MCP_ASYNC_JOBS`, see below), add
  `PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES` (128 MiB by default): job results
  waiting to be fetched live in the server process, not in a worker.
* **CPU.** One vCPU per concurrent render. Rendering is CPU-bound and
  single-threaded.
* **Request timeout.** The Cloud Run request timeout must be more than
  max(`PSCAT_MCP_RENDER_TIMEOUT_MS`, `PSCAT_MCP_MAX_SHEET_TIMEOUT_MS`) +
  `PSCAT_MCP_QUEUE_TIMEOUT_MS` (30 s by default: the sheet deadline
  defaults to the render timeout). 60 s is a good default.
* **CPU allocation.** Keep "CPU only during requests" (the default). The
  server does no background work unless async jobs are on, which this
  deployment should not do (see below).

## Shutdown

On SIGTERM the server stops accepting connections and waits up to
`PSCAT_MCP_SHUTDOWN_GRACE_SECS` (default 8; Cloud Run allows 10) for
in-flight renders, then cancels whatever remains. Worker processes die with
the instance's container. Async jobs, if on, are killed after the grace
period; their results are lost.

## Async render jobs

`start_render`, `render_status`, `render_result`, `cancel_render` and the
MCP Tasks extension keep their jobs and results in the server's memory. On
Cloud Run that does not work:

* a status or result call can reach another instance, or a new one after
  scale-to-zero, which has never heard of the job (`unknown_job`);
* with "CPU only during requests", a job running between polls gets almost
  no CPU;
* a restart or a scale-in loses every job.

So they are **off by default over HTTP** (`PSCAT_MCP_ASYNC_JOBS=false`): the
four tools are not listed and the Tasks extension is not advertised. Turn
them on only for a single instance whose CPU stays allocated: one machine,
or a Cloud Run service with `--max-instances 1`, `--min-instances 1` and
CPU always allocated (and sized for `PSCAT_MCP_MAX_RUNNING_JOBS` workers of
up to the memory limit each, beside the synchronous ones; results add up to
`PSCAT_MCP_JOB_RESULTS_BUDGET_BYTES`, of which one token may hold
`PSCAT_MCP_JOB_RESULTS_PER_PRINCIPAL_BYTES`: the budget less one largest
result by default, so lower it when several tokens share the service). A hosted design (a shared job and
result store, and a place the render gets CPU, such as a Cloud Run Job) is
tracked in the `pscat-cloud` brief, not here.

## Font packs

The image has PSCat's compiled-in fonts only, and the worker's empty
environment does not find packs installed anywhere else. To add the `extras`
or `cjk-thai` pack (Korean, Japanese and Thai labels in `review_sheet`, and
programs that select a pack face):

1. Install the packs into a directory with the PSCat CLI, outside the image:
   `PSCAT_HOME=/srv/pscat-data pscat fonts install --pack cjk-thai`. The
   result is `/srv/pscat-data/fonts/packs/cjk-thai/`.
2. Put that directory on a **read-only volume** (a Cloud Storage bucket
   mount with `readonly=true`, or an NFS/Filestore share mounted read-only)
   and mount it in the container, for example at `/fonts`.
3. Set `PSCAT_MCP_FONT_PACK_DIR=/fonts`. The path is the data root, the
   directory that *contains* `fonts/packs/`, not `fonts/packs` itself.

The image runs as uid 65532 and needs read access only. The server refuses to
start if the path is not a directory, so a missing mount shows at startup
rather than as `?` labels. `describe_art_capabilities` lists the packs the
workers found, and `get_renderer_info` reports their names. The directory is
not added to what programs can read; see "The font-pack directory" in
[ARCHITECTURE.md](ARCHITECTURE.md#the-font-pack-directory).

Plan for memory: a face is read whole into a worker when a program first
selects it (Noto Sans JP is 9.5 MB), counted against
`PSCAT_MCP_MAX_MEMORY_BYTES`. Network-backed volumes are slower to read on a
cold instance than a baked-in file would be.

**Not done here: baking packs into the image.** It would remove the mount,
at a cost: `cjk-thai` is large (about 70 MB of the four Noto and Nanum faces),
so the image, its pull time and its cold start grow, and every pack-train
bump rebuilds it. The packs are SIL OFL 1.1 (each pack carries its
`manifest.json` and the upstream licence files, which must ship with the
fonts), so redistributing them in the image is allowed, but the image would
then carry a second set of licences. Follow-up: an optional build argument
that runs `pscat fonts install` in a build stage and copies the
`fonts/packs` tree into the final image, with `PSCAT_MCP_FONT_PACK_DIR` set
to it by default.

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
