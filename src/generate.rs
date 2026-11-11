//! Generated programs: tools whose input is not PostScript.
//!
//! A tool such as `chalkboard_math` takes some other input (LaTeX, note
//! text, a list of sheet entries), and PSCat turns it into a PostScript
//! program. That input is as untrusted as a program: a pathological formula
//! can take unbounded time or recursion in the generator itself. So the
//! generator runs in the worker, as one [`JobMode::Generate`] job, under the
//! same rlimits, interpreter limits and deadline as a render.
//!
//! A [`GenerateJob`] is one variant per kind, tagged on the wire as
//! `{"kind": "chalkboard_math", "params": {...}}`. Each kind has two phases:
//!
//! * [`GenerateJob::plan`], in the server, before any worker starts: checks
//!   what needs no parsing of the untrusted input (its size; for a board,
//!   the board config) and returns the page, which then passes the same
//!   dpi, side and pixel checks as a render. A request that fails here is
//!   rejected like a bad render request: no slot, no worker.
//! * [`GenerateJob::generate`], in the worker: writes the program.
//!   [`crate::worker`] then runs it through the ordinary render path (PNG,
//!   SVG or PDF encoding, page and byte caps), and asks
//!   [`GenerateJob::explain`] whether a PostScript error stands for a known
//!   input error (an unsupported TeX command, for example).
//!
//! Adding a kind means a variant here with its `plan`, `generate` and
//! `explain` arms, a `RenderService::generate` caller in `mcp.rs`, and any
//! hidden PSCat calls in `pscat_extras`. A generator that produces pages
//! rather than a program (a contact sheet) would add a second output shape
//! next to [`GeneratedProgram`].
//!
//! [`JobMode::Generate`]: crate::worker::JobMode::Generate

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::LimitsConfig;
use crate::pscat_extras::{ChalkBoard, ChalkFailure};
use crate::render::{ErrorCode, RenderError};

/// What a [`JobMode::Generate`](crate::worker::JobMode::Generate) job
/// generates, with its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum GenerateJob {
    /// A LaTeX formula written on a chalkboard (`pscat::chalk`).
    ChalkboardMath(ChalkboardParams),
}

/// Parameters of a chalkboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChalkboardParams {
    /// The formula, as typed in a LaTeX document.
    pub latex: String,
    /// The board arguments as the tool received them: `config` and the flat
    /// keys (`size`, `irregular`, `slope`, `jitter`, `align`, `color`,
    /// `board`, `ghosting`, `seed`, `width`, `height`, `dpi`). Only present
    /// ones.
    pub board: Map<String, Value>,
}

/// The page a generated program will draw on, known before it is written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PagePlan {
    /// Page size in points.
    pub page_points: (u32, u32),
    /// Resolution in dots per inch.
    pub dpi: f64,
}

/// A program a generator wrote, with its page.
#[derive(Debug, Clone)]
pub struct GeneratedProgram {
    /// The PostScript program.
    pub source: String,
    /// Page size in points; the same as the plan's.
    pub page_points: (u32, u32),
    /// Resolution; the same as the plan's.
    pub dpi: f64,
    /// Render on a transparent page instead of a white one.
    pub transparent: bool,
}

/// Why a generator could not make its output. Crosses the worker protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateFailure {
    /// `invalid_request` for input that is wrong before anything runs (a
    /// config value); `postscript_error` for input the program could not
    /// draw (an unsupported TeX command) or a failed run.
    pub code: GenerateErrorCode,
    /// The generator's stable code (`unsupported_command`, `config`, ...).
    pub detail: String,
    /// The command, character or name the error is about, if any.
    #[serde(default)]
    pub token: Option<String>,
    /// One line for the person or agent that made the request.
    pub message: String,
}

/// The [`ErrorCode`] a [`GenerateFailure`] is reported with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerateErrorCode {
    /// [`ErrorCode::InvalidRequest`].
    InvalidRequest,
    /// [`ErrorCode::PostscriptError`].
    PostscriptError,
}

impl From<GenerateErrorCode> for ErrorCode {
    fn from(c: GenerateErrorCode) -> Self {
        match c {
            GenerateErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
            GenerateErrorCode::PostscriptError => ErrorCode::PostscriptError,
        }
    }
}

impl From<ChalkFailure> for GenerateFailure {
    fn from(f: ChalkFailure) -> Self {
        GenerateFailure {
            code: if f.invalid_input {
                GenerateErrorCode::InvalidRequest
            } else {
                GenerateErrorCode::PostscriptError
            },
            detail: f.code,
            token: f.token,
            message: f.message,
        }
    }
}

impl GenerateJob {
    /// The kind's wire name, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            GenerateJob::ChalkboardMath(_) => "chalkboard_math",
        }
    }

    /// Bytes of untrusted input the job carries, for logs.
    pub fn input_len(&self) -> usize {
        match self {
            GenerateJob::ChalkboardMath(p) => p.latex.len(),
        }
    }

    /// Server side, before any worker starts: check the input's size and
    /// everything that needs no parsing of it, and return the page. The
    /// caller checks the page against the dpi, side and pixel limits.
    pub fn plan(&self, limits: &LimitsConfig) -> Result<PagePlan, RenderError> {
        match self {
            GenerateJob::ChalkboardMath(p) => {
                if p.latex.trim().is_empty() {
                    return Err(RenderError::new(
                        ErrorCode::InvalidRequest,
                        "latex is empty",
                    ));
                }
                if p.latex.len() > limits.max_latex_bytes {
                    return Err(RenderError::new(
                        ErrorCode::SourceTooLarge,
                        format!(
                            "latex is {} bytes; the limit is {} bytes (split the formula over \
                             several boards)",
                            p.latex.len(),
                            limits.max_latex_bytes
                        ),
                    ));
                }
                let board = ChalkBoard::from_args(&p.board)
                    .map_err(|m| RenderError::new(ErrorCode::InvalidRequest, m))?;
                let (page_points, dpi) = board.page();
                Ok(PagePlan {
                    page_points,
                    dpi: f64::from(dpi),
                })
            }
        }
    }

    /// Worker side: write the program.
    pub fn generate(&self) -> Result<GeneratedProgram, GenerateFailure> {
        match self {
            GenerateJob::ChalkboardMath(p) => {
                let board = ChalkBoard::from_args(&p.board).map_err(|message| GenerateFailure {
                    code: GenerateErrorCode::InvalidRequest,
                    detail: "config".into(),
                    token: None,
                    message,
                })?;
                let d = board.driver(&p.latex)?;
                Ok(GeneratedProgram {
                    source: d.source,
                    page_points: d.page_points,
                    dpi: f64::from(d.dpi),
                    transparent: d.transparent,
                })
            }
        }
    }

    /// Worker side: the input error a PostScript error from the generated
    /// program stands for, if it is one. Limit errors are never explained.
    pub fn explain(&self, err: &pscat::PsError) -> Option<GenerateFailure> {
        match self {
            GenerateJob::ChalkboardMath(_) => {
                crate::pscat_extras::chalk_explain(err).map(GenerateFailure::from)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chalk(latex: &str, board: Value) -> GenerateJob {
        GenerateJob::ChalkboardMath(ChalkboardParams {
            latex: latex.into(),
            board: board.as_object().unwrap().clone(),
        })
    }

    #[test]
    fn the_wire_shape_is_kind_and_params() {
        let job = chalk("x", json!({"seed": 2}));
        let v = serde_json::to_value(&job).unwrap();
        assert_eq!(
            v,
            json!({"kind": "chalkboard_math", "params": {"latex": "x", "board": {"seed": 2}}})
        );
        assert_eq!(serde_json::from_value::<GenerateJob>(v).unwrap(), job);
    }

    #[test]
    fn plan_checks_size_and_config_only() {
        let limits = LimitsConfig {
            max_latex_bytes: 8,
            ..LimitsConfig::default()
        };
        let code = |job: GenerateJob| job.plan(&limits).unwrap_err().code;
        assert_eq!(code(chalk(" ", json!({}))), ErrorCode::InvalidRequest);
        assert_eq!(
            code(chalk("x+y+z+w+v", json!({}))),
            ErrorCode::SourceTooLarge
        );
        assert_eq!(
            code(chalk("x", json!({"color": "red"}))),
            ErrorCode::InvalidRequest
        );
        // An unsupported command is not seen by the plan: the LaTeX is not
        // parsed in the server.
        let plan = chalk(r"\nosuch", json!({})).plan(&limits).unwrap();
        assert_eq!(
            plan,
            PagePlan {
                page_points: (800, 400),
                dpi: 144.0
            }
        );
    }

    #[test]
    fn generate_writes_a_program_and_names_row_errors() {
        let p = chalk("a^2+b^2=c^2", json!({"board": false}))
            .generate()
            .unwrap();
        assert!(p.transparent);
        assert_eq!(p.page_points, (800, 400));
        assert!(p.source.contains("mathkit"));
        let err = chalk(r"\(x", json!({})).generate().unwrap_err();
        assert_eq!(err.code, GenerateErrorCode::PostscriptError);
        let err = chalk("% only a comment", json!({})).generate().unwrap_err();
        assert_eq!(err.code, GenerateErrorCode::InvalidRequest);
        assert_eq!(err.detail, "empty");
    }

    #[test]
    fn explain_turns_mathkit_errors_into_latex_errors() {
        let job = chalk("x", json!({}));
        let e = job
            .explain(&pscat::PsError::Undefined(
                "mk-unsupported-command:\\aleph".into(),
            ))
            .unwrap();
        assert_eq!(e.code, GenerateErrorCode::PostscriptError);
        assert_eq!(e.detail, "unsupported_command");
        assert_eq!(e.token.as_deref(), Some("\\aleph"));
        assert!(e.message.contains("\\aleph"), "{}", e.message);
        assert!(!e.message.contains("--help"), "{}", e.message);
        assert!(
            job.explain(&pscat::PsError::Undefined("foo".into()))
                .is_none()
        );
    }
}
