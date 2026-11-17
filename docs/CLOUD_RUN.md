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
* Health paths: `GET /healthz` and `GET /health` are the same handler
  (unauthenticated, does no work, `{"status":"ok"}`). Use `/healthz` for the
  in-container startup and liveness probes. Use `/health` for anything
  outside Cloud Run, such as a Cloud Monitoring uptime check, an e2e
  pipeline or `curl`: Google's front end reserves URL paths that end in `z`
  and returns 404 for `/healthz` from outside the service, so the request
  never reaches the container. The in-container probes do reach `/healthz`.
  Check from outside with
  `curl -s -o /dev/null -w '%{http_code}' https://<service>.run.app/health`,
  which prints `200` with no token.

## Required configuration

| Setting | Why |
|---|---|
| `PSCAT_MCP_AUTH_TOKEN_FILE=/secrets/tokens` with a Secret Manager volume | required when `PSCAT_MCP_AUTH` is unset or lists `bearer`: the server then refuses to start without a token; a file keeps it out of `env` listings. An `oauth`-only deployment needs no token (see "OAuth" below), nor does the staging `none` mode below |
| `PSCAT_MCP_ALLOWED_HOSTS=<service>.run.app,<custom domain>` | the default allows only loopback hosts, so every `/mcp` request would get 403 (the server logs a warning at startup). `*` is acceptable only when a load balancer already validates `Host` |
| `PSCAT_MCP_MAX_CONCURRENT_RENDERS=<the service's --cpu>` (for example `2` with `--cpu=2`) | the default is min(CPUs the process sees, 4), and on the first-generation environment (gVisor) that count can exceed what the service is granted: `--cpu=2` was observed to get 4 slots. Set it explicitly; see "Sizing" |
| Cloud Run ingress / IAM | the bearer token (or OAuth, see below) is the application's own check. Cloud Run IAM (`roles/run.invoker`) can be layered in front for service-to-service callers, but MCP clients such as ChatGPT or Claude cannot mint Google ID tokens, so public MCP access needs `--allow-unauthenticated` at the Cloud Run level plus this server's token or OAuth (or, for staging only, the `none` mode below) |

## OAuth (ChatGPT and claude.ai connectors)

Optional: `PSCAT_MCP_AUTH=oauth` or `bearer,oauth` makes `/mcp` accept JWT
access tokens from an external authorization server (see the README's
"Authentication" section and docs/ARCHITECTURE.md, "OAuth resource
server"). The identity provider is not chosen yet, so nothing here names
one. Everything the deployment wires:

| Setting | Kind | Value |
|---|---|---|
| `PSCAT_MCP_AUTH` | env | `bearer,oauth` (bearer for Claude Code, scripts and CI; OAuth for ChatGPT and claude.ai), or `oauth` |
| `PSCAT_MCP_OAUTH_ISSUER` | env, not secret | the provider's issuer URL exactly as it appears in tokens' `iss` (mind a trailing `/`) |
| `PSCAT_MCP_OAUTH_RESOURCE` | env, not secret | the public URL clients connect to, e.g. `https://<custom domain>/mcp`; tokens' `aud` must contain it. Use the custom domain the connectors are given, not the `run.app` URL, unless that is what users type |
| `PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE` | Secret Manager volume (personal data, not a credential) | allowlist, one `sub:<subject>` or `email:<address>` per line; or `PSCAT_MCP_OAUTH_ALLOWED_USERS` (comma list, env). Required |
| `PSCAT_MCP_OAUTH_SCOPES` | env, optional | scopes every token must carry, comma list |
| `PSCAT_MCP_OAUTH_JWKS_URL` | env, optional | only if discovery (RFC 8414 / OpenID Connect) does not work for the provider |
| `PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS`, `PSCAT_MCP_OAUTH_JWKS_TTL_SECS`, `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS`, `PSCAT_MCP_OAUTH_LEEWAY_SECS` | env, optional | defaults 5000, 600, 30, 60 |

* **No secret is needed by the server for OAuth**: it only verifies
  signatures with the provider's public keys. Client IDs and secrets, if
  the provider has no Dynamic Client Registration, belong to the connector
  configuration in ChatGPT or claude.ai, not to this service.
* **New unauthenticated paths**, served only with `oauth` enabled:
  `GET /.well-known/oauth-protected-resource` and
  `GET /.well-known/oauth-protected-resource/<resource path>` (for
  `.../mcp`: `/.well-known/oauth-protected-resource/mcp`). They return a
  fixed JSON document from the settings above (CORS `*`), do no work, and
  must reach the container: don't put them behind IAM or an ingress rule
  that needs auth. `/health` and `/healthz` stay as before.
* **Outbound HTTPS to the provider** is required: the server fetches the
  provider's metadata and JWKS (cached, at most one fetch per
  `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS`). With VPC egress set to all traffic
  and no Cloud NAT, every JWT fails with `401` (`oauth: token rejected`,
  `reason=jwks_unavailable`). Roots are compiled into the binary, so the
  image's CA bundle does not matter. Static bearer tokens never need the
  provider.
* **Startup does not contact the provider**: an outage cannot crash-loop the
  service; it only makes JWTs with keys not yet fetched fail.
* **What is not done yet**: no ChatGPT or claude.ai connector has signed in
  against a deployment. That acceptance check needs a chosen identity
  provider and a staging deployment.

## Sizing

* **Concurrency.** Each render is one worker process using up to
  `PSCAT_MCP_MAX_MEMORY_BYTES` of heap (512 MiB by default) plus the page
  canvas. Set Cloud Run `--concurrency` to cover a few times
  `PSCAT_MCP_MAX_CONCURRENT_RENDERS`: `initialize`, `tools/list` and health
  checks are cheap, and extra renders queue for up to
  `PSCAT_MCP_QUEUE_TIMEOUT_MS`, then get `server_busy`.
* **Memory.** About `max_concurrent_renders × (max_memory + 300 MiB)` plus
  50 MiB for the server. With the defaults and 2 renders: about 1.6 GiB, so
  2 GiB (`--memory=2Gi`). With 4 renders: about 3.2 GiB, more than that
  instance has, so a burst of heavy renders could get it killed for running
  out of memory. Lower
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
  single-threaded. **Set `PSCAT_MCP_MAX_CONCURRENT_RENDERS` explicitly, equal
  to the service's `--cpu`.** Unset, it defaults to min(CPUs the process
  sees, 4). On the first-generation environment (gVisor, recommended below)
  the process can see more CPUs than the service is granted: a staging
  service with `--cpu=2 --memory=2Gi` reported `max_concurrent_renders: 4`
  in `get_renderer_info`. That gives 4 slots on 2 vCPUs and, by the formula
  above, needs about 4 × (512 MiB + 300 MiB) + 50 MiB = 3.2 GiB. Check
  `get_renderer_info` after a deploy. The server does not read the cgroup
  CPU quota.
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

## No authentication for staging

**Staging only.** `PSCAT_MCP_AUTH=none` (or `bearer,none`) lets a request with
no credential render as the principal `anonymous`. Anyone who has the URL
can then spend the service's render CPU time and quota, within the resource
limits and the guard rails below, and the service is billed for it. It is
never on by default and logs a warning at startup. Do not enable it on a
production service.

**Why.** ChatGPT's custom MCP connector can't send a fixed `Authorization`
header: it offers only OAuth or "No authentication". `none` is the stopgap
for trying ChatGPT against a staging service until an identity provider for
`oauth` (above) is set up. Claude, scripts and CI keep their bearer tokens beside it
(`bearer,none`): a request with a valid token is the token's principal and
is never counted against the anonymous limits, and a request with a wrong
token is `401`, never anonymous.

**A staging setup.**

| Setting | Value |
|---|---|
| `PSCAT_MCP_AUTH` | `bearer,none` (or `none` with no token at all) |
| `PSCAT_MCP_AUTH_NONE_UNTIL` | a near time, such as a week ahead (`2026-12-01T00:00:00Z`). A running instance answers `403` after it, and a start after it (a cold start, a new revision) comes up with `none` disabled instead of failing; see "After the window closes" below |
| `PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS` | see "The client address" below: `1` on the `run.app` URL (measured, see below), `2` behind an external Application Load Balancer |
| `PSCAT_MCP_AUTH_NONE_ALLOW_CIDRS` | the connector's source ranges, if you know them; unset allows any source |
| `PSCAT_MCP_AUTH_NONE_RATE_LIMIT` / `_RATE_WINDOW_SECS` | `60` per `60` s per client by default. The limit is kept in each instance, so with several instances it multiplies: also set `--max-instances` to a small number |
| `PSCAT_MCP_ASYNC_JOBS` | unset: the server refuses to start with `none` and async jobs (one anonymous principal could read or cancel another's jobs). MCP Tasks need async jobs too |

**After the window closes.** Cloud Run scales to zero, so an expired
`PSCAT_MCP_AUTH_NONE_UNTIL` left in the configuration meets every cold start
and every new revision. The server does not refuse to start then, because
that would take the bearer clients, CI end-to-end runs and deploys down with
the anonymous access it was only meant to add. With `none` in
`PSCAT_MCP_AUTH` and a time that has already passed, it starts normally with
`none` disabled:

* a `WARN` names `PSCAT_MCP_AUTH_NONE_UNTIL` and the expiry time, and the
  startup `auth_methods` line lists only the methods still enabled (`bearer`);
* bearer requests work as before, and a wrong token is still `401`;
* a request with no credential gets `403 unauthenticated_access_expired`,
  exactly what a running instance answers once its window closes. It is
  never admitted;
* with `none` as the only method, the server still starts, `auth_methods` is
  empty and every request gets that `403`.

This is a safe state, not a finished one: remove `none` and the
`PSCAT_MCP_AUTH_NONE_*` settings, or set a later time, at the next deploy. A
malformed time, a bad CIDR, a rate limit or window of 0, and `none` with
async jobs are configuration errors and still refuse to start.

Cloud Run itself must let the caller in: ChatGPT can't send a Google ID
token, so the service needs `--allow-unauthenticated` (the public
`allUsers` invoker) at the Cloud Run level, with `PSCAT_MCP_ALLOWED_HOSTS`
set to the service's host as in "Required configuration". The other
resource limits (`PSCAT_MCP_MAX_*`, `PSCAT_MCP_MAX_CONCURRENT_RENDERS`,
`PSCAT_MCP_QUEUE_TIMEOUT_MS`) apply unchanged.

**Connecting ChatGPT.** In ChatGPT, add a custom MCP connector (developer
mode), give it the service URL ending in `/mcp`, and choose "No
authentication". Nothing else is needed on the ChatGPT side. This has not
been run against a real ChatGPT or Cloud Run service from this repository.

**The client address.** The limit and the allowlist need the caller's
address. A client can put anything in `X-Forwarded-For`, and each proxy only
appends to it, so the server reads it only as far as you tell it to:
`PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS=N` takes the N-th entry from the right, and
the entries to its left (which the client controls) are never used. With the
default of `0`, the address is the TCP peer, which on Cloud Run is Google's
front end: every caller would then share one rate-limit bucket and the
allowlist would match the front end, so neither check would mean anything.
Set it on Cloud Run.

* Behind an external Application Load Balancer, Google documents the
  header as `X-Forwarded-For: [SUPPLIED_VALUE,] CLIENT_IP, LOAD_BALANCER_IP`,
  where `SUPPLIED_VALUE` is whatever the client sent. The client is the
  second entry from the right: **`PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS=2`**.
  This is taken from Google's documented header format and was not measured.
* On the service's own `*.run.app` URL, measured on Cloud Run gen1
  (us-central1) on 2026-11-16 with no load balancer: the client is the last
  entry, **`PSCAT_MCP_AUTH_NONE_TRUSTED_HOPS=1`**. At 1, a forged entry
  neither let a caller in nor shut one out. At 2, a caller not on the
  allowlist got `200` by sending a forged `X-Forwarded-For` naming an allowed
  address, so 2 is spoofable on a direct URL: the extra hop reads an entry the
  client wrote.

**Check the hop count on staging before trusting it.** A value that is too
high for the real chain lets a client's own entry stand for its address; a
value that is too low names Google's address. To check, set the allowlist to
your own address and the hop count, then call the service from that address
(`200`) and from another network (`403 source_not_allowed`), and add a
forged header from the second network: `curl -H 'X-Forwarded-For: <your
address>' ...` must still be `403`. To see the address the server derived,
run with `RUST_LOG=info,pscat_mcp::auth_none=debug` (the one place addresses
are logged: never at INFO, and not for long, since Cloud Logging keeps it).
Set the trusted-hop count to the value you measured and no higher: a count that is too high lets an entry the client supplied be used as its address. A header line that is not plain visible ASCII makes the source unknown. If the header has fewer entries than the hop count, the source is unknown and
refused, never guessed from the leftmost entry.

**What is logged and counted.** A request `none` admits carries
`auth_method` `none`, so it is counted apart from `bearer` in log-based
metrics. A refused one logs only its `reason` (`source_not_allowed`,
`rate_limited`, `unauthenticated_access_expired`). No client address and no
per-client label is ever emitted at INFO, as the next section promises.

## Sandbox

Use the **first-generation execution environment** (gVisor) for public
traffic. It adds a kernel-level sandbox around the worker processes, beyond
the rlimits and PSCat's own confinement. Second-generation instances run on
a regular Linux kernel, where the worker's `RLIMIT_AS` applies but nothing
else isolates its system calls.

## Logging and metrics

Logs are JSON on stderr: one line per request with `request_id`, method,
path, status, latency and, once an auth method has accepted the request,
`auth_method` (`bearer`, `oauth`, or `none` in the staging mode above). A refused request also logs a warning
with `reason` and `rejected_by`, and the startup lines name the enabled
`auth_methods`. With `oauth`, a refused token also logs `oauth: token
rejected` with a fixed `reason` (such as `expired`, `wrong_audience`,
`not_allowed`, `jwks_unavailable`), and a failed JWKS fetch logs a warning;
no subject, email or token is ever logged, and the principal is a hash. There is one line per render with source byte count,
steps, elapsed ms, pages and failure flag. PostScript source and tokens are
never logged, except in the opt-in capture mode (`PSCAT_MCP_CAPTURE_FILE`),
which only starts on a loopback bind and so never in a deployment. No per-user or per-program labels are emitted, so cardinality
stays low.

The failure and queue lines carry the wire error code (the response's
`structuredContent.error.code`, snake_case), from a fixed vocabulary:

| Line (`message`) | Fields |
|---|---|
| `render finished` | `failed` (bool); when `true`, also `code` (`postscript_error`, `limit_exceeded`, `too_many_pages`, `output_too_large`, `worker_failed`, or from a generator tool `invalid_request`, `too_many_pixels`, `invalid_dimensions`, `source_too_large` or `timeout`) and, for `limit_exceeded`, `limit` (`steps`, `time` or `memory`). Absent when `failed=false`. A limit hit (`limit_exceeded`, the sign that the sizing is wrong) is told from a PostScript error (`postscript_error`) by `code`. |
| `render killed at the deadline` (WARN) | `code=timeout`: the wall-clock deadline killed the worker. |
| `render worker failed` (ERROR) | `code=worker_failed`: the worker crashed or its output was unusable. |
| `request refused: no render slot became free in time` (WARN) | `code=server_busy` and `queue_wait_ms`: one line per request that waited out `PSCAT_MCP_QUEUE_TIMEOUT_MS` for a render slot (the signal to add capacity). Other `server_busy` causes (async job caps, shutdown) are not logged here. |
| `request rejected before execution` | `code`: the wire code (`invalid_request`, `source_too_large`, `too_many_pixels`, ...). |

The code and limit are never taken from the program: a PostScript error's name
or offending command, its message and any program output stay out of the
logs.

The default log filter is `info,rmcp::service=warn`. At INFO, rmcp logs a line
per request, "Service initialized as server", with a Debug dump of the
`initialize` parameters it holds for the peer. Over stdio those are the
client's own (`clientInfo.name`, `version`, capabilities): client-chosen free
text, which the logging contract above does not cover. Over this stateless
HTTP server rmcp 3.9 fills in defaults rather than the client's values, so the
line is noise there, but the filter holds back the same line on both
transports, and the same target's other INFO lines (a notification's or a
cancellation's text, "serve finished"). `RUST_LOG` replaces the default
completely: `RUST_LOG=info` or `rmcp=info` brings the line back, and with it
any client-supplied strings it carries, so leave it unset in a deployment
unless debugging.
