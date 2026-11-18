# Auth strategy for the hosted server

Research note for issue #136. Sources read on 2026-11-17 unless a row says
otherwise. Bracketed IDs such as [A1] refer to the [Sources](#sources) table.
Nothing here changes code; the follow-up tickets are at the end.

**Status: approved by the owner on 2026-11-17, with changes.** WorkOS AuthKit
is chosen as the identity provider (Auth0 stays the fallback only if AuthKit
fails a live test). `none` is deprecated once OAuth is accepted: there is no
public no-auth production service. Production is `bearer,oauth`, with the
allowlist by `email:`. The rest of this note is the reasoning behind that
decision and is left as written; where it proposes a public `none` service,
the owner's decision above wins. Follow-up tickets are tracked as noted under
the table at the end.

## Summary and recommendation

**What the server does today** (`PSCAT_MCP_AUTH`): `bearer` (static tokens
read once at startup, principal `token#N` by list position), `oauth` (a
resource server for one issuer: JWT via JWKS, exact `iss`, `aud` must contain
`PSCAT_MCP_OAUTH_RESOURCE`, optional scopes, an allowlist of `sub:`/`email:`
entries that may not be empty, RFC 9728 metadata) and `none` (anonymous, with
expiry, CIDR allowlist, per-instance per-IP fixed-window rate limit, trusted
hops; refused together with async jobs). All limits (`--max-steps`,
`--timeout`, `--max-memory`, pixels, render slots, queue) are global: they do
not differ by auth method.

**Three findings change the design:**

1. **`oauth` and `none` must not share a URL.** `none` admits any request
   without a credential, so the client never sees a `401`. Claude starts
   sign-in only on a `401` with `WWW-Authenticate` and ignores the header on
   a `200` [A1][A3]; ChatGPT's mixed mode works per tool through tool
   metadata and a tool-level challenge [O1][O2]. On a `bearer,oauth,none`
   URL, ChatGPT and claude.ai users would run as anonymous and never be asked
   to sign in. Mixed auth on one URL needs a body-aware "lazy auth" gate,
   which does not exist (auth reads headers only).
2. **Anonymous and signed-in callers cannot get different limits on one
   service.** The only way to give anonymous callers smaller limits today is a
   separate Cloud Run service (same image, its own env and
   `--max-instances`).
3. **`oauth` cannot admit "anyone who signs in".** The allowlist is required.
   That fits an invite-only production; open sign-up needs a new rule
   (domain, claim or allow-all).

**Recommended mix:**

| Environment | Service | `PSCAT_MCP_AUTH` | Notes |
|---|---|---|---|
| staging (now, until an IdP exists) | one | `bearer,none` | as today, `PSCAT_MCP_AUTH_NONE_UNTIL` set, hops measured, `--max-instances` small |
| staging (once the IdP exists) | one | `bearer,oauth` | drop `none` so ChatGPT and claude.ai sign-in can be tested end to end (#119 acceptance) |
| production, main URL `/mcp` | `pscat-mcp` | `bearer,oauth` | allowlist (invite-only); bearer for CI, scripts, Grok-API bots |
| production, optional public URL | `pscat-mcp-public` | `none` | separate service, tight limits (see Q2), `--max-instances 2`, behind Cloud Armor; ship only after tickets T1, T12, T13 |

**Identity source: WorkOS AuthKit** (free to 1 M MAU), with Auth0 as the
fallback (Q3). Either way, allowlist users by `email:` so a later move to
another provider stays cheap. The authorization server must advertise **both**
CIMD and DCR, issue **JWT** access tokens with `aud` set from the RFC 8707
`resource` parameter, and support PKCE S256 and refresh-token rotation.
Cloudflare's options issue opaque tokens and Firebase/Google are not MCP
authorization servers, so they are ruled out. CIMD alone covers ChatGPT,
Claude (all surfaces), Codex, opencode, VS Code and Antigravity; DCR is still
needed for Cursor, Gemini CLI, Le Chat and Copilot Studio's automatic mode.

**CLI keys:** keep static bearer tokens for CI and non-interactive callers,
but make them named and reloadable (T5). Interactive CLIs (Claude Code,
Codex, opencode, VS Code, Cursor, Antigravity) all do OAuth with a loopback
redirect, so once the IdP exists, people do not need keys at all.

## Q1. Client matrix

Legend: **Yes** documented by the vendor; **Yes (src)** found in the
client's source; **implied** not stated but follows from documented
behaviour (for example, auth config is optional); **No** documented as not
supported, or absent from both docs and source; **unverified** no primary
source found. "Header" means a fixed `Authorization` (or other) header the
user configures. PKCE: the MCP spec makes S256 a client MUST [S1]; the cell
says "Yes" only where the vendor page states it.

| Client | Static header | OAuth 2.1 + PKCE | DCR (RFC 7591) | CIMD | None | Pre-registered client ID | Sources |
|---|---|---|---|---|---|---|---|
| ChatGPT (apps / developer mode) | **No** ("cannot present custom API keys") | Yes (S256) | Yes ("still supported") | Yes, preferred; `https://chatgpt.com/oauth/client.json` | Yes; also "mixed" per tool | Yes ("predefined OAuth clients") | [O1][O2] |
| claude.ai, Claude Desktop, mobile (custom connector) | Beta, limited orgs, set by an org Owner; not `Authorization` together with OAuth | Yes (S256 on every request) | Yes | Yes, needs `none` in `token_endpoint_auth_methods_supported` | Yes | Yes ("Use your own OAuth client") | [A1][A2] |
| Claude Code | Yes (`--header`, `headersHelper`) | Yes | Yes | Yes (`https://claude.ai/oauth/claude-code-client-metadata`) | implied (OAuth starts on 401/403) | Yes (`--client-id`, `--callback-port`) | [A4][A1] |
| Codex CLI | Yes (`bearer_token_env_var`, `http_headers`, `env_http_headers`) | Yes (`codex mcp login`); PKCE not stated | Yes | Yes, needs `none` in auth methods; `https://chatgpt.com/oauth/codex/client.json` | Yes ("may connect without authentication") | Yes (`--oauth-client-id`) | [O3] |
| opencode | Yes (`headers`, `{env:…}`) | Yes (PKCE stated) | Yes | Yes | implied (`"oauth": false`) | Yes (`client_id`) | [OC] |
| Antigravity (successor of Gemini CLI) | Yes (`headers`) | Yes (PKCE stated) | Yes | Yes (IDE ≥ 2.6.0); `https://antigravity.google/oauth/client-metadata.json` | implied | Yes (`oauth.clientId`) | [AG] |
| Gemini CLI (legacy: stopped for individual accounts on 2026-06-18) | Yes (`headers`) | Yes; PKCE not stated | Yes | **No** (not in docs; not in `oauth-provider.ts` on main) | implied | Yes (`oauth.clientId`) | [G1][G2][G3] |
| VS Code (Copilot) | Yes (`headers`, `${input:…}`) | Yes; PKCE not stated | Yes | Yes (src), Microsoft builds only (`authClientIdMetadataUrl` is not in the OSS `product.json`); docs silent | implied | Yes (`oauth.clientId`) | [V1][V2][V3] |
| Cursor | Yes (`headers`, `${env:…}`) | Yes (PKCE stated) | Yes (public client) | **No** (not in docs; staff: "no timeline", last post 2026-06-01) | implied | Yes (`auth.CLIENT_ID`) | [C1][C2] |
| Grok via xAI API (Responses / SDK remote MCP tool) | Yes (`authorization`, `headers`) | n/a (caller supplies the token) | n/a | n/a | implied | n/a | [X1] |
| Grok app (grok.com custom connector) | unverified | unverified | unverified | unverified | unverified | unverified | [X2]: page says only "complete any required authentication" |
| Meta AI / Muse | unverified | unverified | unverified | unverified | unverified | unverified | [M1]: custom connectors are built by Muse from "API information"; no remote MCP URL is documented |
| Microsoft Copilot Studio | Yes (API key in header or query) | Yes; PKCE not stated | Yes ("Dynamic discovery") | not documented | Yes | Yes ("Manual") | [MS] |
| Mistral Le Chat | Yes (Bearer or Basic) | Yes (OAuth 2.1) | Yes | not documented | Yes | not documented | [MI] |

**Who needs DCR or CIMD at the authorization server.** No client strictly
requires them if users paste a client ID, but that is per-user setup and
Le Chat documents no manual option. Without per-user setup:

* CIMD is enough for ChatGPT, claude.ai/Desktop/mobile, Claude Code, Codex,
  opencode, VS Code and Antigravity. Claude and Codex also need `none` in
  `token_endpoint_auth_methods_supported` [A1][O3].
* DCR is needed for Cursor, Gemini CLI, Le Chat and Copilot Studio's
  automatic mode.
* Claude registers a new client on every fresh connection under DCR and
  recommends CIMD for traffic [A1]. ChatGPT registers once per connection
  [O1].
* Redirect URIs the authorization server must accept:
  * ChatGPT: `https://chatgpt.com/connector_platform_oauth_redirect` (with RFC 9207 `iss`), otherwise a per-connection URI [O1].
  * Claude hosted apps: `https://claude.ai/api/mcp/auth_callback`.
  * Claude Code: loopback `localhost` and `127.0.0.1` on any port [A1].
  * Cursor: `https://www.cursor.com/agents/mcp/oauth/callback` and `http://localhost:8787/callback` [C1].
  * VS Code: `http://127.0.0.1:33418` and `https://vscode.dev/redirect` [V2].
  * Antigravity: `https://antigravity.google/oauth-callback` [AG].

**Conflicts and caveats.**

* Anthropic's older help-center articles mention only a client ID and secret
  for custom connectors. The newer claude.com docs list CIMD, DCR and an own
  client [A2]. This note follows the newer docs.
* None of the vendor pages for ChatGPT, Codex, opencode, Cursor or
  Antigravity carry a date, so "read on 2026-11-17" is the only date.
* The Gemini CLI page is dated 2026-10-06, but the product itself is
  sunset [G1][G3].

## Q2. Anonymous tier

**Cost per render.** Cloud Run lists $0.000018 per vCPU-second and $0.000002
per GiB-second in us-central1 [GC1]. The static page showed the same figures
under both billing modes, so check the calculator for the request-based rate.
The free tier is 180,000 vCPU-s, 360,000 GiB-s and 2 M requests a month
[GC1]. A worst-case synchronous render (20 s, one vCPU, about 0.8 GiB) costs
about $0.0004. One render is cheap; the cost comes from saturation. A
`--cpu 2 --memory 2Gi` instance that is busy all the time costs about
$0.00004/s, which is about **$100 a month per instance**. The hard bound is
`max_instances × that`, and only `--max-instances` enforces it.

**What the per-IP limit gives, and what it does not.**

* It is per instance and in memory. Cloud Run scales on CPU and concurrency
  [GC2]. With N instances, one client can get up to N × the limit. Each new
  instance (scale-out, a deploy, scale from zero) starts with empty counters.
* Session affinity is "best effort" [GC3], so it cannot pin a client to one
  counter.
* It limits requests, not CPU. At 60 requests per 60 s and 20 s renders, one
  address can keep two render slots busy with about 6 requests a minute. The
  per-IP limit does not bound cost; the slot count and `--max-instances` do.
* A distributed client (many addresses, or an IPv6 range beyond a /64)
  bypasses it.
* Anonymous requests share render slots and the queue with signed-in users,
  so on a shared service anonymous load causes `server_busy` for paying
  users.

**What Cloud Armor adds.**

* Cloud Armor rate limiting runs at the load balancer edge, across instances
  [GC5]. It can throttle or ban per IP, XFF IP, header, ASN or JA3/JA4
  [GC5].
* Limits are approximate and apply per region [GC5]. Google positions it
  for abuse mitigation, not quotas.
* It needs an external Application Load Balancer with a serverless NEG.
  The `run.app` URL bypasses it unless ingress is restricted [GC8].
  Behind the load balancer the hop count changes: docs/CLOUD_RUN.md gives 2
  from Google's documentation, but only the direct `run.app` value (1) has
  been measured, so measure it on the real setup before relying on 2.
* Cost, Standard tier [GC6][GC7]:
  * $0.0068/h per policy, about $5 a month;
  * $0.0014/h per rule, about $1 a month;
  * $0.75 per M requests;
  * plus about $18 a month for the forwarding rule.
* It covers the per-instance gap for request rate, but not the CPU-per-request
  gap.

**Verdict.** A no-auth tier is acceptable in production only as a separate
service with its own small limits and a hard instance cap. It must never be
the `none` fallback on the authenticated URL (finding 1). Abuse risk is
bounded: renders run in the sandboxed worker, and the cost is capped by
`--max-instances`. Add a budget alert. Starting values, to be measured on
staging:

| Knob (env) | Anonymous service | Authenticated service |
|---|---|---|
| `--max-instances` (Cloud Run) | 2 | 5 (or what the budget allows) |
| `--cpu` / `PSCAT_MCP_MAX_CONCURRENT_RENDERS` | 1 / 1 | 2 / 2 |
| `PSCAT_MCP_RENDER_TIMEOUT_MS` | 5000 | 20000 (default) |
| `PSCAT_MCP_MAX_STEPS` | 500000000 | 5000000000 (default) |
| `PSCAT_MCP_MAX_MEMORY_BYTES` | 268435456 | 536870912 (default) |
| `PSCAT_MCP_MAX_PIXELS` / `_MAX_PAGES` / `_MAX_OUTPUT_BYTES` | 4000000 / 2 / 2097152 | defaults |
| `PSCAT_MCP_MAX_SHEET_CELLS` | 8 | 32 (default) |
| `PSCAT_MCP_QUEUE_TIMEOUT_MS` | 2000 | 10000 (default) |
| `PSCAT_MCP_AUTH_NONE_RATE_LIMIT` / window | 20 / 60 s | n/a |
| Async jobs | off (refused with `none`) | on only with one always-on instance |
| Cloud Armor | throttle per IP, ban on repeat | optional |

**Knobs that do not exist yet:**

* per-method limits on one service (T3);
* a cap on concurrent anonymous renders per address and in total (T4);
* a shared, cross-instance counter. Cloud Armor stands in for this; an
  in-app version would need Redis or Firestore, which is out of proportion
  for this project.

## Q3. OAuth identity source

What the authorization server must do for our resource server and the
clients in Q1:

* issue **JWT** access tokens whose `aud` comes from `resource` and equals
  `PSCAT_MCP_OAUTH_RESOURCE`, since we have no introspection;
* publish a JWKS that RFC 8414 or OIDC discovery can find;
* support PKCE S256;
* support CIMD with `none`, plus DCR;
* rotate refresh tokens for public clients;
* ideally, put RFC 9207 `iss` in authorization responses (ChatGPT then uses
  its stable client ID and redirect [O1]);
* ideally, put `email` and `email_verified` in the access token.

| Option | DCR | CIMD | JWT AT, `aud` from `resource` | Email in AT | Cost at small scale | Lock-in / ops | Fit |
|---|---|---|---|---|---|---|---|
| **WorkOS AuthKit** | Yes (toggle) | Yes, off by default (toggle); `none` advertised | Yes, "`aud` claim that matches the requested `resource`" once a Resource Indicator is configured | Yes, via JWT template | free to 1 M MAU; custom domain $99/mo | user IDs proprietary; hosted, nothing to run | **best hosted fit**; RFC 9207 and public-client rotation confirmed [I1][I2] |
| **Auth0** | Yes, open once a tenant toggle is on | manual import GA; dynamic CIMD Early Access | Yes, with the "Resource Parameter Compatibility Profile"; API identifier = MCP URL | Yes, via an Action custom claim | free to 25k MAU; Essentials $35/mo | Actions and connections; hosted | fallback; whether rotation applies to DCR/CIMD clients is unverified [I3][I4][I5] |
| Stytch Connected Apps | Yes (toggle) | Beta (toggle) | JWT, but `aud` = project id plus a per-client custom audience; `resource` → `aud` unverified | not documented | free to 10k MAU | hosted | fails our `aud` rule unless `resource` is honoured [I6] |
| Keycloak (self-run) | Yes; anonymous DCR governed by policies | experimental flag since 26.6.0 (2026-04-08, old) | JWT; `resource` → `aud` is an experimental flag, otherwise an audience mapper | Yes, via mapper | software free; Cloud Run plus Postgres (Cloud SQL is not free) | open source, exportable; JVM, database, upgrades | too much to run for one maintainer [I7] |
| Cloudflare workers-oauth-provider | Yes | Yes | **opaque** tokens | n/a | Workers/KV (not checked) | MIT library, Workers-only | **ruled out** (no introspection, T9) [I8] |
| Cloudflare Access Managed OAuth | Yes | not mentioned | **opaque** to the client; the origin gets a Cloudflare JWT | unverified | "generous free tier" | traffic must go through Cloudflare; open beta | ruled out [I9] |
| Google Identity Platform / Firebase Auth | No | No | not an OAuth authorization server (no authorize endpoint) | n/a | 50k MAU free | n/a | **ruled out**; usable only as upstream login [I10][I11] |
| Plain Google OAuth | No | No | ID token `aud` is our Google client ID | n/a | free | n/a | ruled out as the AS; Google's own MCP servers support neither DCR nor CIMD [I11] |
| Clerk, Descope, Logto | toggles | Clerk and Descope yes; Logto yes | `aud` and `resource` unverified | unverified | Clerk 50k MRU free; Descope 7,500 MAU free; Logto Free has no API resources | hosted | not checked enough to recommend [I12] |
| Write our own (on Cloud Run, federating to Google) | optional (deprecated) | must build | by construction | from Google's verified ID token | about $0 (Cloud Run plus Firestore) | lowest lock-in; we own security-sensitive code | later option |

**Writing our own** means building:

* RFC 8414 metadata;
* `/authorize` with S256, an exact redirect match and RFC 8252 loopback port
  handling;
* a consent page that shows the redirect host;
* CIMD fetching with SSRF guards (resolve and block private addresses, size
  and time caps, no redirects, caching);
* `/token` with refresh rotation and reuse detection;
* JWT signing and key rotation;
* RFC 9207 `iss`;
* storage for codes and grants;
* Google as the upstream login.

That is weeks of security-sensitive work for one maintainer. Cloudflare's
library docs are a useful checklist if it is ever done [I8].

**Recommendation: WorkOS AuthKit**, configured as follows:

* CIMD and DCR enabled;
* a Resource Indicator equal to the production and staging MCP URLs;
* a JWT template adding `email` and `email_verified`;
* allowlist entries written as `email:`, not `sub:`.

Changing providers later changes `iss`, so every user signs in once more, but
`email` entries carry over. That keeps the choice reversible. **Auth0** is
the fallback if AuthKit fails a live test.

**Lock-in and migration.** Our side is provider-agnostic: an issuer URL, a
resource URL and an allowlist. Leaving a provider means changing two env
vars and having users re-authorize.

**To verify on staging before production:**

* loopback redirects on any port for Claude Code (not documented by WorkOS);
* that DCR clients from Cursor and Le Chat get public-client rotation;
* that ChatGPT and claude.ai pick CIMD;
* that `email_verified` in the AuthKit access token is a JSON boolean. The
  server ignores a string `"true"`, so if the template renders a string,
  every `email:` entry fails with `403 not_allowed`. Decode one token to
  check. The fallback is `sub:` entries, which brings back provider lock-in.

### What the MCP spec requires of us as a resource server

The current revision is **2026-07-28** [S3]. 2025-11-25 [S2] and 2025-06-18
came before it. The resource-server rules are largely unchanged since
2025-11-25. In 2026-07-28, DCR is **deprecated** in favour of CIMD, and
RFC 9207 `iss` is added for authorization servers [S1].

| Requirement (resource server) | Level | Status in `src/oauth.rs` |
|---|---|---|
| Serve RFC 9728 metadata with `authorization_servers` | MUST | met: both well-known paths, one issuer |
| `401` with `WWW-Authenticate: Bearer resource_metadata=…` (or well-known) | MUST (one of) | met: both |
| `scope` in the challenge | SHOULD | met when `PSCAT_MCP_OAUTH_SCOPES` is set |
| Validate tokens per OAuth 2.1 §5.2; `aud` must name us (RFC 8707) | MUST | met: signature, `iss`, `aud`, `exp`/`nbf` |
| Invalid or expired token gets `401` | MUST | met |
| `403 insufficient_scope` with `scope` and `resource_metadata` | SHOULD | met |
| No token passthrough; no tokens in the query string | MUST | met (no upstream calls; header only) |
| Account for scope hierarchies | MUST (2026-07-28) | n/a while scopes are flat; becomes a gap if hierarchical scopes are introduced |
| Accept uppercase scheme/host in the canonical URI | SHOULD | **gap**: `aud` is compared exactly (modulo a trailing `/`) (T8) |
| Secure token storage, never logged | MUST | met (digests only; tokens never logged) |

**Gaps that are not spec violations but limit the choice of identity
source:**

* one issuer only (T7);
* JWT only, no RFC 7662 introspection, so an identity source that issues
  opaque tokens cannot be used (T9);
* an allowlist is required (T6);
* no lazy or mixed auth (T2);
* no ChatGPT per-tool `securitySchemes` (T10).

Claude uses only the first `authorization_servers` entry [A1]. That matches
our single issuer.

## Q4. Keys for CLI agents

Today:

* tokens come from `PSCAT_MCP_AUTH_TOKENS` or `PSCAT_MCP_AUTH_TOKEN_FILE`;
* they are read once at startup and kept as SHA-256 digests;
* the principal is the token's position (`token#N`; see `principal()` in
  `src/mcp.rs`).

So removing a token from the middle of the list renumbers the others, and the
logs cannot say whose token it was. A Secret Manager volume pinned to
`latest` is re-read from Secret Manager each time the file is read [GC4], but
the server reads it only at startup. Rotation therefore means a new revision.

| Option | Issue | Rotate | Revoke | Trade-offs |
|---|---|---|---|---|
| **A. Named tokens in one Secret Manager secret** (`name:sha256hex` per line) | owner runs a script: generate 32 random bytes, append `name:digest`, add a secret version, give the raw token to the user once | add a second line for the same name, retire the old one later | delete the line, new secret version, reload | No database: the secret is the user list. Only digests are at rest. Needs T5 (named, stable principal ids; digest entries; reload on an interval or SIGHUP). Works with every client that has a static header. |
| **B. Self-signed JWT keys** (owner signs long-lived JWTs offline; public JWKS on static hosting) | a script mints a JWT with `sub`, `exp`, `jti` | short `exp` and re-issue | remove `sub` from the `oauth` allowlist, or wait for `exp` | Reuses the existing validator, but `oauth` takes **one** issuer, so it collides with the real IdP (needs T7). Revocation is only as fresh as the allowlist reload. A leaked key is valid until `exp`. |
| **C. Macaroons or other attenuated tokens** | custom | custom | custom | No client support beyond a static header, and new crypto code for little gain. Not recommended. |
| **D. OAuth for interactive CLIs** | the IdP | the IdP (refresh rotation) | the IdP, or remove from the allowlist | No keys at all. Claude Code, Codex, opencode, VS Code, Cursor and Antigravity sign in through a browser (Q1). Headless hosts: Codex `--no-browser` and opencode accept a pasted callback URL [O3][OC]. |
| **E. Client credentials for CI** | an M2M client at the IdP | the IdP | the IdP, or the allowlist (`sub:` of the client) | Works only if the IdP issues JWTs with our `aud` to M2M clients. Claude does not support `client_credentials` for connectors [A1]. In Claude Code or Codex, a `headersHelper` or `http_headers_helper` script can fetch the token [A4][O3]. Extra IdP cost may apply (Q3). |

**Recommendation:** use A now for CI, scripts, Grok-API bots and anyone
whose client cannot do OAuth. Use D as the default for people once the IdP
is live. Use E only if CI must not hold a long-lived secret. Skip B and C.

## Q5. Per-client setup (production, `bearer,oauth` at `https://<domain>/mcp`)

* **ChatGPT.** Add an app in developer mode with the URL and authentication
  OAuth. ChatGPT reads the metadata and uses CIMD, or DCR as the fallback.
  The authorization server must allow ChatGPT's redirect and echo `resource`
  into `aud` [O1].
* **claude.ai, Desktop, mobile.** Customize, then Connectors, then Add custom
  connector, with the URL. Keep the **Detected** OAuth client option (CIMD,
  otherwise DCR). Team and Enterprise plans: an Owner adds it [A2].
* **Claude Code.** For OAuth:
  `claude mcp add --transport http pscat https://<domain>/mcp`, then `/mcp`
  to sign in. For a key, add
  `--header "Authorization: Bearer $TOKEN"`; a configured `Authorization`
  disables OAuth for that server [A4].
* **Codex CLI.** `codex mcp add pscat --url https://<domain>/mcp`, then
  `codex mcp login pscat`. For a key, set `bearer_token_env_var` under
  `[mcp_servers.pscat]` [O3].
* **opencode.** For OAuth: a remote entry with `"url"`, then
  `opencode mcp auth pscat`. For a key:
  `"headers": {"Authorization": "Bearer {env:PSCAT_TOKEN}"}` and
  `"oauth": false` [OC].
* **VS Code, Cursor, Antigravity.** For OAuth, a remote `url` entry and the
  first connection signs in. Cursor needs DCR or a static `auth.CLIENT_ID`.
  For a key, use `headers` with `${input:…}` (VS Code) or `${env:…}` (Cursor)
  [V1][C1][AG].
* **Grok bots (xAI API).** Pass the server URL and `authorization` (the
  bearer token) in the remote MCP tool definition [X1]. The Grok app is
  unverified.
* **Copilot Studio, Le Chat.** OAuth through DCR ("Dynamic discovery"), or
  an API key header [MS][MI].
* **Public tier (if deployed).** Give its separate URL. No setup; the client
  picks "No authentication".

## Follow-up tickets

| # | Repo | Ticket | Why | Priority |
|---|---|---|---|---|
| T1 (tracked in #153) | pscat-mcp | Warn or refuse `oauth` together with `none` on one server; update the README and CLOUD_RUN.md `none` wording (today "staging only") to allow a separate production public service | with both, OAuth sign-in is never triggered (finding 1) | high |
| T2 | pscat-mcp | Lazy auth gate: answer `401` + challenge for `tools/call` without a credential, let `initialize`/`tools/list` through. The 2026-07-28 `Mcp-Method` header names the method, not the tool, and older clients do not send it, so the gate must parse the JSON-RPC body | one URL for anonymous browsing plus sign-in [A3] | low |
| T3 | pscat-mcp | Per-method limit profiles (timeout, steps, memory, pixels, output) | anonymous vs signed-in limits on one service | medium |
| T4 | pscat-mcp | Cap concurrent anonymous renders, per address and in total | the request-rate limit does not bound CPU | medium |
| T5 | pscat-mcp | Named bearer tokens (`name:sha256`), stable principal ids, periodic or SIGHUP reload | issue, rotate and revoke keys without a database or redeploy | high |
| T6 | pscat-mcp | `oauth` admission rules beyond the allowlist: email domain, a required claim or group, or explicit allow-all | open sign-up in production | medium (only if sign-up opens) |
| T7 | pscat-mcp | Accept several issuers in `oauth` | self-signed keys next to the IdP; IdP migration without downtime | low |
| T8 | pscat-mcp | Case-insensitive scheme/host when comparing `aud` | spec SHOULD [S1] | low |
| T9 | pscat-mcp | RFC 7662 introspection | only if the chosen identity source issues opaque tokens | conditional |
| T10 | pscat-mcp | Per-tool `securitySchemes` in tool metadata for ChatGPT | ChatGPT's mixed-auth UI [O1] | low |
| T11 | pscat-cloud | Choose and provision the identity source (Q3); register the redirects listed in Q1 | unblocks #119 acceptance and pscat-cloud#26 | high |
| T12 | pscat-cloud | Production layout: an auth service plus an optional public service, `--max-instances`, budget alert | cost bound and separate limits (Q2) | high |
| T13 | pscat-cloud | External ALB + serverless NEG + Cloud Armor throttle; restrict ingress; measure the trusted-hop count behind the ALB (docs say 2, unmeasured) (pscat-cloud#8) | cross-instance rate limit for the public tier | medium |
| T14 | pscat-cloud | Token issuance runbook or script for option A (Secret Manager versions) | repeatable key issuance and revocation | medium |
| T15 | pscat | Per-client setup page (#1136) from Q5 | users of every client | medium |
| T16 | pscat | ChatGPT package (#1253) points at the OAuth URL | ChatGPT cannot send a header | medium |
| T17 | pscat | Grok bots (#1142) through the xAI API with a named bearer token | the only verified Grok path | low |

Tracked after the owner's decision: T1 in #153; the deprecation and removal
of `none` in #154; the WorkOS AuthKit setup guide and the token checker script
(`docs/OAUTH_WORKOS.md`, `scripts/oauth-check.py`) in #152.

## Sources

All read on 2026-11-17. "Undated" means the page shows no date. "Old" flags a
source dated before 2026-05-17.

| ID | Source | Date shown |
|---|---|---|
| S1 | https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization (and its subpages `authorization-server-discovery`, `security-considerations`) | revision 2026-07-28 |
| S2 | https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization | revision 2025-11-25 (old, superseded) |
| S3 | https://modelcontextprotocol.io/specification/versioning | undated; names 2026-07-28 as current |
| O1 | https://developers.openai.com/apps-sdk/build/auth | undated |
| O2 | https://developers.openai.com/api/docs/guides/developer-mode | undated |
| O3 | https://learn.chatgpt.com/docs/extend/mcp?surface=cli (redirect from developers.openai.com/codex/mcp) | undated |
| A1 | https://claude.com/docs/connectors/building/authentication.md | undated |
| A2 | https://claude.com/docs/connectors/custom/add-unlisted.md | undated |
| A3 | https://claude.com/docs/connectors/building/lazy-authentication.md | undated |
| A4 | https://code.claude.com/docs/en/mcp | undated |
| OC | https://opencode.ai/docs/mcp-servers/ | undated |
| AG | https://antigravity.google/docs/mcp | undated |
| G1 | https://geminicli.com/docs/tools/mcp-server/ | 2026-10-06 |
| G2 | https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/mcp/oauth-provider.ts | main branch |
| G3 | https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/ | 2026-05-19 |
| V1 | https://code.visualstudio.com/docs/agents/reference/mcp-configuration | 2026-11-11 |
| V2 | https://code.visualstudio.com/api/extension-guides/ai/mcp | undated |
| V3 | https://github.com/microsoft/vscode/blob/main/src/vs/workbench/api/browser/mainThreadAuthentication.ts and `product.json` | main branch |
| C1 | https://cursor.com/docs/context/mcp | undated |
| C2 | https://forum.cursor.com/t/mcp-oauth-cimd-support-plans-and-timelines/148096 | last post 2026-06-01 |
| X1 | https://docs.x.ai/developers/tools/remote-mcp | undated |
| X2 | https://docs.x.ai/grok/connectors | undated |
| M1 | https://www.meta.com/help/superintelligence/1687253048996149/ | undated |
| MS | https://learn.microsoft.com/microsoft-copilot-studio/mcp-add-existing-server-to-agent | 2026-11-06 |
| MI | https://docs.mistral.ai/le-chat/knowledge-integrations/connectors/mcp-connectors | undated |
| GC1 | https://cloud.google.com/run/pricing | undated |
| GC2 | https://docs.cloud.google.com/run/docs/about-instance-autoscaling | undated |
| GC3 | https://docs.cloud.google.com/run/docs/configuring/session-affinity | undated |
| GC4 | https://docs.cloud.google.com/run/docs/configuring/services/secrets | undated |
| GC5 | https://docs.cloud.google.com/armor/docs/rate-limiting-overview | undated |
| GC6 | https://cloud.google.com/armor/pricing | undated |
| GC7 | https://cloud.google.com/load-balancing/pricing | undated |
| GC8 | https://docs.cloud.google.com/armor/docs/integrating-cloud-armor (search excerpt only) | undated |
| I1 | https://workos.com/docs/authkit/mcp.md | undated |
| I2 | https://workos.com/docs/authkit/jwt-templates and https://workos.com/pricing | undated |
| I3 | https://auth0.com/docs/get-started/applications/dynamic-client-registration and https://auth0.com/docs/get-started/auth0-overview/create-applications/cimd-overview | undated |
| I4 | https://auth0.com/ai/docs/mcp/guides/resource-param-compatibility-profile and https://auth0.com/ai/docs/mcp/get-started/authorization-for-your-mcp-server | undated |
| I5 | https://auth0.com/blog/auth0-auth-for-mcp-servers-generally-available/ and https://auth0.com/pricing | 2026-05-06 (old) / undated |
| I6 | https://stytch.com/docs/connected-apps/oauth-learn-more/client-types and https://stytch.com/docs/api-reference/consumer/api/connected-apps/tokens/connected-app-access-token-object | undated |
| I7 | https://www.keycloak.org/securing-apps/mcp-authz-server and https://www.keycloak.org/2026/04/keycloak-2660-released | undated / 2026-04-08 (old) |
| I8 | https://github.com/cloudflare/workers-oauth-provider (storage-schema.md, docs/authorization-server.md) | v1.2.4, 2026-10-22 |
| I9 | https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/ | 2026-11-03 |
| I10 | https://cloud.google.com/identity-platform/pricing; Firebase discovery document checked directly | undated |
| I11 | https://docs.cloud.google.com/mcp/set-up-authentication-mcp-servers; `accounts.google.com/.well-known/openid-configuration` | undated |
| I12 | https://clerk.com/changelog/2026-08-06-client-id-metadata-documents, https://docs.descope.com/agentic-identity-hub/core-components/mcp-servers/registration-methods, https://docs.logto.io/integrate-logto/third-party-applications/dynamic-apps, and their pricing pages | 2026-08-06 / undated |
