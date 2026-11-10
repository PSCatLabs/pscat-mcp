#!/usr/bin/env bash
# Container smoke test for pscat-mcp-server.
#
# Builds the image, runs it hardened (read-only root filesystem, no
# capabilities, no-new-privileges, 1 GiB / 1 CPU / 64 PIDs), and drives it
# over Streamable HTTP with curl:
#
#   * /healthz answers 200
#   * POST /mcp without a token is refused with 401
#   * MCP initialize (protocol 2025-06-18) + notifications/initialized
#   * tools/call render_postscript -> PNG, decoded and checked (signature, 200x100)
#   * tools/call render_postscript format=pdf -> starts with %PDF-
#   * an infinite loop is stopped (timeout / limit_exceeded) and the server
#     still answers /healthz and renders normally afterwards
#
# Usage: scripts/container-smoke.sh
#   IMAGE=pscat-mcp:smoke   image tag to build and run
#   SKIP_BUILD=1            reuse an existing image
#   SMOKE_PORT=18080        host port (default: a free port on 127.0.0.1)
#
# Needs: docker, curl, python3 (only the standard library; run with -I).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-pscat-mcp:smoke}"
NAME="pscat-mcp-smoke-$$"
PROTOCOL_VERSION="2025-06-18"

FAILURES=0
CONTAINER_STARTED=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/pscat-mcp-smoke.XXXXXX")"
chmod 700 "$TMP"

pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
info() { printf '.... %s\n' "$*"; }

cleanup() {
  local rc=$?
  if [[ $CONTAINER_STARTED == 1 ]]; then
    if [[ $rc != 0 || $FAILURES != 0 ]]; then
      echo "---- docker logs (tail) ----"
      docker logs --tail 60 "$NAME" 2>&1 || true
      echo "----------------------------"
    fi
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------------------
# Python helper (standard library only, isolated mode).
# ---------------------------------------------------------------------------
PY_HELPER="$TMP/mcp_helper.py"
cat >"$PY_HELPER" <<'PY'
import base64, json, struct, sys

def load_message(path, want_id):
    """Return the JSON-RPC message with id `want_id` from a JSON or SSE body."""
    raw = open(path, "rb").read().decode("utf-8", "replace")
    candidates = []
    stripped = raw.strip()
    if stripped.startswith("{") or stripped.startswith("["):
        doc = json.loads(stripped)
        candidates = doc if isinstance(doc, list) else [doc]
    else:
        # text/event-stream: events separated by blank lines; join data lines.
        data = []
        for line in raw.replace("\r\n", "\n").split("\n"):
            if line.startswith("data:"):
                data.append(line[5:].lstrip(" "))
            elif line == "" and data:
                chunk = "\n".join(data); data = []
                try:
                    candidates.append(json.loads(chunk))
                except ValueError:
                    pass
        if data:
            try:
                candidates.append(json.loads("\n".join(data)))
            except ValueError:
                pass
    for m in candidates:
        if isinstance(m, dict) and str(m.get("id")) == str(want_id):
            return m
    raise SystemExit("no JSON-RPC message with id %s in %s" % (want_id, path))

def walk(node):
    yield node
    if isinstance(node, dict):
        for v in node.values():
            yield from walk(v)
    elif isinstance(node, list):
        for v in node:
            yield from walk(v)

def expanded(msg):
    """The message plus any JSON embedded in text content items."""
    nodes = [msg]
    for n in walk(msg):
        if isinstance(n, dict) and n.get("type") == "text" and isinstance(n.get("text"), str):
            try:
                nodes.append(json.loads(n["text"]))
            except ValueError:
                pass
    return nodes

def find_artifact(msg, mime):
    for root in expanded(msg):
        for n in walk(root):
            if not isinstance(n, dict):
                continue
            mt = n.get("mimeType") or n.get("mime_type")
            # image content: {"type":"image","data":...,"mimeType":...}
            # embedded resource: {"uri":...,"mimeType":...,"blob":...}
            for key in ("data", "blob", "data_base64"):
                if isinstance(n.get(key), str) and (mt is None or mt == mime):
                    try:
                        data = base64.b64decode(n[key], validate=True)
                    except Exception:
                        continue
                    if data:
                        return data
    return None

def cmd_build(args):
    req_id, method, params = args[0], args[1], json.loads(args[2])
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    if req_id != "-":
        msg["id"] = int(req_id)
    sys.stdout.write(json.dumps(msg))

def cmd_call(args):
    req_id, tool, source_file, extra = args[0], args[1], args[2], json.loads(args[3])
    arguments = {"source": open(source_file).read()}
    arguments.update(extra)
    msg = {"jsonrpc": "2.0", "id": int(req_id), "method": "tools/call",
           "params": {"name": tool, "arguments": arguments}}
    sys.stdout.write(json.dumps(msg))

def cmd_extract(args):
    body, req_id = args[0], args[1]
    sys.stdout.write(json.dumps(load_message(body, req_id)))

def cmd_check_init(args):
    msg = json.load(open(args[0]))
    res = msg.get("result") or {}
    if "error" in msg or "protocolVersion" not in res:
        raise SystemExit("initialize failed: %s" % json.dumps(msg)[:400])
    tools = res.get("capabilities", {}).get("tools")
    print("protocolVersion=%s server=%s tools_capability=%s" % (
        res["protocolVersion"], res.get("serverInfo", {}).get("name"), tools is not None))

def cmd_artifact(args):
    msg = json.load(open(args[0]))
    out, mime = args[1], args[2]
    if "error" in msg:
        raise SystemExit("JSON-RPC error: %s" % json.dumps(msg["error"])[:400])
    if (msg.get("result") or {}).get("isError"):
        raise SystemExit("tool error: %s" % json.dumps(msg["result"])[:600])
    data = find_artifact(msg, mime)
    if data is None:
        raise SystemExit("no %s artifact in result: %s" % (mime, json.dumps(msg)[:600]))
    open(out, "wb").write(data)
    print("%d bytes" % len(data))

def cmd_png_dims(args):
    data = open(args[0], "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit("not a PNG (signature %r)" % data[:8])
    if data[12:16] != b"IHDR":
        raise SystemExit("first chunk is %r, not IHDR" % data[12:16])
    w, h = struct.unpack(">II", data[16:24])
    print("%dx%d" % (w, h))

def cmd_error_code(args):
    msg = json.load(open(args[0]))
    known = {"invalid_request", "source_too_large", "unsupported_format",
             "invalid_dimensions", "too_many_pixels", "postscript_error",
             "limit_exceeded", "timeout", "too_many_pages", "output_too_large",
             "server_busy", "cancelled", "worker_failed"}
    is_error = "error" in msg or bool((msg.get("result") or {}).get("isError"))
    for root in expanded(msg):
        for n in walk(root):
            if isinstance(n, dict) and n.get("code") in known:
                print(("tool_error " if is_error else "no_error_flag ") + n["code"])
                return
    raise SystemExit("no structured error code in: %s" % json.dumps(msg)[:600])

def cmd_free_port(_args):
    import socket
    s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()

{"build": cmd_build, "call": cmd_call, "extract": cmd_extract,
 "check-init": cmd_check_init, "artifact": cmd_artifact,
 "png-dims": cmd_png_dims, "error-code": cmd_error_code,
 "free-port": cmd_free_port}[sys.argv[1]](sys.argv[2:])
PY
py() { python3 -I "$PY_HELPER" "$@"; }

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  info "building $IMAGE"
  if DOCKER_BUILDKIT=1 docker build -t "$IMAGE" "$ROOT"; then
    pass "image builds ($IMAGE)"
  else
    fail "docker build"
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Run (hardened)
# ---------------------------------------------------------------------------
if command -v openssl >/dev/null 2>&1; then
  TOKEN="$(openssl rand -hex 32)"
else
  TOKEN="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
fi
# Keep the token out of process listings: env file for docker, header file
# for curl, both inside the private temp dir.
umask 077
printf 'PSCAT_MCP_AUTH_TOKENS=%s\n' "$TOKEN" >"$TMP/env"
printf 'Authorization: Bearer %s\n' "$TOKEN" >"$TMP/auth-header"

PORT="${SMOKE_PORT:-$(py free-port)}"
BASE="http://127.0.0.1:$PORT"

info "starting container $NAME on $BASE"
docker run -d --name "$NAME" \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --memory 1g \
  --cpus 1 \
  --pids-limit 64 \
  --env-file "$TMP/env" \
  -e PSCAT_MCP_ALLOWED_HOSTS=localhost,127.0.0.1 \
  -e PSCAT_MCP_RENDER_TIMEOUT_MS=3000 \
  -p "127.0.0.1:$PORT:8080" \
  "$IMAGE" >/dev/null
CONTAINER_STARTED=1

hc="$(docker inspect -f '{{.HostConfig.ReadonlyRootfs}} {{.HostConfig.PidsLimit}} {{.Config.User}}' "$NAME")"
read -r ro pids user <<<"$hc"
if [[ "$ro" == true && "$pids" == 64 ]]; then
  pass "container is hardened (read-only rootfs=$ro, pids-limit=$pids, user=${user:-image default})"
else
  fail "container hardening not applied: $hc"
fi

healthz() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$BASE/healthz" || true; }

ready=0
for _ in $(seq 1 60); do
  if [[ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != true ]]; then
    fail "container exited during startup"
    exit 1
  fi
  if [[ "$(healthz)" == 200 ]]; then ready=1; break; fi
  sleep 0.5
done
if [[ $ready == 1 ]]; then pass "/healthz returns 200"; else fail "/healthz never returned 200"; exit 1; fi

# ---------------------------------------------------------------------------
# MCP over Streamable HTTP
# ---------------------------------------------------------------------------
SESSION=""
INITIALIZED=0

# mcp_post NAME [--no-auth] < body  -> prints HTTP status; body/headers in $TMP
mcp_post() {
  local name="$1"; shift
  local -a args=(-sS -o "$TMP/$name.body" -D "$TMP/$name.headers" -w '%{http_code}'
    --max-time 60 -X POST
    -H 'Content-Type: application/json'
    -H 'Accept: application/json, text/event-stream')
  if [[ "${1:-}" != --no-auth ]]; then
    args+=(-H "@$TMP/auth-header")
  fi
  if [[ $INITIALIZED == 1 ]]; then
    args+=(-H "MCP-Protocol-Version: $PROTOCOL_VERSION")
  fi
  if [[ -n "$SESSION" ]]; then
    args+=(-H "Mcp-Session-Id: $SESSION")
  fi
  curl "${args[@]}" --data-binary @- "$BASE/mcp" || echo "curl-failed"
}

INIT_PARAMS="{\"protocolVersion\":\"$PROTOCOL_VERSION\",\"capabilities\":{},\"clientInfo\":{\"name\":\"container-smoke\",\"version\":\"1.0\"}}"

# 1. Unauthenticated -> 401
status="$(py build 1 initialize "$INIT_PARAMS" | mcp_post unauth --no-auth)"
if [[ "$status" == 401 ]]; then pass "unauthenticated POST /mcp -> 401"; else fail "unauthenticated POST /mcp -> $status (want 401)"; fi

# 2. initialize
status="$(py build 1 initialize "$INIT_PARAMS" | mcp_post init)"
if [[ "$status" == 200 ]] \
  && py extract "$TMP/init.body" 1 >"$TMP/init.json" \
  && detail="$(py check-init "$TMP/init.json")"; then
  pass "initialize ($detail)"
else
  fail "initialize -> HTTP $status: $(head -c 400 "$TMP/init.body" 2>/dev/null)"
  exit 1
fi
SESSION="$(awk 'tolower($1)=="mcp-session-id:" {print $2}' "$TMP/init.headers" | tr -d '\r')"
info "session: ${SESSION:-<stateless>}"
INITIALIZED=1

status="$(py build - notifications/initialized '{}' | mcp_post initialized)"
if [[ "$status" == 202 || "$status" == 200 || "$status" == 204 ]]; then
  pass "notifications/initialized accepted ($status)"
else
  fail "notifications/initialized -> $status"
fi

# call_tool NAME ID TOOL SOURCE_FILE EXTRA_ARGS_JSON -> $TMP/NAME.json
call_tool() {
  local name="$1" id="$2" tool="$3" src="$4" extra="$5" status
  status="$(py call "$id" "$tool" "$src" "$extra" | mcp_post "$name")"
  if [[ "$status" != 200 ]]; then
    echo "HTTP $status: $(head -c 400 "$TMP/$name.body" 2>/dev/null)" >&2
    return 1
  fi
  py extract "$TMP/$name.body" "$id" >"$TMP/$name.json"
}

cat >"$TMP/shapes.ps" <<'PS'
%!PS
% 200x100 page: background, red circle, green square, blue triangle
0.95 0.95 1.0 setrgbcolor 0 0 200 100 rectfill
1 0 0 setrgbcolor newpath 40 50 30 0 360 arc closepath fill
0 0.6 0 setrgbcolor 80 20 60 60 rectfill
0 0 1 setrgbcolor newpath 150 20 moveto 190 20 lineto 170 80 lineto closepath fill
showpage
PS

render_png_check() { # label id
  local label="$1" id="$2" out dims
  if call_tool "png$id" "$id" render_postscript "$TMP/shapes.ps" '{"format":"png","width":200,"height":100}' \
    && out="$(py artifact "$TMP/png$id.json" "$TMP/out$id.png" image/png)" \
    && dims="$(py png-dims "$TMP/out$id.png")"; then
    if [[ "$dims" == 200x100 ]]; then
      pass "$label: render_postscript png -> valid PNG $dims ($out)"
    else
      fail "$label: PNG is $dims, want 200x100"
    fi
  else
    fail "$label: render_postscript png failed: $(head -c 600 "$TMP/png$id.json" 2>/dev/null)"
  fi
}

# 3. PNG render
render_png_check "render" 2

# 4. PDF render
if call_tool pdf 3 render_postscript "$TMP/shapes.ps" '{"format":"pdf","width":200,"height":100}' \
  && out="$(py artifact "$TMP/pdf.json" "$TMP/out.pdf" application/pdf)"; then
  if [[ "$(head -c 5 "$TMP/out.pdf")" == "%PDF-" ]]; then
    pass "render_postscript pdf -> starts with %PDF- ($out)"
  else
    fail "render_postscript pdf: output does not start with %PDF-"
  fi
else
  fail "render_postscript pdf failed: $(head -c 600 "$TMP/pdf.json" 2>/dev/null)"
fi

# 5. Runaway program is stopped (PSCAT_MCP_RENDER_TIMEOUT_MS=3000)
printf '%%!PS\n{ } loop\n' >"$TMP/loop.ps"
start=$(date +%s)
if call_tool loop 4 render_postscript "$TMP/loop.ps" '{}' \
  && code="$(py error-code "$TMP/loop.json")"; then
  elapsed=$(( $(date +%s) - start ))
  case "$code" in
    "tool_error timeout" | "tool_error limit_exceeded")
      pass "infinite loop stopped in ${elapsed}s with structured error ${code#tool_error }" ;;
    *) fail "infinite loop: unexpected result '$code'" ;;
  esac
else
  fail "infinite loop: no structured error: $(head -c 600 "$TMP/loop.json" 2>/dev/null)"
fi

# 6. Still healthy and still rendering
if [[ "$(healthz)" == 200 ]]; then pass "/healthz still 200 after the runaway"; else fail "/healthz after the runaway"; fi
render_png_check "render after runaway" 5

# PIDs in use (renders above ran under --pids-limit 64)
pids_now="$(docker stats --no-stream --format '{{.PIDs}}' "$NAME" 2>/dev/null || echo '?')"
info "container PIDs now: $pids_now (limit 64)"

if [[ "$(docker inspect -f '{{.State.Running}}' "$NAME")" == true ]]; then
  pass "container still running"
else
  fail "container stopped"
fi

echo
if [[ $FAILURES == 0 ]]; then
  echo "SMOKE TEST PASSED"
else
  echo "SMOKE TEST FAILED ($FAILURES failure(s))"
  exit 1
fi
