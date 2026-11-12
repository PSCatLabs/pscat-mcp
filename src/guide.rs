//! Agent guidance: the PSCat plugin's skills as MCP resources and prompts.
//!
//! The files of PSCat's `plugin/skills/` (the `pscat`, `psart`, `psviz` and
//! `functional-postscript` skills and their reference files) are vendored,
//! unchanged, in `guide/skills/` by `scripts/vendor-guide.py`, from the
//! commit `Cargo.lock` pins; `guide/SOURCE.json` records the tag and commit.
//! `build.rs` embeds every file, so the binary serves exactly what was
//! vendored and needs no files at run time.
//!
//! * **Resources.** Each file is a `text/markdown` resource at
//!   `pscat://guide/<path under plugin/skills>`, e.g.
//!   `pscat://guide/psart/references/paint.md`. The URIs mirror the
//!   directory layout, so the relative links in the text
//!   (`references/paint.md`, `../SKILL.md`) resolve against them.
//! * **Server note.** The text was written for the PSCat plugin, which runs
//!   the `pscat` command line on the user's machine and has tools and
//!   arguments this server does not (`out_path`, render jobs where async jobs
//!   are off, ...). PSCat's
//!   text is never rewritten. Instead `resources/read` returns a short note
//!   written by this server, then PSCat's text byte for byte. The note says
//!   where the text came from (tag, commit, path, copyright, licence) and
//!   what differs here; the tool lists in it are computed from the running
//!   server's own tools, so they stay true as tools are added.
//! * **Prompts.** A few prompts (`diagram`, `generative_art`, `chart`), each
//!   a short task written by this server followed by one guide document as
//!   served (note and text).

use std::fmt::Write as _;
use std::sync::OnceLock;

use rmcp::ErrorData;
use rmcp::model::{
    GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, ReadResourceResult,
    Resource, ResourceContents, Role,
};
use serde::Deserialize;
use serde_json::{Value, json};

include!(concat!(env!("OUT_DIR"), "/guide_files.rs"));

/// `guide/SOURCE.json`, written by `scripts/vendor-guide.py`.
const SOURCE_JSON: &str = include_str!("../guide/SOURCE.json");

/// Every guide resource URI starts with this.
pub const URI_PREFIX: &str = "pscat://guide/";
/// MIME type of every guide resource.
pub const MIME_TYPE: &str = "text/markdown";
/// Default longest value of one prompt argument
/// (`PSCAT_MCP_MAX_PROMPT_ARGUMENT_BYTES`).
pub const MAX_PROMPT_ARGUMENT_BYTES: usize = 8192;

/// The tools of the PSCat plugin's own MCP server (`pscat-mcp`, the one the
/// vendored text describes), to say which of them this server lacks.
const PLUGIN_TOOLS: &[&str] = &[
    "render_postscript",
    "eval_postscript",
    "handwrite",
    "chalkboard_math",
    "review_sheet",
    "describe_art_capabilities",
    "start_render",
    "render_status",
    "render_result",
    "cancel_render",
];

/// Arguments of the plugin's `render_postscript` that this server's may lack.
const PLUGIN_RENDER_ARGUMENTS: &[&str] = &["out_path", "bands", "bbox"];

/// Lines of the note for one file only, keyed by path under `guide/skills`.
/// A test checks that every key is a vendored file.
const FILE_NOTES: &[(&str, &str)] = &[
    (
        "pscat/references/mcp.md",
        "This page describes the plugin's own MCP server, not this one. This server's \
         limits are in `get_renderer_info`.",
    ),
    (
        "pscat/references/handwriting-chalk.md",
        "A TeX command outside mathkit's subset makes `chalkboard_math` fail with \
         `postscript_error`, and `error.input.code` and `error.input.token` name it.",
    ),
    (
        "psart/SKILL.md",
        "The image-model example reads `background.jpg` from disk, which this server \
         refuses (`invalidfileaccess`).",
    ),
];

/// The guide files that name font-pack faces (`/Bangers`, the Korean, Japanese
/// and Thai faces, ...). Each gets a line from [`PackFaces`]. A test checks
/// that every path is vendored.
const PACK_FACE_FILES: &[&str] = &[
    "pscat/SKILL.md",
    "psart/SKILL.md",
    "psart/references/styles-and-type.md",
];

/// Which font-pack faces this server can reach, as far as the guide says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PackFaces {
    /// No font-pack directory is configured, so no pack is ever reachable.
    #[default]
    NotConfigured,
    /// A directory is configured. `installed` names the packs the worker's
    /// catalog lists, or is `None` when the catalog could not be read.
    Configured {
        /// The installed pack names, or `None` when they could not be read.
        installed: Option<Vec<String>>,
    },
}

/// Where the vendored files came from (`guide/SOURCE.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    /// PSCat repository URL.
    pub repository: String,
    /// PSCat release tag.
    pub tag: String,
    /// PSCat crate version at that tag.
    pub version: String,
    /// Full commit hash.
    pub commit: String,
    /// Directory of the PSCat repository the files came from.
    pub path: String,
    /// SPDX licence expression of the files.
    pub license: String,
    /// Copyright holders.
    pub copyright: Vec<String>,
    /// Files, relative to `path`.
    pub files: Vec<String>,
}

/// The parsed `guide/SOURCE.json`.
pub fn source() -> &'static Source {
    static SOURCE: OnceLock<Source> = OnceLock::new();
    SOURCE.get_or_init(|| serde_json::from_str(SOURCE_JSON).expect("guide/SOURCE.json is valid"))
}

/// Every vendored file: `(path under guide/skills, PSCat's text)`.
pub fn files() -> &'static [(&'static str, &'static str)] {
    GUIDE_FILES
}

/// What the note needs to know about the running server.
#[derive(Debug, Clone, Default)]
pub struct ServerFacts {
    /// Names of this server's tools.
    pub tools: Vec<String>,
    /// Argument names of this server's `render_postscript`.
    pub render_arguments: Vec<String>,
    /// The render deadline, in milliseconds.
    pub render_timeout_ms: u64,
    /// The async job deadline, in milliseconds, when `start_render` is served.
    pub job_timeout_ms: Option<u64>,
    /// Longest value of one prompt argument, in bytes.
    pub max_prompt_argument_bytes: usize,
    /// Which font-pack faces are reachable (see [`PackFaces`]).
    pub pack_faces: PackFaces,
}

impl ServerFacts {
    fn has(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }
}

/// One served document.
#[derive(Debug, Clone)]
struct Doc {
    path: &'static str,
    title: String,
    /// The note, then PSCat's text.
    served: String,
}

/// The guide resources and prompts, built once per server.
#[derive(Debug, Clone)]
pub struct Guide {
    docs: Vec<Doc>,
    max_argument_bytes: usize,
}

/// One prompt: its definition and the document it carries.
struct PromptDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    doc: &'static str,
    arguments: &'static [(&'static str, &'static str)],
}

const PROMPTS: &[PromptDef] = &[
    PromptDef {
        name: "diagram",
        title: "PSCat diagram",
        description: "A task to draw a diagram in PostScript and render it with this server, with \
                      PSCat's `pscat` skill (pscat://guide/pscat/SKILL.md) attached. Every \
                      argument is optional.",
        doc: "pscat/SKILL.md",
        arguments: &[
            ("subject", "What the diagram shows."),
            ("format", "Output format: png (default), svg or pdf."),
        ],
    },
    PromptDef {
        name: "generative_art",
        title: "Generative art with artkit",
        description: "A task to make a generative art piece with PSCat's artkit library and \
                      render it with this server, with PSCat's `psart` skill \
                      (pscat://guide/psart/SKILL.md) attached. Every argument is optional.",
        doc: "psart/SKILL.md",
        arguments: &[
            ("subject", "What the piece is about."),
            (
                "palette",
                "An artkit palette name, e.g. dusk or tide (describe_art_capabilities, kind \
                 palette, lists them).",
            ),
            (
                "seed",
                "An integer for `srand`, so the piece renders the same each time.",
            ),
        ],
    },
    PromptDef {
        name: "chart",
        title: "Chart with dataviz.ps or graph.ps",
        description: "A task to make a paper-ready chart or plot with PSCat's lib/dataviz.ps or \
                      lib/graph.ps and render it with this server, with PSCat's `psviz` skill \
                      (pscat://guide/psviz/SKILL.md) attached. Every argument is optional.",
        doc: "psviz/SKILL.md",
        arguments: &[
            (
                "kind",
                "bar, line, area, scatter or pie (dataviz.ps), or function, parametric, polar \
                 or surface (graph.ps).",
            ),
            ("data", "The numbers or the function to plot."),
        ],
    },
];

const CHART_KINDS: &[&str] = &[
    "bar",
    "line",
    "area",
    "scatter",
    "pie",
    "function",
    "parametric",
    "polar",
    "surface",
];

impl Guide {
    /// The guide for a server with `facts`.
    pub fn new(facts: &ServerFacts) -> Self {
        let docs = files()
            .iter()
            .map(|(path, text)| Doc {
                path,
                title: title_of(path, text),
                served: format!("{}{text}", note(path, facts)),
            })
            .collect();
        Guide {
            docs,
            max_argument_bytes: facts.max_prompt_argument_bytes,
        }
    }

    fn doc(&self, path: &str) -> Option<&Doc> {
        self.docs.iter().find(|d| d.path == path)
    }

    /// `resources/list`: every document.
    pub fn resources(&self) -> Vec<Resource> {
        let src = source();
        self.docs
            .iter()
            .map(|d| {
                Resource::new(uri(d.path), d.path)
                    .with_title(d.title.clone())
                    .with_description(format!(
                        "{} ({}/{} from PSCat {}, with a note on what differs on this server).",
                        d.title, src.path, d.path, src.tag
                    ))
                    .with_mime_type(MIME_TYPE)
                    .with_size(d.served.len() as u64)
            })
            .collect()
    }

    /// `resources/read`: the note and PSCat's text, or `resource_not_found`.
    pub fn read(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let doc = uri
            .strip_prefix(URI_PREFIX)
            .and_then(|path| self.doc(path))
            .ok_or_else(|| {
                ErrorData::resource_not_found(
                    format!("no resource {uri}; resources/list lists the guide documents"),
                    Some(json!({ "uri": uri })),
                )
            })?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(doc.served.clone(), uri.to_string()).with_mime_type(MIME_TYPE),
        ]))
    }

    /// The served text of the document at `path` (under `guide/skills`).
    pub fn served(&self, path: &str) -> Option<&str> {
        self.doc(path).map(|d| d.served.as_str())
    }

    /// `prompts/list`.
    pub fn prompts(&self) -> Vec<Prompt> {
        PROMPTS
            .iter()
            .map(|p| {
                let arguments = p
                    .arguments
                    .iter()
                    .map(|(name, description)| {
                        PromptArgument::new(*name)
                            .with_description(*description)
                            .with_required(false)
                    })
                    .collect();
                Prompt::new(p.name, Some(p.description), Some(arguments)).with_title(p.title)
            })
            .collect()
    }

    /// `prompts/get`: the task and the document, or `invalid_params` for an
    /// unknown prompt or a bad argument.
    pub fn prompt(
        &self,
        name: &str,
        arguments: Option<&JsonObject>,
    ) -> Result<GetPromptResult, ErrorData> {
        let def = PROMPTS.iter().find(|p| p.name == name).ok_or_else(|| {
            let names: Vec<&str> = PROMPTS.iter().map(|p| p.name).collect();
            ErrorData::invalid_params(
                format!("unknown prompt {name:?}; prompts: {}", names.join(", ")),
                Some(json!({ "name": name })),
            )
        })?;
        let args = PromptArgs::parse(def, arguments, self.max_argument_bytes)?;
        let task = match def.name {
            "diagram" => diagram_task(&args)?,
            "generative_art" => art_task(&args)?,
            _ => chart_task(&args)?,
        };
        let doc = self
            .doc(def.doc)
            .expect("prompt documents are vendored (tested)");
        let text = format!(
            "{task}\n\nThe guide below is {uri} as this server serves it: a note on what \
             differs on this server, then PSCat's text.\n\n{}",
            doc.served,
            uri = uri(def.doc),
        );
        Ok(
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
                .with_description(def.title),
        )
    }
}

/// The URI of the document at `path` (under `guide/skills`).
pub fn uri(path: &str) -> String {
    format!("{URI_PREFIX}{path}")
}

/// The first `# ` heading, or the path.
fn title_of(path: &str, text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// `a`, `a or b`, `a, b or c` (each in backticks).
fn either(items: &[&str]) -> String {
    list(items, "or")
}

/// `a`, `a and b`, `a, b and c` (each in backticks), with `conjunction`
/// before the last item.
fn list(items: &[&str], conjunction: &str) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("`{i}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}

/// The note's line on font-pack faces.
fn pack_line(packs: &PackFaces) -> String {
    match packs {
        PackFaces::NotConfigured => {
            "- Font-pack faces are not available on this server (no font-pack directory is \
             configured). A name from a pack, such as `/Bangers` or a Korean, Japanese or \
             Thai face, falls back to a built-in face. The standard 35 and the `lib/fonts` \
             faces are built in."
                .to_string()
        }
        PackFaces::Configured { installed: None } => {
            "- The font packs installed on this server could not be read just now, so a name \
             from a pack may fall back to a built-in face. `get_renderer_info` reports \
             `font_packs.installed`."
                .to_string()
        }
        PackFaces::Configured {
            installed: Some(names),
        } if names.is_empty() => {
            "- No font pack is installed on this server, so a name from a pack falls back to a \
             built-in face. The standard 35 and the `lib/fonts` faces are built in."
                .to_string()
        }
        PackFaces::Configured {
            installed: Some(names),
        } => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            format!(
                "- Font packs installed on this server: {}. Their faces are available; a face \
                 from any other pack falls back to a built-in face. `describe_art_capabilities` \
                 lists the faces.",
                list(&names, "and")
            )
        }
    }
}

/// The note this server puts before PSCat's text for the file at `path`.
fn note(path: &str, facts: &ServerFacts) -> String {
    let src = source();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "**Note from pscat-mcp** (written by this server; not part of PSCat's text). \
         Below, unchanged, is `{}/{path}` from PSCat {} (commit {}), Copyright {}, \
         {}. It was written for the PSCat plugin, which runs `pscat` on the user's own \
         machine. On this server:",
        src.path,
        src.tag,
        src.commit,
        src.copyright.join("; "),
        src.license,
    ));
    let mut tools = facts.tools.clone();
    tools.sort();
    let mut tools_line = format!(
        "- The tools are {}.",
        tools
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let missing: Vec<&str> = PLUGIN_TOOLS
        .iter()
        .copied()
        .filter(|t| !facts.has(t))
        .collect();
    if !missing.is_empty() {
        let _ = write!(
            tools_line,
            " The plugin's {} {} not available here.",
            missing
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() == 1 { "is" } else { "are" },
        );
    }
    if !facts.has("eval_postscript") && facts.has("validate_postscript") {
        tools_line.push_str(
            " Where the text uses `eval_postscript`, `validate_postscript` runs the program and \
             returns what it printed, or its error, with lint findings.",
        );
    }
    lines.push(tools_line);
    lines.push(
        "- There is no `pscat` command line. `pscat --png`, `--svg` and `--pdf` are \
         `render_postscript` with `format`; `--page WxH` is `width` and `height`, `--dpi` is \
         `dpi`, `--halftone` is `halftone`; `--headless`, `-e` and `--lint` are \
         `validate_postscript`; `--capabilities` and `--fonts` are \
         `describe_art_capabilities`. Other options (sweeps, contact sheets, `--profile`, \
         `photo-etch`, the window) have no equivalent here."
            .to_string(),
    );
    for (command, tool) in [
        ("pscat handwrite", "handwrite"),
        ("pscat chalk", "chalkboard_math"),
        ("pscat sheet", "review_sheet"),
    ] {
        if facts.has(tool) {
            let last = lines.last_mut().expect("a line");
            let _ = write!(last, " `{command}` is the `{tool}` tool.");
        }
    }
    lines.push(
        "- Pages come back inline in the tool result. There is no `out_path`, nothing is \
         saved, and there is no file path to show the user."
            .to_string(),
    );
    lines.push(
        "- A program can read only PSCat's built-in `lib/` (for example `(lib/artkit.ps) \
         run`). Every other file, including the user's files and the repository's \
         `examples/` and `gallery/`, is `invalidfileaccess`, and nothing can be written."
            .to_string(),
    );
    let lacking: Vec<&str> = PLUGIN_RENDER_ARGUMENTS
        .iter()
        .copied()
        .filter(|a| !facts.render_arguments.iter().any(|r| r == a))
        .collect();
    if !lacking.is_empty() {
        lines.push(format!(
            "- `render_postscript` has no {} argument.",
            either(&lacking)
        ));
    }
    let jobs = match facts.job_timeout_ms {
        Some(ms) if facts.has("start_render") => {
            format!(" (a `start_render` job, within {} s)", ms.div_ceil(1000))
        }
        _ => String::new(),
    };
    lines.push(format!(
        "- Each render must finish within {} s on this server{jobs}; `get_renderer_info` \
         lists every limit.",
        facts.render_timeout_ms.div_ceil(1000)
    ));
    if PACK_FACE_FILES.contains(&path) {
        lines.push(pack_line(&facts.pack_faces));
    }
    for (p, extra) in FILE_NOTES {
        if *p == path {
            lines.push(format!("- {extra}"));
        }
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 1 {
            out.push_str(">\n");
        }
        let _ = writeln!(out, "> {line}");
    }
    out.push_str("\n---\n\n");
    out
}

/// A prompt's arguments, checked: only declared names, string values (an
/// integer is accepted for `seed`), each at most `max` bytes. Empty values
/// count as absent.
struct PromptArgs(Vec<(&'static str, String)>);

impl PromptArgs {
    fn parse(
        def: &PromptDef,
        arguments: Option<&JsonObject>,
        max: usize,
    ) -> Result<Self, ErrorData> {
        let mut out = Vec::new();
        for (key, value) in arguments.into_iter().flatten() {
            let Some((name, _)) = def.arguments.iter().find(|(n, _)| n == key) else {
                let names: Vec<&str> = def.arguments.iter().map(|(n, _)| *n).collect();
                return Err(bad_argument(
                    def.name,
                    key,
                    format!("unknown argument; {} takes {}", def.name, names.join(", ")),
                ));
            };
            let text = match value {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                Value::Null => continue,
                _ => return Err(bad_argument(def.name, key, "must be a string".into())),
            };
            if text.len() > max {
                return Err(bad_argument(
                    def.name,
                    key,
                    format!("longer than {max} bytes"),
                ));
            }
            if !text.is_empty() {
                out.push((*name, text));
            }
        }
        Ok(PromptArgs(out))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn bad_argument(prompt: &str, argument: &str, why: String) -> ErrorData {
    ErrorData::invalid_params(
        format!("prompt {prompt}: argument {argument:?}: {why}"),
        Some(json!({ "name": prompt, "argument": argument })),
    )
}

fn one_of(
    prompt: &str,
    argument: &str,
    value: &str,
    allowed: &[&str],
) -> Result<String, ErrorData> {
    let v = value.to_ascii_lowercase();
    if allowed.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(bad_argument(
            prompt,
            argument,
            format!("{value:?} is not one of {}", allowed.join(", ")),
        ))
    }
}

const COMMON_STEPS: &str = "\
- Write a complete program that ends each page with `showpage`. Size the page with \
render_postscript's `width` and `height` (points), or with `<< /PageSize [w h] >> setpagedevice`.
- Set type in the built-in standard 35 fonts (Helvetica, Times, Courier, Palatino, ...); \
describe_art_capabilities lists every face, palette and library procedure this build has.
- Look at the rendered page and refine it. A PostScript error comes back with the error name, \
the offending command and the line; validate_postscript runs a program and reports errors and \
lint findings without returning pages.
- PSCat's built-in libraries load with `(lib/NAME.ps) run`; no other file can be read.";

fn diagram_task(args: &PromptArgs) -> Result<String, ErrorData> {
    let format = args
        .get("format")
        .map(|f| one_of("diagram", "format", f, &["png", "svg", "pdf"]))
        .transpose()?;
    let mut t = String::from("Draw ");
    match args.get("subject") {
        Some(s) => {
            let _ = write!(t, "this as a diagram: {s}\n\nWrite it");
        }
        None => t.push_str("a diagram. Write it"),
    }
    t.push_str(" as a PostScript program and render it with this server's render_postscript tool");
    if let Some(f) = format {
        let _ = write!(t, " (format {f})");
    }
    t.push_str(".\n\n");
    t.push_str(COMMON_STEPS);
    if args.get("subject").is_none() {
        t.push_str("\n\nIf I have not said what the diagram shows yet, ask me first.");
    }
    Ok(t)
}

fn art_task(args: &PromptArgs) -> Result<String, ErrorData> {
    if let Some(p) = args.get("palette")
        && (p.len() > 64
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    {
        return Err(bad_argument(
            "generative_art",
            "palette",
            "a palette name is letters, digits, '_' and '-'".into(),
        ));
    }
    let seed = args
        .get("seed")
        .map(|s| {
            s.parse::<i32>().map_err(|_| {
                bad_argument(
                    "generative_art",
                    "seed",
                    format!("{s:?} is not an integer that fits PostScript's srand"),
                )
            })
        })
        .transpose()?;
    let mut t = String::from("Make a generative art piece");
    if let Some(s) = args.get("subject") {
        let _ = write!(t, ": {s}\n\nBuild it");
    } else {
        t.push_str(". Build it");
    }
    t.push_str(
        " with PSCat's artkit library (`(lib/artkit.ps) run`) and render it with this server's \
         render_postscript tool, as PNG.",
    );
    if let Some(p) = args.get("palette") {
        let _ = write!(t, " Use the /{p} palette (`/{p} pal`, `/{p} palpick`).");
    }
    if let Some(s) = seed {
        let _ = write!(t, " Start the program with `{s} srand`.");
    }
    t.push_str("\n\n");
    t.push_str(COMMON_STEPS);
    t.push_str("\n- Follow the guide's loop: write, render, look, refine.");
    Ok(t)
}

fn chart_task(args: &PromptArgs) -> Result<String, ErrorData> {
    let kind = args
        .get("kind")
        .map(|k| one_of("chart", "kind", k, CHART_KINDS))
        .transpose()?;
    let mut t = String::from("Make a paper-ready ");
    match &kind {
        Some(k) if ["function", "parametric", "polar", "surface"].contains(&k.as_str()) => {
            let _ = write!(t, "{k} plot with PSCat's lib/graph.ps");
        }
        Some(k) => {
            let _ = write!(t, "{k} chart with PSCat's lib/dataviz.ps");
        }
        None => t.push_str(
            "chart or plot with PSCat's lib/dataviz.ps (bar, line, area, scatter and pie \
             charts) or lib/graph.ps (function, parametric, polar and surface plots)",
        ),
    }
    t.push_str(", and render it with this server's render_postscript tool.");
    match args.get("data") {
        Some(d) => {
            let _ = write!(t, "\n\nThe data: {d}");
        }
        None => t.push_str(" If I have not given the data yet, ask me for it."),
    }
    t.push_str("\n\n");
    t.push_str(COMMON_STEPS);
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ServerFacts {
        ServerFacts {
            tools: vec!["render_postscript".into(), "validate_postscript".into()],
            render_arguments: vec!["source".into(), "bands".into()],
            render_timeout_ms: 20_000,
            job_timeout_ms: None,
            max_prompt_argument_bytes: MAX_PROMPT_ARGUMENT_BYTES,
            pack_faces: PackFaces::NotConfigured,
        }
    }

    #[test]
    fn source_matches_the_embedded_files() {
        let mut embedded: Vec<&str> = files().iter().map(|(p, _)| *p).collect();
        embedded.sort_unstable();
        let mut listed: Vec<&str> = source().files.iter().map(String::as_str).collect();
        listed.sort_unstable();
        assert_eq!(
            embedded, listed,
            "guide/SOURCE.json and guide/skills disagree"
        );
        assert!(!embedded.is_empty());
    }

    #[test]
    fn prompt_documents_and_file_notes_exist() {
        let guide = Guide::new(&facts());
        for p in PROMPTS {
            assert!(guide.served(p.doc).is_some(), "{} needs {}", p.name, p.doc);
        }
        for (path, _) in FILE_NOTES {
            assert!(guide.served(path).is_some(), "note for missing {path}");
        }
    }

    #[test]
    fn served_text_is_the_note_then_the_file() {
        let guide = Guide::new(&facts());
        for (path, text) in files() {
            let served = guide.served(path).unwrap();
            assert!(served.ends_with(text), "{path}");
            let n = &served[..served.len() - text.len()];
            assert!(n.starts_with("> **Note from pscat-mcp**"), "{path}");
            assert!(n.contains(&source().commit[..10]), "{path}");
        }
    }

    #[test]
    fn the_render_job_tools_follow_the_router() {
        // Async jobs off (HTTP by default): listed as missing.
        let off = note("pscat/SKILL.md", &facts());
        let line = off.lines().find(|l| l.contains("The tools are")).unwrap();
        assert!(line.contains("`start_render`"), "{line}");
        assert!(!off.contains("a `start_render` job"), "{off}");
        // Async jobs on (stdio): available, and their deadline is given.
        let mut on = facts();
        for t in [
            "start_render",
            "render_status",
            "render_result",
            "cancel_render",
        ] {
            on.tools.push(t.into());
        }
        on.job_timeout_ms = Some(600_000);
        let n = note("pscat/SKILL.md", &on);
        let line = n.lines().find(|l| l.contains("The tools are")).unwrap();
        let missing = line.split(" not available").next().unwrap();
        let missing = missing.rsplit("The plugin's").next().unwrap();
        for t in [
            "start_render",
            "render_status",
            "render_result",
            "cancel_render",
        ] {
            assert!(!missing.contains(&format!("`{t}`")), "{line}");
        }
        assert!(missing.contains("`eval_postscript`"), "{line}");
        assert!(n.contains("(a `start_render` job, within 600 s)"), "{n}");
    }

    #[test]
    fn note_names_what_is_missing() {
        let n = note("pscat/SKILL.md", &facts());
        assert!(n.contains("`review_sheet`"));
        assert!(n.contains("`start_render`"));
        assert!(!n.contains("`render_postscript` are not"));
        assert!(n.contains("no `out_path` or `bbox` argument"));
        assert!(!n.contains("`pscat chalk` is"));
        assert_eq!(either(&["a", "b", "c"]), "`a`, `b` or `c`");
        assert_eq!(either(&["a"]), "`a`");
        assert!(!n.contains("`bands`, "));
        assert!(n.contains("20 s"));
    }

    #[test]
    fn pack_files_are_vendored() {
        for path in PACK_FACE_FILES {
            assert!(files().iter().any(|(p, _)| p == path), "{path}");
        }
    }

    #[test]
    fn pack_line_follows_the_pack_facts() {
        let mut f = facts();
        let none = note("pscat/SKILL.md", &f);
        assert!(none.contains("Font-pack faces are not available on this server"));
        assert!(!none.contains("Font packs installed"));

        f.pack_faces = PackFaces::Configured {
            installed: Some(vec!["cjk-thai".into()]),
        };
        let some = note("pscat/SKILL.md", &f);
        assert!(some.contains("Font packs installed on this server: `cjk-thai`."));
        assert!(!some.contains("not available on this server"));

        f.pack_faces = PackFaces::Configured {
            installed: Some(vec!["cjk-thai".into(), "extras".into()]),
        };
        assert!(
            note("psart/SKILL.md", &f).contains("`cjk-thai` and `extras`"),
            "two packs are joined with and"
        );

        f.pack_faces = PackFaces::Configured {
            installed: Some(vec![]),
        };
        let empty = note("pscat/SKILL.md", &f);
        assert!(empty.contains("No font pack is installed on this server"));

        f.pack_faces = PackFaces::Configured { installed: None };
        let unread = note("pscat/SKILL.md", &f);
        assert!(unread.contains("could not be read just now"));
        assert!(unread.contains("`font_packs.installed`"));
    }

    #[test]
    fn only_the_pack_face_files_get_the_pack_line() {
        let f = facts();
        for (path, _) in files() {
            let has = note(path, &f).contains("Font-pack faces are not available");
            assert_eq!(has, PACK_FACE_FILES.contains(path), "{path}");
        }
    }

    #[test]
    fn prompt_arguments_are_checked() {
        let guide = Guide::new(&facts());
        let args = |v: Value| v.as_object().cloned().unwrap();
        assert!(guide.prompt("nope", None).is_err());
        assert!(
            guide
                .prompt("diagram", Some(&args(json!({"colour": "red"}))))
                .is_err()
        );
        assert!(
            guide
                .prompt("diagram", Some(&args(json!({"format": "gif"}))))
                .is_err()
        );
        assert!(
            guide
                .prompt("generative_art", Some(&args(json!({"seed": "x"}))))
                .is_err()
        );
        assert!(
            guide
                .prompt("generative_art", Some(&args(json!({"palette": "a b"}))))
                .is_err()
        );
        let long = "x".repeat(MAX_PROMPT_ARGUMENT_BYTES + 1);
        assert!(
            guide
                .prompt("chart", Some(&args(json!({"data": long}))))
                .is_err()
        );
        let ok = guide
            .prompt(
                "generative_art",
                Some(&args(json!({"seed": 7, "palette": "tide"}))),
            )
            .unwrap();
        let text = ok.messages[0].content.as_text().unwrap().text.clone();
        assert!(text.contains("`7 srand`"));
        assert!(text.contains("/tide pal"));
        assert!(text.ends_with(files().iter().find(|f| f.0 == "psart/SKILL.md").unwrap().1));
    }
}
