# Platform prompts: what a client does with artifact links

`scripts/agent-eval.sh` drives only Claude Code and Codex, against a server
it starts itself with memory links (`pscat://` resources). It can't drive
claude.ai, Claude Desktop or ChatGPT, and it can't reach a server with
Cloud Storage links, whose links are `https` signed URLs. This page has a
prompt to paste into those clients, so the owner can measure what each does
with the links `delivery: "url"` and `"both"` return (issue #98, step 1;
#155). The harness side, for Claude Code, is
[`LOCAL_TESTING.md`](LOCAL_TESTING.md#artifact-link-evals).

## What you need

* A server running with `PSCAT_MCP_ARTIFACT_LINKS=gcs`, a bucket
  (`PSCAT_MCP_ARTIFACT_BUCKET`) and the grants in
  [`CLOUD_RUN.md`](CLOUD_RUN.md#cloud-storage-artifact-links). Below, its
  address is written `https://<your-server-host>/mcp`.
* A way for the client to authenticate. **`gcs` is refused with the `none`
  auth method**, so a client that can connect only with no auth cannot reach
  a `gcs` server. ChatGPT connects only with no auth or OAuth, so it has to
  wait for OAuth (the record in
  [`LOCAL_TESTING.md`](LOCAL_TESTING.md#real-bucket-check), step 6). Check
  what each other client supports; Claude Code can send a bearer token:
  `claude mcp add --transport http pscat-gcs https://<your-server-host>/mcp --header "Authorization: Bearer <token>"`
  (remove it afterwards).
* The server added to the client as a custom MCP server or connector, with
  its tools turned on for the chat.

Signed URLs are bearer capabilities: anyone holding one can fetch that file
until it expires (30 minutes by default). Use a throwaway chat, don't share
the transcript, and never commit a real link. When you record results in the
repository, replace every link with `<signed-url>`.
With a public origin (#163) the links are short links on the server,
`https://<your-server-host>/a/<render_id>/<n>.<ext>?e=...&s=...`, which
redirect to a signed URL when opened and keep working when a client appends
a parameter such as `utm_source`; they are bearer capabilities in the same
way. Record them as `<short-link>`.

## The prompt

Paste everything between the lines into a new chat of each client, with the
PSCat server enabled. It is the same for claude.ai, Claude Desktop, ChatGPT
and Claude Code.

---

I'm testing how this chat client handles files that an MCP tool returns.
Use the PSCat render tool (`render_postscript`) and report exactly what you
receive. Do not invent, guess or reconstruct anything. If you did not
receive something, write "not received". If you could not do something,
write "could not" and quote the error text.

Make these four calls, one after the other:

A. `delivery` set to `"url"`: a PNG, 800 x 500 points, of this program:

```
0.1 0.3 0.7 setrgbcolor 0 0 800 500 rectfill 1 0.8 0 setrgbcolor 400 250 150 0 360 arc fill
```

B. `delivery` set to `"url"`: an SVG (`format` `"svg"`), 400 x 300 points,
of this program, which makes an SVG over 1 MB:

```
0 1 13000 {dup 37 mul 400 mod exch 53 mul 300 mod 3 3 rectfill} for
```

C. `delivery` set to `"both"`: the same PNG as A.

D. `delivery` set to `"both"`: the same SVG as B.

If a call fails, report the error and go on with the next one. Don't retry
with a different `delivery` unless I ask.

For each call, try to open or show what the tool gave you for the file, with
whatever you can do in this chat: show an image in your reply, give me a
clickable link, open or fetch the link with a browsing or code tool, or read
it as a resource. Say which of these you tried and what happened. Only say
you saw an image or file contents if you really received them, and then
describe what you saw (for the PNG, its colors and shapes; for the SVG, its
first 100 characters).

Then give me one table, with one row per call (A, B, C, D) and exactly these
columns:

| Call | Content received | Fields of the structured result | Exact link text | Opened or shown? | Error |
|---|---|---|---|---|---|

* **Content received**: every kind of content you got from the tool, from
  this list: text, image, resource_link, embedded resource (text or blob),
  structuredContent, nothing. Say "image" only for an image you could see.
* **Fields of the structured result**: which of `render_id`, `returned_as`,
  `download_url`, `expires_at` you saw, with their values (for
  `returned_as`: `link`, `image`, `text` or `blob`).
* **Exact link text**: the link character for character, if you can quote
  it. Otherwise "cannot quote it", and why.
* **Opened or shown?**: "shown in the reply", "clickable link only",
  "fetched, saw ...", "could not open", or "did not try", with the reason.
* **Error**: the exact error text, or "none".

After the table, in one or two sentences, say which of these happened for
the links: you showed them to me, you fetched them, or you ignored them.

---

## Recording the result

Fill in one row per client and date. The tool call details a client shows
(expanding the tool call, or a developer or debug view) let you compare the
raw result with what the assistant claims; use them where you can, because
an assistant can be wrong about what it received.

| Client, version, date | A (PNG, `url`) | B (large SVG, `url`) | C (PNG, `both`) | D (large SVG, `both`) | Notes |
|---|---|---|---|---|---|
| claude.ai | | | | | |
| Claude Desktop | | | | | |
| ChatGPT | | | | | |
| Claude Code | | | | | |

In each cell write one of: **shown** (the assistant displayed the file or
the user could see it in the client), **fetched** (the assistant read the
file's content, not shown), **link only** (the link reached the user or the
assistant but nothing opened it), **ignored** (the link did not reach the
assistant, or it did not mention it), or **error** (with the code). For C and
D add whether the inline PNG or SVG was also shown.

## ChatGPT widget

For the opt-in image widget (#162): a server with `gcs` links (short links
preferred) and `PSCAT_MCP_CHATGPT_WIDGET=true`, added to ChatGPT as a
developer-mode connector with OAuth. Refresh the connector after turning the
option on, so ChatGPT reads the new `tools/list`. Record the result in
[`LOCAL_TESTING.md`](LOCAL_TESTING.md#chatgpt-widget-check).

Paste this into a new chat with the connector on:

---

Use the PSCat `render_postscript` tool once, with `delivery` set to `"url"`,
`format` `"png"`, 400 x 250 points, `dpi` 144, of this program:

```
0.1 0.3 0.7 setrgbcolor 0 0 400 250 rectfill 1 0.8 0 setrgbcolor 200 125 80 0 360 arc fill
```

Then call it again with the same program and no `delivery`. After each call,
tell me in one sentence what you received from the tool. Do not describe the
picture unless you can see it.

---

What the owner should see, outside the model's reply:

1. **First call (`url`).** Under the tool call, a bordered panel with the
   picture: a blue rectangle with a yellow disc in the middle, 800 x 500
   pixels scaled to the panel's width, and under it the link in small
   monospace text (`https://<your-server-host>/a/<render_id>/1.png?e=...&s=...`
   with short links, a `storage.googleapis.com` URL without).
2. **Second call (default `inline`).** The same panel with only the text
   "The pictures were returned inline, which this view cannot show. Ask for
   the render again with delivery "url" to see them here."
3. **After the link's TTL** (30 minutes by default), reopening the chat:
   "this link expired at ...; render again for a new one." in place of the
   picture.

Failures to tell apart, each worth a row in the record:

* **no panel at all**: ChatGPT did not use the template (a developer-mode
  connector may not render widgets, or it did not re-read `tools/list`);
* **a panel that stays at "Waiting for the render result."**: the page
  loaded but received no result (`window.openai.toolOutput` empty and no
  `ui/notifications/tool-result`);
* **"the picture could not be loaded from its link."**: the `<img>` was
  blocked (the sandbox's policy, or the redirect target) or the link failed;
  open the link from the panel's text in a browser to tell them apart;
* **a panel with the link and a broken image icon**: the same, in a host that
  does not fire `error`.

Treat the link like any other: it is a bearer capability until it expires.
