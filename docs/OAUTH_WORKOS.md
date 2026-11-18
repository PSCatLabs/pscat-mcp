# OAuth with WorkOS AuthKit: setup runbook

How to configure WorkOS AuthKit as the identity provider for pscat-mcp's
`oauth` method, wire the deployment, and check that ChatGPT, claude.ai and
Claude Code can sign in. It is the working guide for the acceptance of
[#119](https://github.com/PSCatLabs/pscat-mcp/issues/119) (a ChatGPT
connector signs in; a user who is not on the allowlist gets `403`). The choice
of AuthKit and the reasons are in [`AUTH_STRATEGY.md`](AUTH_STRATEGY.md)
(approved 2026-11-17); this file is only the steps.

**Two readers.**

* **Owner**: has the WorkOS dashboard. Sections 2 (AuthKit), 5 and 6 (a token
  and the checker) and 7 (clients).
* **pscat-cloud**: wires Terraform and Cloud Run. Sections 3 (settings) and 4
  (checks without a client), and 8 (the order).

Everyone reads section 1 (one decision) and section 9 (diagnosing failures).

**Legend.** A statement marked **`unverified`** is not confirmed in a primary
source or against a live AuthKit. Facts taken from WorkOS's public
documentation cite the page in [Sources](#sources), all read on 2026-11-17.
Where a claim is only stated here as a plan, it is a plan, not a result. Tick
the `unverified` items off as they are confirmed on staging (section 8), and
update this file.

**What the server enforces** (the rules in `src/oauth.rs`; section 6 checks a
real token against them): a JWT access token whose header has a supported
`alg` (RS256/384/512, PS256/384/512, ES256, ES384, EdDSA), a `kid` that is in
the issuer's JWKS and no `crit`; `iss` equal to `PSCAT_MCP_OAUTH_ISSUER`
exactly; `aud` containing `PSCAT_MCP_OAUTH_RESOURCE` (case-sensitive, one
trailing `/` ignored); `exp` in the future and `nbf` not in the future (60 s
leeway); a non-empty `sub`; and a user on the allowlist. An `email:` entry
matches only when the token has an `email` and `email_verified` that is the
JSON boolean `true`.

## 1. Decision: the resource URL

The **resource URL** is the exact public URL clients connect to, for example
`https://mcp.example.com/mcp`. The same string goes into three places, and a
difference in any of them makes sign-in fail:

| Where | Setting | Who |
|---|---|---|
| WorkOS dashboard | a **Resource Indicator** | owner |
| pscat-mcp | `PSCAT_MCP_OAUTH_RESOURCE` | pscat-cloud |
| The connector | the server URL typed into ChatGPT, claude.ai, `claude mcp add` | users |

Rules:

1. **https, the custom domain users type, the `/mcp` path, no trailing slash,
   no query, no fragment.** Use the custom domain, not the `*.run.app` URL,
   unless that is what users will type. The server ignores one trailing `/`
   when comparing `aud`, but WorkOS's matching of a Resource Indicator is
   **`unverified`**, so write the string identically everywhere.
2. **One URL per environment.** Staging and production have different
   resource URLs. A staging token then fails on production with
   `wrong_audience`, which is intended.
3. Changing the URL later means a new Resource Indicator, a new deployment
   setting and a new connector for every user. Decide it before section 2.

Record the two chosen URLs in the issue for #119 before starting. Below,
`https://mcp.example.com/mcp` stands for a resource URL and
`https://<authkit-domain>` for the AuthKit domain of the matching WorkOS
environment (shown in the dashboard; it is the token `iss`, section 2.1).

## 2. AuthKit setup (owner)

Do this once per environment, staging first (section 8). Use one WorkOS
environment per deployment environment if the dashboard offers separate ones
(then the issuer, signing keys, JWT template and users are separate for
staging and production). Whether your account has separate environments, and
that each has its own AuthKit domain, is **`unverified`**: the WorkOS pages
read do not say. If there is only one, add both resource URLs as indicators
(step 4), keep one JWT template, and expect the same `iss` for both
deployments; the `aud` still keeps their tokens apart.

1. **Find the issuer.** The AuthKit domain is the issuer: the MCP guide gives
   `https://authkit_domain` as the `iss` to verify and publishes its metadata
   at `https://<authkit-domain>/.well-known/oauth-authorization-server`
   [W1]. Copy the `issuer` member of that document verbatim (no trailing
   slash in WorkOS's examples). That string is `PSCAT_MCP_OAUTH_ISSUER`. The
   server refuses a discovered `issuer` that differs from it. The JWKS is
   `https://<authkit-domain>/oauth2/jwks` [W1][W3]; the server finds it by
   discovery, so `PSCAT_MCP_OAUTH_JWKS_URL` stays unset.
2. **Enable Client ID Metadata Documents (CIMD).** In the dashboard under
   *Connect*, then *Configuration*: CIMD "is off by default" [W1]. Turn it
   on. ChatGPT, claude.ai, Claude Code, Codex, opencode, VS Code and
   Antigravity can sign in with CIMD alone [AUTH_STRATEGY Q1].
3. **Enable Dynamic Client Registration (DCR)** in the same area [W1]. Cursor,
   Gemini CLI, Le Chat and Copilot Studio's automatic mode still need it
   [AUTH_STRATEGY Q1]. The exact toggle labels are **`unverified`** (the
   page was read through a summarising fetch).
4. **Add the Resource Indicator.** Add the resource URL of section 1 as a
   valid Resource Indicator [W1]. Clients send it as the `resource`
   parameter and AuthKit issues the token "with an `aud` claim that matches
   the requested `resource`" [W1]. With staging and production in one
   environment, add both URLs. Do **not** use a wildcard indicator: it
   cannot be the default and it would accept audiences you did not choose.
5. **Set the default Resource Indicator** to the environment's own resource
   URL (the *...* menu, *Set as default*) [W1]. Requests that omit
   `resource` then still get the right audience. Without a default the
   audience falls back to the environment's client ID [W1], which fails here
   with `wrong_audience`. With two URLs in one environment there is no safe
   default: leave it unset and rely on clients sending `resource` (the MCP
   spec requires it; each client is **`unverified`**, see section 7).
6. **Create the JWT template** (*Authentication* page, under *Features*)
   [W2]. A template is rendered into "every access token the environment
   issues through a user sign-in or consent flow", MCP clients registered by
   DCR or CIMD included, and there is one template per environment [W2].
   Add exactly the claim names the server reads, `email` and
   `email_verified`:

   ```
   {
     "email": {{ user.email }},
     "email_verified": {{ user.email_verified }}
   }
   ```

   The syntax (`{{ user.email }}`) and the variable `user.email_verified`
   are from [W2]. These points are **`unverified`**, so check them with a
   real token (sections 5 and 6):
   * that the dashboard accepts the bare names `email` and `email_verified`
     (the WorkOS examples use namespaced keys such as `urn:myapp:email`);
   * that `email_verified` renders as a JSON boolean and not the string
     `"true"` (the page does not say). If it is a string, every `email:`
     entry fails with `403 not_allowed`; the fallback is `sub:` entries,
     which ties the allowlist to WorkOS user IDs (more lock-in);
   * that AuthKit marks an address verified only after it was verified.
   The access token's default lifetime is five minutes [W1], so run the
   checker on a fresh token.
7. **Redirect URIs.** The WorkOS pages read do not describe redirect-URI
   settings for DCR or CIMD clients [W1][W4][W5]. A DCR or CIMD client sends
   its own redirect URI, so nothing should need adding by hand;
   whether AuthKit validates them against a list, and whether it accepts
   a loopback URI on any port, is **`unverified`**. These are the
   redirect URIs the clients use. If AuthKit rejects one at the
   authorization step, this is the list to compare with:

   | Client | Redirect URI | Source |
   |---|---|---|
   | ChatGPT | `https://chatgpt.com/connector_platform_oauth_redirect` (with RFC 9207 `iss`), otherwise a per-connection URI | [O1] |
   | claude.ai, Desktop, mobile | `https://claude.ai/api/mcp/auth_callback` | [AUTH_STRATEGY Q1] |
   | Claude Code | loopback `localhost` and `127.0.0.1`, **any port** | [A1] |
   | Codex CLI | a loopback callback from `codex mcp login`; the exact value is not in the strategy note | **`unverified`** |
   | Cursor | `https://www.cursor.com/agents/mcp/oauth/callback`, `http://localhost:8787/callback` | [C1] |
   | VS Code | `http://127.0.0.1:33418`, `https://vscode.dev/redirect` | [V2] |

   Claude Code on any loopback port is on the strategy note's list of things
   to verify on staging (WorkOS does not document it).
8. **Consent screen.** AuthKit shows its consent page the first time a user
   authorizes a third-party application [W4]. That is expected on a first
   sign-in from each client.
9. **Who can sign up** is the owner's choice in AuthKit. It is not the access
   control: the allowlist in pscat-mcp is, and someone who can sign in but
   is not listed gets `403`. Keep the two separate.

## 3. pscat-mcp settings

Wire these in Terraform for the Cloud Run service. They are the only
things the server needs; there is no OAuth secret (it only checks public
signatures).

| Environment variable | Value | Notes |
|---|---|---|
| `PSCAT_MCP_AUTH` | `bearer,oauth` | **Never `none`, alone or in a list.** `oauth` and `none` on one URL stop the clients from ever starting sign-in (they would run anonymously). The server does not refuse that combination yet (tracked in #153), so this is a deployment rule. `bearer` keeps CI, scripts and `Authorization`-header clients working. |
| `PSCAT_MCP_AUTH_TOKEN_FILE` | existing bearer-token file | `bearer` needs at least one token; unchanged. |
| `PSCAT_MCP_OAUTH_ISSUER` | `https://<authkit-domain>` | Copied from the metadata `issuer` (2.1). Exact; a trailing `/` makes every token `wrong_issuer`. |
| `PSCAT_MCP_OAUTH_RESOURCE` | `https://mcp.example.com/mcp` | The resource URL of section 1, byte for byte the Resource Indicator. |
| `PSCAT_MCP_OAUTH_ALLOWED_USERS_FILE` | a path such as `/secrets/oauth-allowlist` | A Secret Manager secret mounted as a file (volume); not an env var, since it holds personal data. One entry per line: `email:person@example.com` (production: `email:` only). `#` comments and blank lines are skipped; an empty list, or an entry that is neither `sub:` nor `email:`, stops the server from starting. |
| `PSCAT_MCP_OAUTH_SCOPES` | **unset** | No scope check. WorkOS advertises the scopes `email`, `offline_access`, `openid` and `profile` [W1]; whether `scope` in the access token carries them is **`unverified`**, so do not require one. If a scope is ever required, the token must carry it in `scope` or `scp`. |
| `PSCAT_MCP_OAUTH_JWKS_URL` | **unset** | Discovery works for AuthKit. |
| `PSCAT_MCP_AUTH_NONE_*` | **unset** | Part of `none`, which must be off. |

Deployment checks:

* The allowlist is read **once at startup**. Adding or removing a person
  means a new secret version and a new revision (or a restart).
* The service needs **outbound HTTPS to `<authkit-domain>`** for the
  discovery document and the JWKS. With VPC egress set to all traffic and
  no Cloud NAT, every JWT fails with `jwks_unavailable` (section 9).
* `GET /.well-known/oauth-protected-resource/mcp` (and
  `/.well-known/oauth-protected-resource`) must be reachable without
  authentication, as must `POST /mcp` to get its `401` challenge: the Cloud
  Run invoker must be public (`--allow-unauthenticated`), the application is
  what checks the token. See [`CLOUD_RUN.md`](CLOUD_RUN.md#oauth-chatgpt-and-claudeai-connectors).
* The metadata URL in the `401` challenge comes from
  `PSCAT_MCP_OAUTH_RESOURCE`, never from the `Host` header, so the custom
  domain must be the one in the setting.
* Startup never contacts AuthKit, so an AuthKit outage cannot crash-loop the
  service; it only makes JWTs with a key not yet fetched fail.

## 4. Check the deployment without a client (pscat-cloud)

Run these from a shell, with your real values in place of the placeholders.
No token is involved.

```bash
# 1. No token: expect 401 with WWW-Authenticate: Bearer realm="pscat-mcp",
#    resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource/mcp"
curl -si -X POST https://mcp.example.com/mcp -H 'content-type: application/json' -d '{}' | head -n 12

# 2. The metadata: expect 200 and JSON whose "resource" is the resource URL and
#    whose "authorization_servers" holds exactly the issuer.
curl -s https://mcp.example.com/.well-known/oauth-protected-resource/mcp

# 3. The issuer's own metadata: "issuer" must equal PSCAT_MCP_OAUTH_ISSUER,
#    and it should advertise client_id_metadata_document_supported (CIMD,
#    unverified member name), a registration_endpoint (DCR) and S256.
curl -s https://<authkit-domain>/.well-known/oauth-authorization-server
```

If step 1 answers `200` instead of `401`, the service is running with `none`
(or no auth): fix that first, no client will ever sign in.

## 5. Get a token to check (owner or pscat-cloud)

The checker (section 6) needs a real access token. The server never logs
tokens, and clients do not show them, so there are two ways. **Handle a token
like a password**: it is valid for about five minutes and names a person. Do
not paste it in an issue, a chat or a PR, and do not put it on a command
line (the shell history keeps it).

* **An OAuth debugger that shows the access token**, such as MCP
  Inspector's OAuth flow against the resource URL (**`unverified`**: check
  that its current version displays the token).
* **A manual authorization-code flow** against AuthKit, in a throwaway shell.
  Standard OAuth; **`unverified`** against AuthKit:

  1. Register a client with the `registration_endpoint` of the metadata
     (DCR, RFC 7591) with `redirect_uris: ["http://127.0.0.1:8765/callback"]`,
     `token_endpoint_auth_method: "none"`, and note the `client_id`.
  2. Make a PKCE pair:

     ```bash
     verifier=$(openssl rand -base64 48 | tr -d '=+/\n' | cut -c1-64)
     challenge=$(printf %s "$verifier" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
     ```
  3. Open, in a browser, the `authorization_endpoint` with
     `response_type=code`, `client_id`, `redirect_uri`, `code_challenge`,
     `code_challenge_method=S256`, `scope=openid email profile`,
     `resource=<the resource URL, URL-encoded>` and a `state`. Sign in as
     the account under test. The browser lands on the redirect URI (the page
     fails to load; that is fine); copy the `code` from the address bar.
  4. Exchange it and keep the result out of the terminal:

     ```bash
     OAUTH_CHECK_TOKEN=$(curl -s <token_endpoint> \
       -d grant_type=authorization_code -d client_id=<client_id> -d code=<code> \
       -d redirect_uri=http://127.0.0.1:8765/callback -d code_verifier="$verifier" \
       | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
     export OAUTH_CHECK_TOKEN
     ```

     (The command line holds the one-time code and verifier, not the token,
     and the assignment does not echo it. Run `unset OAUTH_CHECK_TOKEN` when
     done.)

## 6. The token checker: `scripts/oauth-check.py`

Standard library only. It decodes the token **without verifying the
signature** and prints a pass/fail checklist of what the server checks in
`src/oauth.rs`, each failure with the `reason` the server would log. Use it
to find out why a token is refused before involving a client.

```bash
# the token is in $OAUTH_CHECK_TOKEN (or name another variable with --token-env)
python3 -I scripts/oauth-check.py \
  --issuer 'https://<authkit-domain>' \
  --resource 'https://mcp.example.com/mcp' \
  --allow-user 'email:person@example.com' \
  --fetch-jwks
```

* `--issuer` and `--resource` are the two settings of section 3, exactly.
* `--allow-user sub:X` or `email:Y` (repeatable) is an allowlist entry; the
  checklist says which entry would match. The script never prints the
  token's `sub` or `email`, only whether they are present and which of your
  entries match, so its output can be shared.
* `--jwks-file FILE` checks the `kid` against a saved JWKS (offline);
  `--fetch-jwks` fetches it the way the server does (discovery, then the
  `jwks_uri`) and prints each URL it fetched. The token is never sent. An
  optional `--jwks-url` skips discovery.
* `--scope S` (repeatable) and `--leeway SECS` (default 60) mirror
  `PSCAT_MCP_OAUTH_SCOPES` and `PSCAT_MCP_OAUTH_LEEWAY_SECS`.
* Exit status `0` when no check failed, `1` when one failed, `2` for a usage
  error. Skipped checks (no JWKS, no allowlist, no scope) are listed.
* **It does not verify the signature**, so `0` means "nothing here would make
  the server refuse the claims", not "the server will accept it". A key that
  does not fit the `alg` is flagged (`bad_signature`), a wrong key or a
  tampered token is not.
* The token is read from an environment variable, never from the command
  line (a token pasted after an option is rejected without being echoed),
  and neither it nor any part of it is printed.

What to look for on a real AuthKit token (the `unverified` items of
section 2): `email` present, `email_verified` a **JSON boolean**, `aud`
containing the resource URL, `iss` equal to the issuer, a `kid`.

## 7. Client setup checks

Do these on staging first. For each client, the expected result is: the
connector shows an AuthKit sign-in, then lists the server's tools, and a
`render_postscript` call returns an image. For each, also sign in once as an
account that is **not** on the allowlist: expect the call to fail with `403`
(`not_allowed` in the server log, section 9). How each client words a `403`
is **`unverified`**; the server sends no `error` parameter so a client should
not start a consent loop.

1. **ChatGPT** (developer mode, app/connector). Add an app with the resource
   URL and authentication **OAuth** [O1]. Expect: ChatGPT reads the
   metadata, uses CIMD (or DCR as the fallback), opens the AuthKit sign-in,
   and returns. ChatGPT cannot send a static header, so this is its only
   authenticated path. Check that the redirect URI in the AuthKit flow is
   the one in 2.7 and that the token's `aud` is the resource URL (check
   section 9 `wrong_audience` if not). Whether ChatGPT picks CIMD is
   **`unverified`**.
2. **claude.ai** (Customize, Connectors, Add custom connector; Team and
   Enterprise: an Owner adds it) [A2]. Enter the resource URL and keep the
   **Detected** OAuth client option. Claude needs `none` in the issuer's
   `token_endpoint_auth_methods_supported` for CIMD [A1]; whether AuthKit
   advertises it is **`unverified`** (check the metadata of 4.3). claude.ai
   registers a fresh DCR client on every connection if CIMD is not used [A1].
3. **Claude Code.** `claude mcp add --transport http pscat https://mcp.example.com/mcp`,
   then `/mcp` to sign in [A4]. The loopback redirect on a random port is
   **`unverified`** with AuthKit. If it fails, fall back to a bearer token
   (`--header "Authorization: Bearer $TOKEN"`; a configured
   `Authorization` header disables OAuth for that server [A4]), and report
   it.
4. **Codex, Cursor, VS Code** (not part of #119's acceptance). Codex:
   `codex mcp add pscat --url ...` then `codex mcp login pscat` [O3]. Cursor
   needs DCR [AUTH_STRATEGY Q1]; VS Code works with CIMD in Microsoft builds
   only, otherwise DCR. All **`unverified`** against AuthKit.
5. **A bearer client** (`Authorization: Bearer <static token>`) must still
   work beside OAuth, and a wrong token must get `401`, never anonymous.

## 8. Order: staging, then production

1. **Prepare (all).** Decide the two resource URLs (section 1). pscat-cloud
   creates the allowlist secret for staging (a few `email:` lines, including
   the owner and one test account that is *not* on it for the `403` check).
2. **Staging, AuthKit (owner).** Section 2 on the staging environment.
3. **Staging, deployment (pscat-cloud).** Section 3 with `PSCAT_MCP_AUTH=bearer,oauth`.
   **This replaces the current `bearer,none` window**: `none` must be removed
   in the same change, or ChatGPT and claude.ai never start sign-in. Tell the
   owner when staging stops being anonymous.
4. **Staging, checks.** Section 4 (no token), then sections 5 and 6 (one real
   token, fresh, from the owner's account: every check passes), then
   section 7 (ChatGPT first, then claude.ai, then Claude Code, then the
   non-allowlisted account). Record the result and the logs in #119.
5. **Update the unverified items.** For each `unverified` in this file that
   the staging run settled, edit this file and the "To verify on staging"
   list in [`AUTH_STRATEGY.md`](AUTH_STRATEGY.md). If `email_verified` is not
   a boolean, or the bare claim names are refused, stop and decide between
   `sub:` entries and Auth0 (the strategy note's fallback) before production.
6. **Production** only after #119 is accepted on staging: repeat steps 2 to
   4 for the production environment, with its own resource URL, issuer (if
   it is a separate WorkOS environment), and an invite-only allowlist of
   `email:` entries. `PSCAT_MCP_AUTH=bearer,oauth`; **no `none`**, there is
   no public no-auth production service (owner decision, 2026-11-17; the
   removal of `none` itself is #154). The checks of step 4 are short in
   production: sections 4 and 6, and one client.
7. **Rollback.** To go back, set `PSCAT_MCP_AUTH=bearer` and redeploy. Do
   not bring `none` back in production.

## 9. Diagnosing a failed sign-in

Two kinds of log line tell the story (log level `info`, the default).

* `rejected unauthenticated request` (WARN), with the fields `reason` (the
  answer's code: `missing_credentials`, `malformed_credentials`,
  `invalid_token`, `not_allowed`, `insufficient_scope`) and `rejected_by` (the
  methods that rejected a credential, comma separated; empty when none was
  presented). One line per refused request.
* `oauth: token rejected` (INFO), with a fixed `reason` naming the exact cause
  (table below). It is logged just before the line above. A JWT is never
  logged, nor a subject or an email. `not_a_jwt` is logged only at `debug`
  (it is usually a static token meant for `bearer`).
* `fetching the OAuth issuer's JWKS failed` (WARN), with an `error`.

In Cloud Logging, search the service's logs for the text `oauth: token
rejected`, then read `reason`. The fields are on the JSON line (their nesting
depends on the log format; look at one line). To reproduce a failure offline,
get a token for the same sign-in (section 5) and run the checker (section 6):
the `reason=` it prints is the same vocabulary.

| Symptom | Log: `reason` (`oauth: token rejected`) | Likely cause and fix |
|---|---|---|
| The client never opens a sign-in; the first call works anonymously | no rejection line | The service accepts anonymous requests: `none` is on, or no auth. Remove `none` (section 3, check 4.1). |
| The client reports it found no authorization server / metadata | `missing_credentials` for the probe is normal; no `oauth:` line | `GET /.well-known/oauth-protected-resource/mcp` is not `200` or its `resource` differs from the connector URL: ingress or IAM blocking it, `PSCAT_MCP_OAUTH_RESOURCE` wrong (4.1, 4.2). |
| AuthKit shows an error before sign-in (invalid resource, redirect, unknown client) | none (the request never reaches the server) | Resource Indicator missing or different from the URL (2.4); CIMD/DCR off (2.2, 2.3); redirect URI rejected (2.7). The wording of AuthKit's errors is **`unverified`**. |
| Sign-in completes, then `401` on every call | `wrong_audience` | `aud` is not the resource URL. The Resource Indicator differs from `PSCAT_MCP_OAUTH_RESOURCE` (a typo, `run.app` vs the custom domain, a trailing slash), or the client sent no `resource` and AuthKit used its default or the client ID (2.4, 2.5). Run the checker to see the shape of `aud`. |
| `401` after a correct sign-in | `wrong_issuer` | `PSCAT_MCP_OAUTH_ISSUER` is not the token's `iss`: a trailing slash, the other WorkOS environment, or a staging issuer on production (2.1). |
| `401` on every call, JWKS warning in the log | `jwks_unavailable` | The service cannot reach `<authkit-domain>` (egress with no Cloud NAT, a firewall) or it timed out (`PSCAT_MCP_OAUTH_JWKS_TIMEOUT_MS`, 5000). Check the `error` of the warning. Static bearer tokens still work. |
| `401`, only some tokens | `unknown_kid` | The token's `kid` is not in the JWKS the server holds (usually just after a key rotation: the server refetches at most every `PSCAT_MCP_OAUTH_JWKS_REFETCH_SECS`, 30 s; try again), or the JWKS of another environment. |
| `401` | `bad_signature` | The key does not verify the token: a token from another environment with a `kid` in common, a tampered token, or a key whose type does not fit the `alg`. The checker flags the last one. |
| `401` after some minutes, a refresh fails | `expired` | Access tokens last about five minutes [W1]; the client must refresh. If it does not, it is a client problem (re-add the connector). A skewed server clock would also show this (leeway 60 s). |
| `401` right after sign-in | `not_yet_valid` | `nbf` in the future: a skewed clock beyond the 60 s leeway. |
| `401`, no `oauth:` INFO line, `rejected_by` names the methods that rejected it | `not_a_jwt` (debug) | The token is not a JWT: AuthKit issued an opaque token, or a wrong static token. The server needs JWT access tokens (`AUTH_STRATEGY` Q3). Check the checker's `shape`. |
| `401` | `unsupported_alg`, `missing_kid`, `missing_subject`, `token_too_large` | The token header or claims are outside what the server accepts; the checker names which. |
| `403` after a correct sign-in | `not_allowed` (`reason=not_allowed` on the request line, no `error` parameter in the challenge) | The user is not on the allowlist, or an `email:` entry cannot match: the token has no `email`, or `email_verified` is missing, `false` or a **string** (fix the JWT template, 2.6), or the address differs (matching ignores case only). Run the checker with `--allow-user`. After editing the list, deploy a new revision (read once at startup). |
| `403` | `insufficient_scope` | `PSCAT_MCP_OAUTH_SCOPES` is set and the token lacks one (`scope`/`scp`). Unset it (section 3). |
| The service does not start | none (startup error, no secret echoed) | `oauth` without `PSCAT_MCP_OAUTH_ISSUER`, `_RESOURCE`, or an allowlist (an empty, unreadable or malformed file); `http` URLs off loopback; `none` with async jobs or in-memory artifact links. |
| ChatGPT works, claude.ai or Claude Code does not (or the reverse) | per the rows above | Compare the redirect URI and client registration method (CIMD/DCR) per client (2.2, 2.3, 2.7). |

## Sources

All read on 2026-11-17 (WorkOS's pages carry no date). The pages were read
through a summarising fetch; text in quotation marks is what it returned as a
quote, the rest is paraphrase.

| ID | Source |
|---|---|
| W1 | https://workos.com/docs/authkit/mcp: CIMD off by default (dashboard *Connect*, *Configuration*), DCR, Resource Indicators and the default indicator, `aud` from `resource`, access-token lifetime, metadata and JWKS URLs, scopes |
| W2 | https://workos.com/docs/authkit/jwt-templates: templates on the *Authentication* page, `{{ }}` variables, one per environment, applied to MCP-client tokens; boolean rendering not stated |
| W3 | https://workos.com/docs/authkit/connect/oauth/verifying-tokens: JWKS at `/oauth2/jwks`; says nothing about `alg`, `kid`, `exp`, `email` |
| W4 | https://workos.com/docs/authkit/connect/oauth: consent page; nothing on DCR, CIMD or redirect rules |
| W5 | https://workos.com/docs/authkit/connect and https://workos.com/docs/authkit/connect/dcr: nothing on DCR, CIMD, redirect URIs or environments |
| O1, O3, A1, A2, A4, C1, V2, AUTH_STRATEGY Q1 | the same-named rows of the [Sources](AUTH_STRATEGY.md#sources) table of the strategy note (read 2026-11-17) |
