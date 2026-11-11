//! Server-layer evals (issue #40): scenario files run with the rmcp client
//! against the real server over stdio, Streamable HTTP with a bearer token,
//! and, when `PSCAT_MCP_EVAL_IMAGE` names a container image, the image.
//! No model and no network beyond loopback.
//!
//! The scenarios are data: `evals/server/scenarios/*.json`. Their format is
//! the agent-layer case format (`evals/agent/cases/`) where it fits:
//! `id`, `title`, `origin`, `transports`, `{{file:...}}` placeholders, a
//! per-call `result` with `is_error`, `error_code`, `content_types`,
//! dotted `structured` paths and `text_contains`, and the same matcher
//! operators (`in`, `equals`, `equals_text`, `contains`, `not_contains`,
//! `min`, `max`, `regex`, `absent`). The server layer adds `png` (size and
//! pixel samples), `resource` (an SVG or PDF signature), `elapsed_ms` and
//! `renders_executed`, and for async jobs two step keys: `save` (a map of
//! names to dotted `structuredContent` paths, kept for the scenario's later
//! steps as `{{saved:NAME}}`) and `poll_ms` (repeat the call until its
//! `result` matches, for at most that long). See `docs/LOCAL_TESTING.md`,
//! "Server-layer evals".
//!
//! One test per target runs every scenario for that target, groups scenarios
//! that need the same `server_env` (PSCAT_MCP_* variables, so stdio, HTTP and
//! the container are configured the same way) onto one server, prints a
//! table, and writes `target/eval/<target>.{json,xml}` (JSON and JUnit;
//! `PSCAT_EVAL_REPORT_DIR` overrides the directory).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};

type Client = RunningService<RoleClient, ()>;

const CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// The log line the server writes for every job it hands to a worker.
const RENDER_MARK: &str = "render started";
const KNOWN_TRANSPORTS: [&str; 3] = ["stdio", "http", "container"];
/// Keys a step may have.
const STEP_KEYS: &[&str] = &[
    "label", "tool", "args", "result", "parallel", "outcomes", "delay_ms", "save", "poll_ms",
];
/// Keys a step's `result` may have. Unknown keys are an error, so a typo
/// cannot silently turn an assertion off.
const RESULT_KEYS: &[&str] = &[
    "is_error",
    "error_code",
    "content_types",
    "forbid_content_types",
    "structured",
    "text_contains",
    "png",
    "resource",
    "elapsed_ms",
    "renders_executed",
];

// ---------------------------------------------------------------------------
// Scenario files

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    title: String,
    #[serde(default)]
    origin: String,
    /// Targets this scenario runs on; all of them when omitted.
    #[serde(default)]
    transports: Option<Vec<String>>,
    /// `PSCAT_MCP_*` variables for the server this scenario runs on.
    #[serde(default)]
    server_env: BTreeMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    steps: Vec<Value>,
}

impl Scenario {
    fn runs_on(&self, target: &str) -> bool {
        self.transports
            .as_ref()
            .is_none_or(|t| t.iter().any(|x| x == target))
    }
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_scenarios() -> Result<Vec<Scenario>, String> {
    let dir = repo().join("evals/server/scenarios");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut out: Vec<Scenario> = Vec::new();
    for p in paths {
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let sc: Scenario =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
        validate_scenario(&sc).map_err(|e| format!("{}: {e}", p.display()))?;
        if out.iter().any(|o| o.id == sc.id) {
            return Err(format!(
                "{}: duplicate scenario id {:?}",
                p.display(),
                sc.id
            ));
        }
        out.push(sc);
    }
    if out.is_empty() {
        return Err(format!("no scenarios in {}", dir.display()));
    }
    Ok(out)
}

fn validate_scenario(sc: &Scenario) -> Result<(), String> {
    if let Some(ts) = &sc.transports {
        for t in ts {
            if !KNOWN_TRANSPORTS.contains(&t.as_str()) {
                return Err(format!("unknown transport {t:?}"));
            }
        }
    }
    for (k, v) in &sc.server_env {
        if k == "PSCAT_MCP_CAPTURE_FILE" {
            return Err("server_env must never enable capture mode".into());
        }
        if !k.starts_with("PSCAT_MCP_") || v.is_empty() {
            return Err(format!("server_env {k:?}: only PSCAT_MCP_* with a value"));
        }
    }
    if sc.steps.is_empty() {
        return Err("no steps".into());
    }
    for (i, step) in sc.steps.iter().enumerate() {
        let obj = step
            .as_object()
            .ok_or(format!("step {i} is not an object"))?;
        for k in obj.keys() {
            if !STEP_KEYS.contains(&k.as_str()) {
                return Err(format!("step {i}: unknown key {k:?}"));
            }
        }
        let calls: Vec<&Value> = match obj.get("parallel") {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(_) => return Err(format!("step {i}: parallel must be an array")),
            None => vec![step],
        };
        if obj.contains_key("parallel") != obj.contains_key("outcomes") {
            return Err(format!("step {i}: parallel and outcomes go together"));
        }
        if obj.contains_key("parallel") && (obj.contains_key("save") || obj.contains_key("poll_ms"))
        {
            return Err(format!("step {i}: save and poll_ms are for a single call"));
        }
        if let Some(save) = obj.get("save")
            && !save
                .as_object()
                .is_some_and(|m| m.values().all(Value::is_string))
        {
            return Err(format!("step {i}: save maps names to structured paths"));
        }
        if obj.get("poll_ms").is_some_and(|v| v.as_u64().is_none()) {
            return Err(format!("step {i}: poll_ms is a number of milliseconds"));
        }
        for c in calls {
            if c.get("tool").and_then(Value::as_str).is_none() {
                return Err(format!("step {i}: a call needs a tool"));
            }
            // A parallel call only has these; its result is the group's.
            if obj.contains_key("parallel")
                && let Some(co) = c.as_object()
                && let Some(k) = co
                    .keys()
                    .find(|k| !["tool", "args", "delay_ms"].contains(&k.as_str()))
            {
                return Err(format!("step {i}: unknown key {k:?} in a parallel call"));
            }
        }
        if let Some(Value::Object(r)) = obj.get("result") {
            for k in r.keys() {
                if !RESULT_KEYS.contains(&k.as_str()) {
                    return Err(format!("step {i}: unknown result key {k:?}"));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Placeholders and matchers (the same vocabulary as evals/agent/transcript.py)

fn lock_field(key: &str) -> String {
    let lock = include_str!("../Cargo.lock");
    let entry = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"pscat\"\n"))
        .expect("pscat in Cargo.lock");
    let value = entry
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key} = \"")))
        .map(|v| v.trim_end_matches('"').to_string())
        .expect("field in Cargo.lock");
    if key == "source" {
        return value.rsplit('#').next().unwrap_or("").to_string();
    }
    value
}

fn placeholder(key: &str) -> Result<String, String> {
    match key {
        "repo" => Ok(repo().display().to_string()),
        "server_version" => Ok(env!("CARGO_PKG_VERSION").to_string()),
        "pscat_version" => Ok(lock_field("version")),
        "pscat_revision" => Ok(lock_field("source")),
        // About 285 KB, over the 256 KiB default source limit.
        "huge_source" => Ok("0 0 moveto 1 1 lineto stroke\n".repeat(9500)),
        // A `handwrite` text of about 100 KB, over the 64 KiB default limit.
        "huge_note" => Ok("word ".repeat(20_000)),
        // A `handwrite` text of `n` short lines.
        k if k.starts_with("lines:") => {
            let n: usize = k[6..].parse().map_err(|_| format!("bad {{{{{k}}}}}"))?;
            Ok("line\n".repeat(n))
        }
        // A multi-page program: `n` blank pages.
        k if k.starts_with("pages:") => {
            let n: usize = k[6..].parse().map_err(|_| format!("bad {{{{{k}}}}}"))?;
            Ok("showpage\n".repeat(n))
        }
        // Filled in from an earlier step (`save`) before expansion; left as
        // it is here.
        k if k.starts_with("saved:") => Ok(format!("{{{{{k}}}}}")),
        k if k.starts_with("file:") => {
            let path = repo().join(&k[5..]);
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
        }
        _ => Err(format!("unknown placeholder {{{{{key}}}}}")),
    }
}

/// Replace `{{saved:NAME}}` with what an earlier step saved.
fn fill_saved(v: &Value, saved: &BTreeMap<String, String>) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            while let Some(i) = out.find("{{saved:") {
                let Some(j) = out[i..].find("}}") else {
                    break;
                };
                let name = out[i + 8..i + j].trim().to_string();
                let value = saved
                    .get(&name)
                    .ok_or_else(|| format!("nothing saved as {name:?}"))?;
                out.replace_range(i..i + j + 2, value);
            }
            Ok(Value::String(out))
        }
        Value::Array(a) => Ok(Value::Array(
            a.iter()
                .map(|v| fill_saved(v, saved))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(o) => Ok(Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), fill_saved(v, saved)?)))
                .collect::<Result<_, String>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn expand(v: &Value) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find("{{") {
                let Some(j) = rest[i..].find("}}") else {
                    break;
                };
                out.push_str(&rest[..i]);
                out.push_str(&placeholder(rest[i + 2..i + j].trim())?);
                rest = &rest[i + j + 2..];
            }
            out.push_str(rest);
            Ok(Value::String(out))
        }
        Value::Array(a) => Ok(Value::Array(
            a.iter().map(expand).collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(o) => Ok(Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), expand(v)?)))
                .collect::<Result<_, String>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() <= 70 {
        s
    } else {
        format!("{}...", s.chars().take(67).collect::<String>())
    }
}

fn num_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn normalize_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// A literal spec means equality; an object holds operators that must all
/// hold. `value` is `None` when the field is absent.
fn match_value(spec: &Value, value: Option<&Value>) -> Result<(), String> {
    let Value::Object(ops) = spec else {
        return match value {
            None => Err(format!("absent, want {}", short(spec))),
            Some(v) if num_eq(v, spec) => Ok(()),
            Some(v) => Err(format!("{} != {}", short(v), short(spec))),
        };
    };
    for (op, want) in ops {
        if op == "absent" {
            let absent = want.as_bool().unwrap_or(true);
            if value.is_none() != absent {
                return Err(format!(
                    "{}, want absent={absent}",
                    if value.is_none() { "absent" } else { "present" }
                ));
            }
            continue;
        }
        if op == "in" {
            let list = want.as_array().ok_or("`in` wants a list")?;
            let null = Value::Null;
            let v = value.unwrap_or(&null);
            if !list.iter().any(|x| num_eq(x, v)) {
                return Err(format!("{} not in {}", short(v), short(want)));
            }
            continue;
        }
        let Some(v) = value else {
            return Err(format!("absent (want {op} {})", short(want)));
        };
        let strings = |w: &Value| -> Vec<String> {
            match w {
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                Value::String(s) => vec![s.clone()],
                _ => vec![],
            }
        };
        match op.as_str() {
            "equals" => {
                if !num_eq(v, want) {
                    return Err(format!("{} != {}", short(v), short(want)));
                }
            }
            "equals_text" => {
                let (Some(a), Some(b)) = (v.as_str(), want.as_str()) else {
                    return Err("equals_text wants strings".into());
                };
                if normalize_text(a) != normalize_text(b) {
                    return Err("text differs from expected".into());
                }
            }
            "contains" => {
                let hay = v.as_str().ok_or(format!("{} is not text", short(v)))?;
                let missing: Vec<String> = strings(want)
                    .into_iter()
                    .filter(|w| !hay.contains(w.as_str()))
                    .collect();
                if !missing.is_empty() {
                    return Err(format!("does not contain {missing:?}"));
                }
            }
            "not_contains" => {
                let hay = v.as_str().unwrap_or("");
                let found: Vec<String> = strings(want)
                    .into_iter()
                    .filter(|w| hay.contains(w.as_str()))
                    .collect();
                if !found.is_empty() {
                    return Err(format!("contains {found:?}"));
                }
            }
            "min" | "max" => {
                let (Some(x), Some(bound)) = (v.as_f64(), want.as_f64()) else {
                    return Err(format!("{} is not a number", short(v)));
                };
                if (op == "min" && x < bound) || (op == "max" && x > bound) {
                    return Err(format!("{x} violates {op} {bound}"));
                }
            }
            "regex" => {
                let re = want.as_str().ok_or("regex wants a string")?;
                let hay = v.as_str().ok_or(format!("{} is not text", short(v)))?;
                if !simple_regex_match(re, hay) {
                    return Err(format!("no match for /{re}/"));
                }
            }
            other => return Err(format!("unknown matcher {other:?}")),
        }
    }
    Ok(())
}

/// `^`, `$`, `.`, `\d`, `[a-z]` classes and `+`/`*`/`?` are all the
/// scenarios need; keeping this tiny avoids a regex dependency. Unanchored
/// patterns match anywhere.
fn simple_regex_match(re: &str, hay: &str) -> bool {
    #[derive(Clone)]
    enum Atom {
        Any,
        Digit,
        Lit(char),
        Class(Vec<(char, char)>),
    }
    let chars: Vec<char> = re.chars().collect();
    let mut i = 0;
    let anchored_start = chars.first() == Some(&'^');
    if anchored_start {
        i = 1;
    }
    let mut items: Vec<(Atom, char)> = Vec::new(); // atom and quantifier ('1', '?', '*', '+')
    let mut anchored_end = false;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        let atom = match c {
            '$' if i == chars.len() => {
                anchored_end = true;
                break;
            }
            '.' => Atom::Any,
            '\\' => {
                let e = *chars.get(i).unwrap_or(&'\\');
                i += 1;
                if e == 'd' { Atom::Digit } else { Atom::Lit(e) }
            }
            '[' => {
                let mut ranges = Vec::new();
                while i < chars.len() && chars[i] != ']' {
                    if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|c| *c != ']')
                    {
                        ranges.push((chars[i], chars[i + 2]));
                        i += 3;
                    } else {
                        ranges.push((chars[i], chars[i]));
                        i += 1;
                    }
                }
                i += 1;
                Atom::Class(ranges)
            }
            other => Atom::Lit(other),
        };
        let q = match chars.get(i) {
            Some(q @ ('?' | '*' | '+')) => {
                i += 1;
                *q
            }
            _ => '1',
        };
        items.push((atom, q));
    }
    fn one(a: &Atom, c: char) -> bool {
        match a {
            Atom::Any => true,
            Atom::Digit => c.is_ascii_digit(),
            Atom::Lit(l) => *l == c,
            Atom::Class(r) => r.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)),
        }
    }
    fn go(items: &[(Atom, char)], s: &[char], end: bool) -> bool {
        let Some(((atom, q), rest)) = items.split_first() else {
            return !end || s.is_empty();
        };
        let (min, max) = match q {
            '?' => (0, 1),
            '*' => (0, usize::MAX),
            '+' => (1, usize::MAX),
            _ => (1, 1),
        };
        let mut n = 0;
        while n < s.len() && n < max && one(atom, s[n]) {
            n += 1;
        }
        loop {
            if n >= min && go(rest, &s[n..], end) {
                return true;
            }
            if n == 0 || n <= min {
                return false;
            }
            n -= 1;
        }
    }
    let hay: Vec<char> = hay.chars().collect();
    if anchored_start {
        return go(&items, &hay, anchored_end);
    }
    (0..=hay.len()).any(|s| go(&items, &hay[s..], anchored_end))
}

fn dig<'a>(mut v: &'a Value, path: &str) -> Option<&'a Value> {
    for part in path.split('.') {
        v = match v {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

// ---------------------------------------------------------------------------
// Checking one tool result

/// `text`, `image:<mime>` or `resource:<mime>` for each content block.
fn block_types(result: &Value) -> Vec<String> {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str().unwrap_or("?") {
            "image" => format!("image:{}", b["mimeType"].as_str().unwrap_or("")),
            "resource" => format!(
                "resource:{}",
                b["resource"]["mimeType"].as_str().unwrap_or("")
            ),
            other => other.to_string(),
        })
        .collect()
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_code(result: &Value) -> Option<String> {
    result["structuredContent"]["error"]["code"]
        .as_str()
        .map(str::to_string)
}

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}

/// Width, height and RGBA of a decoded PNG.
fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<[u8; 4]>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("not a PNG: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG frame: {e}"))?;
    let ch = info.color_type.samples();
    let px = buf[..info.buffer_size()]
        .chunks(ch)
        .map(|c| match ch {
            4 => [c[0], c[1], c[2], c[3]],
            3 => [c[0], c[1], c[2], 255],
            2 => [c[0], c[0], c[0], c[1]],
            _ => [c[0], c[0], c[0], 255],
        })
        .collect();
    Ok((info.width, info.height, px))
}

fn check_png(spec: &Value, result: &Value, problems: &mut Vec<String>) {
    let Some(block) = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["type"] == "image" && b["mimeType"] == "image/png")
    else {
        problems.push("png: no image/png block".into());
        return;
    };
    let bytes = b64(block["data"].as_str().unwrap_or(""));
    if bytes.get(..8) != Some(&b"\x89PNG\r\n\x1a\n"[..]) {
        problems.push("png: bad signature".into());
        return;
    }
    let (w, h, px) = match decode_png(&bytes) {
        Ok(d) => d,
        Err(e) => {
            problems.push(format!("png: {e}"));
            return;
        }
    };
    for (key, got) in [("width", w), ("height", h)] {
        if let Some(want) = spec.get(key).and_then(Value::as_u64)
            && u64::from(got) != want
        {
            problems.push(format!("png {key} {got}, want {want}"));
        }
    }
    for p in spec["pixels"].as_array().into_iter().flatten() {
        let (x, y) = (
            p["x"].as_u64().unwrap_or(0) as u32,
            p["y"].as_u64().unwrap_or(0) as u32,
        );
        let tol = p["tolerance"].as_i64().unwrap_or(10);
        let Some(got) = px.get((y as usize) * (w as usize) + x as usize) else {
            problems.push(format!("png pixel ({x},{y}) is outside {w}x{h}"));
            continue;
        };
        let want: Vec<i64> = p["rgb"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if want.len() != 3 || (0..3).any(|i| (i64::from(got[i]) - want[i]).abs() > tol) {
            problems.push(format!(
                "png pixel ({x},{y}) is rgb({},{},{}), want rgb{want:?} +-{tol}",
                got[0], got[1], got[2]
            ));
        }
    }
}

fn check_resource(spec: &Value, result: &Value, problems: &mut Vec<String>) {
    let mime = spec["mime"].as_str().unwrap_or("");
    let Some(res) = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["type"] == "resource" && b["resource"]["mimeType"] == mime)
        .map(|b| &b["resource"])
    else {
        problems.push(format!("resource: no {mime} block"));
        return;
    };
    let bytes = match (res["blob"].as_str(), res["text"].as_str()) {
        (Some(b), _) => b64(b),
        (None, Some(t)) => t.as_bytes().to_vec(),
        _ => {
            problems.push("resource: neither blob nor text".into());
            return;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let starts: Vec<&str> = match &spec["starts_with"] {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !starts.is_empty() && !starts.iter().any(|s| text.starts_with(s)) {
        problems.push(format!(
            "{mime} starts with {:?}, want one of {starts:?}",
            text.chars().take(12).collect::<String>()
        ));
    }
    for needle in spec["contains"].as_array().into_iter().flatten() {
        if let Some(n) = needle.as_str()
            && !text.contains(n)
        {
            problems.push(format!("{mime} does not contain {n:?}"));
        }
    }
    if let Some(n) = spec["min_bytes"].as_u64()
        && (bytes.len() as u64) < n
    {
        problems.push(format!(
            "{mime} is {} bytes, want at least {n}",
            bytes.len()
        ));
    }
}

/// Everything about one call's outcome that a `result` spec can assert on.
struct Observed<'a> {
    result: &'a Value,
    elapsed_ms: u128,
    /// `None` when the target cannot count renders.
    renders: Option<usize>,
}

fn check_result(spec: &Value, obs: &Observed) -> Vec<String> {
    let mut problems = Vec::new();
    let res = obs.result;
    let is_error = res["isError"].as_bool().unwrap_or(false);
    let want_error = spec["is_error"].as_bool().unwrap_or(false);
    if is_error != want_error {
        problems.push(format!(
            "is_error={is_error}, want {want_error}: {}",
            result_text(res).chars().take(160).collect::<String>()
        ));
    }
    if let Some(s) = spec.get("error_code")
        && let Err(e) = match_value(s, error_code(res).map(Value::String).as_ref())
    {
        problems.push(format!("error code: {e}"));
    }
    let types = block_types(res);
    for want in spec["content_types"].as_array().into_iter().flatten() {
        let want = want.as_str().unwrap_or("");
        if !types.iter().any(|t| t.starts_with(want)) {
            problems.push(format!("no {want} block (got {types:?})"));
        }
    }
    for bad in spec["forbid_content_types"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let bad = bad.as_str().unwrap_or("");
        if types.iter().any(|t| t.starts_with(bad)) {
            problems.push(format!("unexpected {bad} block (got {types:?})"));
        }
    }
    for (path, s) in spec["structured"].as_object().into_iter().flatten() {
        if let Err(e) = match_value(s, dig(&res["structuredContent"], path)) {
            problems.push(format!("structured {path}: {e}"));
        }
    }
    let text = result_text(res).to_lowercase();
    for needle in spec["text_contains"].as_array().into_iter().flatten() {
        let n = needle.as_str().unwrap_or("");
        if !text.contains(&n.to_lowercase()) {
            problems.push(format!("result text lacks {n:?}"));
        }
    }
    if let Some(p) = spec.get("png") {
        check_png(p, res, &mut problems);
    }
    if let Some(r) = spec.get("resource") {
        check_resource(r, res, &mut problems);
    }
    if let Some(s) = spec.get("elapsed_ms")
        && let Err(e) = match_value(s, Some(&json!(obs.elapsed_ms as u64)))
    {
        problems.push(format!("elapsed_ms: {e}"));
    }
    if let (Some(want), Some(got)) = (spec.get("renders_executed"), obs.renders)
        && let Err(e) = match_value(want, Some(&json!(got)))
    {
        problems.push(format!("renders_executed: {e}"));
    }
    problems
}

// ---------------------------------------------------------------------------
// Servers under test

struct Server {
    client: Client,
    /// Jobs the server handed to a worker, counted from its log.
    renders: Arc<AtomicUsize>,
    /// Kills the process or container when dropped.
    _cleanup: Box<dyn Send>,
}

struct RmContainer(String);
impl Drop for RmContainer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn server_binary() -> PathBuf {
    env!("CARGO_BIN_EXE_pscat-mcp-server").into()
}

/// Count server log lines that mark a job start; hand the first `"addr"`
/// to `addr_tx` when given.
fn spawn_log_reader<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    renders: Arc<AtomicUsize>,
    addr_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains(RENDER_MARK) {
                renders.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(tx) = &addr_tx
                && let Some(i) = line.find("\"addr\":\"")
            {
                let rest = &line[i + 8..];
                if let Some(j) = rest.find('"') {
                    let _ = tx.send(rest[..j].to_string());
                }
            }
        }
    });
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// A command with every PSCAT_MCP_* variable of the parent removed, so a
/// developer's environment cannot change a scenario.
fn clean_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    for (k, _) in std::env::vars() {
        if k.starts_with("PSCAT_MCP_") || k == "PORT" {
            cmd.env_remove(k);
        }
    }
    cmd
}

async fn connect_http(url: &str, token: &str) -> Result<Client, String> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_string()).auth_header(token),
    );
    tokio::time::timeout(CALL_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| "initialize timed out".to_string())?
        .map_err(|e| format!("initialize: {e}"))
}

async fn wait_healthy(base: &str) -> Result<(), String> {
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(r) = http.get(format!("{base}/healthz")).send().await
            && r.status().is_success()
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("{base}/healthz never answered"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn start_stdio(env: &BTreeMap<String, String>) -> Result<Server, String> {
    let renders = Arc::new(AtomicUsize::new(0));
    let mut cmd = clean_command(server_binary());
    cmd.arg("serve-stdio")
        .env("PSCAT_MCP_LOG_FORMAT", "pretty")
        .env("RUST_LOG", "info");
    cmd.envs(env);
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    spawn_log_reader(stderr.ok_or("no stderr")?, renders.clone(), None);
    let client = tokio::time::timeout(CALL_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| "initialize timed out".to_string())?
        .map_err(|e| format!("initialize: {e}"))?;
    Ok(Server {
        client,
        renders,
        _cleanup: Box::new(()),
    })
}

async fn start_http(env: &BTreeMap<String, String>) -> Result<Server, String> {
    let renders = Arc::new(AtomicUsize::new(0));
    let token = random_token();
    let mut cmd = clean_command(server_binary());
    cmd.args(["serve-http", "--bind", "127.0.0.1", "--port", "0"])
        .env("PSCAT_MCP_AUTH_TOKENS", &token)
        .env("PSCAT_MCP_LOG_FORMAT", "json")
        .env("RUST_LOG", "info")
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_log_reader(
        child.stderr.take().ok_or("no stderr")?,
        renders.clone(),
        Some(tx),
    );
    let addr = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .map_err(|_| "serve-http did not report its address".to_string())?
        .ok_or("serve-http exited before it listened")?;
    let base = format!("http://{addr}");
    wait_healthy(&base).await?;
    let client = connect_http(&format!("{base}/mcp"), &token).await?;
    Ok(Server {
        client,
        renders,
        _cleanup: Box::new(child),
    })
}

async fn start_container(image: &str, env: &BTreeMap<String, String>) -> Result<Server, String> {
    let renders = Arc::new(AtomicUsize::new(0));
    let token = random_token();
    let name = format!("pscat-mcp-eval-{}", uuid::Uuid::new_v4().simple());
    // The token goes in a private env file, not on a command line.
    let env_file = std::env::temp_dir().join(format!("{name}.env"));
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut f = options
            .open(&env_file)
            .map_err(|e| format!("env file: {e}"))?;
        writeln!(f, "PSCAT_MCP_AUTH_TOKENS={token}").map_err(|e| e.to_string())?;
    }
    let mut args: Vec<String> = [
        "run",
        "-d",
        "--name",
        &name,
        // The hardening scripts/container-smoke.sh checks.
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--memory",
        "1g",
        "--cpus",
        "1",
        "--pids-limit",
        "64",
        "--env-file",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(env_file.display().to_string());
    args.extend(
        ["-e", "PSCAT_MCP_ALLOWED_HOSTS=localhost,127.0.0.1"]
            .iter()
            .map(|s| s.to_string()),
    );
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.extend(["-p".into(), "127.0.0.1::8080".into(), image.to_string()]);
    let run = tokio::process::Command::new("docker")
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("docker run: {e}"))?;
    let _ = std::fs::remove_file(&env_file);
    if !run.status.success() {
        return Err(format!(
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    let cleanup = RmContainer(name.clone());
    let port = tokio::process::Command::new("docker")
        .args(["port", &name, "8080/tcp"])
        .output()
        .await
        .map_err(|e| format!("docker port: {e}"))?;
    let port = String::from_utf8_lossy(&port.stdout)
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .map(str::to_string)
        .ok_or("no published port")?;

    let mut logs = tokio::process::Command::new("docker")
        .args(["logs", "-f", &name])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("docker logs: {e}"))?;
    spawn_log_reader(
        logs.stdout.take().ok_or("no stdout")?,
        renders.clone(),
        None,
    );
    spawn_log_reader(
        logs.stderr.take().ok_or("no stderr")?,
        renders.clone(),
        None,
    );

    let base = format!("http://127.0.0.1:{port}");
    wait_healthy(&base).await?;
    let client = connect_http(&format!("{base}/mcp"), &token).await?;
    Ok(Server {
        client,
        renders,
        _cleanup: Box::new((cleanup, logs)),
    })
}

// ---------------------------------------------------------------------------
// Running scenarios

#[derive(Debug, Clone)]
struct StepRow {
    scenario: String,
    step: String,
    ok: bool,
    elapsed_ms: u128,
    detail: String,
}

async fn call_tool(client: &Client, tool: &str, args: &Value) -> Result<Value, String> {
    let mut params = CallToolRequestParams::new(tool.to_string());
    if let Value::Object(m) = args {
        params = params.with_arguments(m.clone());
    }
    let res = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .map_err(|_| format!("{tool} timed out after {CALL_TIMEOUT:?}"))?
        .map_err(|e| format!("{tool} failed at the protocol level: {e}"))?;
    serde_json::to_value(&res).map_err(|e| e.to_string())
}

/// Wait for the server's log to show `want` jobs since `base` (it is written
/// before the reply but read asynchronously), then a moment longer to catch
/// extra ones.
async fn renders_since(server: &Server, base: usize, want: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.renders.load(Ordering::SeqCst) < base + want && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.renders.load(Ordering::SeqCst) - base
}

/// Wait until the render count has stopped changing. The log is read
/// asynchronously (through `docker logs -f` for a container), so a render
/// from an earlier step can still be on its way and must not be counted in
/// the next step's window.
async fn settle_render_count(server: &Server) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = server.renders.load(Ordering::SeqCst);
    let mut since = Instant::now();
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = server.renders.load(Ordering::SeqCst);
        if now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= Duration::from_millis(250) {
            return;
        }
    }
}

async fn run_step(
    server: &Server,
    step: &Value,
    saved: &mut BTreeMap<String, String>,
) -> Result<(u128, Vec<String>), String> {
    let spec = expand(&fill_saved(
        step.get("result").unwrap_or(&json!({})),
        saved,
    )?)?;
    if spec.get("renders_executed").is_some() {
        settle_render_count(server).await;
    }
    let base = server.renders.load(Ordering::SeqCst);
    let started = Instant::now();
    let mut problems = Vec::new();

    if let Some(Value::Array(calls)) = step.get("parallel") {
        let mut set = tokio::task::JoinSet::new();
        for c in calls {
            let peer = server.client.peer().clone();
            let tool = c["tool"].as_str().unwrap_or("").to_string();
            let args = expand(c.get("args").unwrap_or(&json!({})))?;
            let delay = c["delay_ms"].as_u64().unwrap_or(0);
            set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let mut params = CallToolRequestParams::new(tool);
                if let Value::Object(m) = args {
                    params = params.with_arguments(m);
                }
                tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params)).await
            });
        }
        let mut outcomes: Vec<String> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined.map_err(|e| e.to_string())? {
                Ok(Ok(r)) => {
                    let v = serde_json::to_value(&r).map_err(|e| e.to_string())?;
                    outcomes.push(if v["isError"].as_bool().unwrap_or(false) {
                        error_code(&v).unwrap_or_else(|| "error".into())
                    } else {
                        "ok".into()
                    });
                }
                Ok(Err(e)) => outcomes.push(format!("protocol error: {e}")),
                Err(_) => outcomes.push("timed out".into()),
            }
        }
        outcomes.sort();
        let mut want: Vec<String> = step["outcomes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        want.sort();
        if outcomes != want {
            problems.push(format!("outcomes {outcomes:?}, want {want:?}"));
        }
        let elapsed = started.elapsed().as_millis();
        let renders = if spec.get("renders_executed").is_some() {
            let want = spec["renders_executed"].as_u64().unwrap_or(0) as usize;
            Some(renders_since(server, base, want).await)
        } else {
            None
        };
        // Only the group-level keys apply to a parallel step.
        let mut group = serde_json::Map::new();
        for key in ["elapsed_ms", "renders_executed"] {
            if let Some(v) = spec.get(key) {
                group.insert(key.into(), v.clone());
            }
        }
        problems.extend(check_result(
            &Value::Object(group),
            &Observed {
                result: &json!({}),
                elapsed_ms: elapsed,
                renders,
            },
        ));
        return Ok((elapsed, problems));
    }

    let tool = step["tool"].as_str().unwrap_or("");
    let args = expand(&fill_saved(step.get("args").unwrap_or(&json!({})), saved)?)?;
    let poll = step["poll_ms"].as_u64().map(Duration::from_millis);
    let (result, elapsed, step_problems) = loop {
        let result = call_tool(&server.client, tool, &args).await?;
        let elapsed = started.elapsed().as_millis();
        let renders = if let Some(want) = spec.get("renders_executed") {
            let want = want.as_u64().unwrap_or(0) as usize;
            Some(renders_since(server, base, want).await)
        } else {
            None
        };
        let found = check_result(
            &spec,
            &Observed {
                result: &result,
                elapsed_ms: elapsed,
                renders,
            },
        );
        match poll {
            Some(limit) if !found.is_empty() && started.elapsed() < limit => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => break (result, elapsed, found),
        }
    };
    problems.extend(step_problems);
    if let Some(Value::Object(save)) = step.get("save") {
        for (name, path) in save {
            let path = path.as_str().unwrap_or("");
            match dig(&result["structuredContent"], path) {
                Some(Value::String(v)) => {
                    saved.insert(name.clone(), v.clone());
                }
                Some(v) => {
                    saved.insert(name.clone(), v.to_string());
                }
                None => problems.push(format!("nothing at structured.{path} to save")),
            }
        }
    }
    Ok((elapsed, problems))
}

async fn run_scenario(server: &Server, sc: &Scenario, rows: &mut Vec<StepRow>) {
    let mut saved = BTreeMap::new();
    for (i, step) in sc.steps.iter().enumerate() {
        let label = step["label"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("step {}", i + 1));
        let (ok, elapsed_ms, detail) = match run_step(server, step, &mut saved).await {
            Ok((ms, problems)) => (problems.is_empty(), ms, problems.join("; ")),
            Err(e) => (false, 0, e),
        };
        rows.push(StepRow {
            scenario: sc.id.clone(),
            step: label,
            ok,
            elapsed_ms,
            detail,
        });
        if !ok {
            break; // later steps depend on earlier ones
        }
    }
}

async fn run_target(target: &str) -> Vec<StepRow> {
    let scenarios = load_scenarios().expect("scenario files");
    let image = std::env::var("PSCAT_MCP_EVAL_IMAGE").unwrap_or_default();

    // Scenarios that share a server_env share a server.
    let mut groups: Vec<(BTreeMap<String, String>, Vec<&Scenario>)> = Vec::new();
    for sc in scenarios.iter().filter(|s| s.runs_on(target)) {
        match groups.iter_mut().find(|(e, _)| *e == sc.server_env) {
            Some((_, g)) => g.push(sc),
            None => groups.push((sc.server_env.clone(), vec![sc])),
        }
    }

    let mut rows = Vec::new();
    for (env, group) in groups {
        let started = match target {
            "stdio" => start_stdio(&env).await,
            "http" => start_http(&env).await,
            _ => start_container(&image, &env).await,
        };
        match started {
            Ok(server) => {
                for sc in group {
                    run_scenario(&server, sc, &mut rows).await;
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), server.client.cancel()).await;
            }
            Err(e) => {
                for sc in group {
                    rows.push(StepRow {
                        scenario: sc.id.clone(),
                        step: "start the server".into(),
                        ok: false,
                        elapsed_ms: 0,
                        detail: format!("{env:?}: {e}"),
                    });
                }
            }
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Reports

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn report(target: &str, rows: &[StepRow]) -> String {
    let mut scenarios: Vec<&str> = Vec::new();
    for r in rows {
        if !scenarios.contains(&r.scenario.as_str()) {
            scenarios.push(&r.scenario);
        }
    }
    let status = |id: &str| rows.iter().filter(|r| r.scenario == id).all(|r| r.ok);

    let mut table = format!("server evals over {target}\n");
    table.push_str(&format!(
        "{:<24} {:<38} {:<5} {:>7}  detail\n",
        "scenario", "step", "", "ms"
    ));
    for r in rows {
        table.push_str(&format!(
            "{:<24} {:<38} {:<5} {:>7}  {}\n",
            r.scenario,
            r.step.chars().take(38).collect::<String>(),
            if r.ok { "pass" } else { "FAIL" },
            r.elapsed_ms,
            r.detail
        ));
    }
    let failed = scenarios.iter().filter(|s| !status(s)).count();
    table.push_str(&format!(
        "{} scenario(s), {} step(s), {} failed scenario(s)\n",
        scenarios.len(),
        rows.len(),
        failed
    ));

    // JSON and JUnit for CI.
    let dir = std::env::var("PSCAT_EVAL_REPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo().join("target/eval"));
    let _ = std::fs::create_dir_all(&dir);
    let json_rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({"target": target, "scenario": r.scenario, "step": r.step,
                   "pass": r.ok, "elapsed_ms": r.elapsed_ms as u64, "detail": r.detail})
        })
        .collect();
    let _ = std::fs::write(
        dir.join(format!("{target}.json")),
        serde_json::to_string_pretty(&json!({"target": target, "steps": json_rows}))
            .unwrap_or_default(),
    );
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"server-evals-{target}\" tests=\"{}\" failures=\"{failed}\">\n",
        scenarios.len()
    );
    for id in &scenarios {
        let mine: Vec<&StepRow> = rows.iter().filter(|r| r.scenario == *id).collect();
        let ms: u128 = mine.iter().map(|r| r.elapsed_ms).sum();
        xml.push_str(&format!(
            "  <testcase classname=\"server-evals.{target}\" name=\"{}\" time=\"{:.3}\"",
            xml_escape(id),
            ms as f64 / 1000.0
        ));
        match mine.iter().find(|r| !r.ok) {
            None => xml.push_str("/>\n"),
            Some(bad) => xml.push_str(&format!(
                ">\n    <failure message=\"{}\">{}</failure>\n  </testcase>\n",
                xml_escape(&format!("{}: {}", bad.step, bad.detail)),
                xml_escape(&bad.detail)
            )),
        }
    }
    xml.push_str("</testsuite>\n");
    let _ = std::fs::write(dir.join(format!("{target}.xml")), xml);

    // GitHub Actions job summary.
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path)
    {
        // One write, so the stdio and HTTP tables cannot interleave.
        use std::io::Write as _;
        let mut md = format!(
            "### Server evals over {target}\n\n| scenario | step | result | ms |\n|---|---|---|---|\n"
        );
        for r in rows {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                r.scenario,
                r.step,
                if r.ok { "pass" } else { "**FAIL**" },
                r.elapsed_ms
            ));
        }
        md.push('\n');
        let _ = f.write_all(md.as_bytes());
    }
    table
}

async fn run_and_report(target: &str) {
    let started = Instant::now();
    let rows = run_target(target).await;
    let table = report(target, &rows);
    println!(
        "{table}({target} took {:.1}s)",
        started.elapsed().as_secs_f64()
    );
    let failures: Vec<String> = rows
        .iter()
        .filter(|r| !r.ok)
        .map(|r| format!("{} / {}: {}", r.scenario, r.step, r.detail))
        .collect();
    assert!(
        failures.is_empty(),
        "server evals failed over {target}:\n{}\n\n{table}",
        failures.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenarios_over_stdio() {
    run_and_report("stdio").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenarios_over_http_with_a_bearer_token() {
    run_and_report("http").await;
}

/// Runs only when `PSCAT_MCP_EVAL_IMAGE` names an image (see
/// `scripts/container-eval.sh`, which takes `IMAGE=`). It is a different
/// variable from `IMAGE` on purpose: `container.yml` sets `IMAGE` for the
/// whole workflow, and a stray `docker run` of the wrong image must not
/// follow from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenarios_in_the_container_image() {
    if std::env::var("PSCAT_MCP_EVAL_IMAGE")
        .unwrap_or_default()
        .is_empty()
    {
        eprintln!(
            "SKIPPED scenarios_in_the_container_image: PSCAT_MCP_EVAL_IMAGE is not set \
             (scripts/container-eval.sh IMAGE=... runs it)"
        );
        return;
    }
    run_and_report("container").await;
}

// ---------------------------------------------------------------------------
// The harness itself

#[test]
fn scenario_files_are_well_formed() {
    let scenarios = load_scenarios().expect("scenario files load and validate");
    // Placeholders in every step resolve.
    for sc in &scenarios {
        assert!(!sc.title.is_empty(), "{}: no title", sc.id);
        assert!(!sc.origin.is_empty(), "{}: no origin", sc.id);
        for step in &sc.steps {
            expand(step).unwrap_or_else(|e| panic!("{}: {e}", sc.id));
        }
    }
}

#[test]
fn matchers_follow_the_agent_layer_vocabulary() {
    let v = |x: Value| x;
    assert!(match_value(&json!(400), Some(&json!(400.0))).is_ok());
    assert!(match_value(&json!(400), Some(&json!(300))).is_err());
    assert!(match_value(&json!(1), None).is_err());
    assert!(match_value(&v(json!({"in": ["a", "b"]})), Some(&json!("b"))).is_ok());
    assert!(match_value(&v(json!({"in": ["a", null]})), None).is_ok());
    assert!(match_value(&v(json!({"absent": true})), None).is_ok());
    assert!(match_value(&v(json!({"absent": true})), Some(&json!(1))).is_err());
    assert!(match_value(&v(json!({"contains": ["ab", "cd"]})), Some(&json!("abcd"))).is_ok());
    assert!(match_value(&v(json!({"contains": "zz"})), Some(&json!("abcd"))).is_err());
    assert!(match_value(&v(json!({"not_contains": "zz"})), Some(&json!("abcd"))).is_ok());
    assert!(match_value(&v(json!({"min": 5, "max": 9})), Some(&json!(7))).is_ok());
    assert!(match_value(&v(json!({"min": 5})), Some(&json!(4))).is_err());
    assert!(match_value(&v(json!({"equals_text": "a \nb\n"})), Some(&json!("a\nb"))).is_ok());
    assert!(
        match_value(
            &v(json!({"regex": "^\\d+\\.\\d+\\.\\d+"})),
            Some(&json!("0.1.0"))
        )
        .is_ok()
    );
    assert!(match_value(&v(json!({"regex": "^\\d+$"})), Some(&json!("0.1"))).is_err());
    assert!(match_value(&v(json!({"nope": 1})), Some(&json!(1))).is_err());
}

#[test]
fn a_result_spec_catches_each_kind_of_difference() {
    let ok = json!({
        "isError": false,
        "content": [
            {"type": "image", "mimeType": "image/png", "data": ""},
            {"type": "text", "text": "Rendered 1 page"}
        ],
        "structuredContent": {"ok": true, "artifacts": [{"width_px": 40}]}
    });
    fn obs(r: &Value) -> Observed<'_> {
        Observed {
            result: r,
            elapsed_ms: 50,
            renders: Some(1),
        }
    }
    let pass = json!({
        "content_types": ["image", "text"],
        "forbid_content_types": ["resource"],
        "structured": {"ok": true, "artifacts.0.width_px": 40},
        "text_contains": ["rendered"],
        "elapsed_ms": {"max": 100},
        "renders_executed": 1
    });
    assert_eq!(check_result(&pass, &obs(&ok)), Vec::<String>::new());
    for (bad, what) in [
        (json!({"is_error": true}), "is_error"),
        (json!({"error_code": "timeout"}), "error code"),
        (json!({"content_types": ["resource"]}), "no resource"),
        (
            json!({"forbid_content_types": ["image"]}),
            "unexpected image",
        ),
        (
            json!({"structured": {"artifacts.0.width_px": 41}}),
            "width_px",
        ),
        (json!({"text_contains": ["nope"]}), "lacks"),
        (json!({"elapsed_ms": {"max": 10}}), "elapsed_ms"),
        (json!({"renders_executed": 0}), "renders_executed"),
    ] {
        let problems = check_result(&bad, &obs(&ok)).join("|");
        assert!(problems.contains(what), "{what}: {problems}");
    }
}
