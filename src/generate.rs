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
//! `{"kind": "chalkboard_math", "params": {...}}`. Each kind has three hooks:
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
//! The page the plan returns is a contract the generated program must meet
//! ([`PlannedPage`]): either the whole page is fixed (a board), or only the
//! width and the dpi are, and the worker decides the height from the
//! untrusted input (a note's height is its line count) within a
//! [`DecidedHeight`] budget, which it enforces before anything is allocated.
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

use crate::config::{LimitsConfig, PSCAT_MAX_SIDE_PX};
use crate::handwrite::HandwriteParams;
use crate::pscat_extras::{ChalkBoard, ChalkFailure};
use crate::render::{ErrorCode, RenderError, device_px};
use crate::worker::{InterpLimits, PsDiagnostic};

/// What a [`JobMode::Generate`](crate::worker::JobMode::Generate) job
/// generates, with its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum GenerateJob {
    /// A LaTeX formula written on a chalkboard (`pscat::chalk`).
    ChalkboardMath(ChalkboardParams),
    /// Text written as a handwriting-style note (`/HandScript`,
    /// `lib/handscript.ps`); see [`crate::handwrite`].
    Handwrite(HandwriteParams),
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

/// The page a generated program will draw on, as far as it is known before
/// the program is written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PagePlan {
    /// What is fixed about the page.
    pub page: PlannedPage,
    /// Resolution in dots per inch.
    pub dpi: f64,
}

/// What the server fixes about a generated page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlannedPage {
    /// The whole page. The server checks it against the dpi, side and pixel
    /// limits; the worker refuses a program that draws on any other page.
    Fixed {
        /// Page size in points.
        page_points: (u32, u32),
    },
    /// The width and the dpi. The height comes from laying out the untrusted
    /// input, so only the worker can know it; it must fit the budget.
    FixedWidth(DecidedHeight),
}

/// The width a generated page must have, and the budget its height, decided
/// in the worker, must fit. Travels in the worker job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecidedHeight {
    /// Page width in points, already checked against the limits.
    pub width_points: u32,
    /// Most device pixels on the page (width x height).
    pub max_pixels: u64,
    /// Most device pixels on one side.
    pub max_side_px: u32,
}

/// Why a page of a decided height is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// `InvalidDimensions` for the side limit, `TooManyPixels` for the area.
    pub code: GenerateErrorCode,
    /// The page in device pixels.
    pub pixels: (u32, u32),
    /// The budget that was exceeded.
    pub budget: DecidedHeight,
}

impl DecidedHeight {
    /// Whether a page of this width and `height_points` at `dpi` is within
    /// the budget; `Ok` holds its device pixels. The side limit is checked
    /// first, then the area, as [`crate::render`] checks a fixed page.
    pub fn admits(&self, height_points: u32, dpi: f64) -> Result<(u32, u32), Refusal> {
        let px = (
            device_px(self.width_points, dpi),
            device_px(height_points, dpi),
        );
        let refuse = |code| Refusal {
            code,
            pixels: px,
            budget: *self,
        };
        if px.0 > self.max_side_px || px.1 > self.max_side_px {
            return Err(refuse(GenerateErrorCode::InvalidDimensions));
        }
        if u64::from(px.0) * u64::from(px.1) > self.max_pixels {
            return Err(refuse(GenerateErrorCode::TooManyPixels));
        }
        Ok(px)
    }
}

impl Refusal {
    /// The failure to report. `what` names the page ("the note"), `extent`
    /// what made it that size ("97 lines") and `advice` what to change.
    pub fn failure(&self, what: &str, extent: &str, advice: &str) -> GenerateFailure {
        let (w, h) = self.pixels;
        let message = match self.code {
            GenerateErrorCode::TooManyPixels => format!(
                "{what} ({extent}) would be {w}x{h} = {} pixels; this server draws at most {} \
                 pixels. {advice}",
                u64::from(w) * u64::from(h),
                self.budget.max_pixels
            ),
            _ => format!(
                "{what} ({extent}) would be {w}x{h} pixels; this server draws at most {} \
                 pixels a side. {advice}",
                self.budget.max_side_px
            ),
        };
        GenerateFailure {
            code: self.code,
            detail: "height".into(),
            token: None,
            message,
        }
    }
}

/// What a generator spent of the interpreter limits before the render. The
/// worker takes it off the render's step and time limits, so that all the
/// programs of one job together stay within them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spent {
    /// Interpreter steps.
    pub steps: u64,
    /// Milliseconds of interpreter time.
    pub elapsed_ms: u64,
}

/// What the worker tells a generator about the job.
#[derive(Debug, Clone, Copy)]
pub struct GenerateContext<'a> {
    /// The interpreter limits every program of the job runs under.
    pub limits: &'a InterpLimits,
    /// The height budget, when the plan decided only the width.
    pub decided: Option<&'a DecidedHeight>,
}

/// A program a generator wrote, with its page.
#[derive(Debug, Clone)]
pub struct GeneratedProgram {
    /// The PostScript program.
    pub source: String,
    /// Page size in points. The plan's for a fixed page; for a decided
    /// height, the planned width and the height the generator chose.
    pub page_points: (u32, u32),
    /// Resolution; the same as the plan's.
    pub dpi: f64,
    /// Render on a transparent page instead of a white one.
    pub transparent: bool,
    /// What the generator spent running PostScript of its own.
    pub spent: Spent,
}

/// Why a generator could not make its program.
#[derive(Debug, Clone)]
pub enum GenerateError {
    /// The input is refused; reported with the generator's code.
    Input(GenerateFailure),
    /// A PostScript program the generator ran raised an error or passed an
    /// interpreter limit; reported like the render's.
    PostScript(Box<PsDiagnostic>),
    /// The job itself is unusable (a bug, not the input).
    Internal(String),
}

impl From<GenerateFailure> for GenerateError {
    fn from(f: GenerateFailure) -> Self {
        GenerateError::Input(f)
    }
}

/// Why a generator could not make its output. Crosses the worker protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateFailure {
    /// `invalid_request` for input that is wrong before anything runs (a
    /// config value); `postscript_error` for input the program could not
    /// draw (an unsupported TeX command) or a failed run;
    /// `too_many_pixels` and `invalid_dimensions` for a page whose size the
    /// generator decided and the budget refuses.
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
    /// [`ErrorCode::TooManyPixels`].
    TooManyPixels,
    /// [`ErrorCode::InvalidDimensions`].
    InvalidDimensions,
}

impl From<GenerateErrorCode> for ErrorCode {
    fn from(c: GenerateErrorCode) -> Self {
        match c {
            GenerateErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
            GenerateErrorCode::PostscriptError => ErrorCode::PostscriptError,
            GenerateErrorCode::TooManyPixels => ErrorCode::TooManyPixels,
            GenerateErrorCode::InvalidDimensions => ErrorCode::InvalidDimensions,
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
            token: f.token.map(|t| bounded(&t, MAX_TOKEN_CHARS)),
            message: bounded(&f.message, MAX_MESSAGE_CHARS),
        }
    }
}

/// Longest generator error message kept. PSCat's config reader quotes the
/// offending value (`got ...`), which a client controls, so a message is cut
/// here to keep the reply small.
const MAX_MESSAGE_CHARS: usize = 400;
/// Longest token (a command or environment name) kept.
const MAX_TOKEN_CHARS: usize = 100;

/// `s` cut to `max` characters, with `…` when cut.
pub(crate) fn bounded(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

impl GenerateJob {
    /// The kind's wire name, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            GenerateJob::ChalkboardMath(_) => "chalkboard_math",
            GenerateJob::Handwrite(_) => "handwrite",
        }
    }

    /// Bytes of untrusted input the job carries, for logs.
    pub fn input_len(&self) -> usize {
        match self {
            GenerateJob::ChalkboardMath(p) => p.latex.len(),
            GenerateJob::Handwrite(p) => p.text.len(),
        }
    }

    /// Server side, before any worker starts: check the input's size and
    /// everything that needs no parsing or layout of it, and return what is
    /// known of the page. The caller checks that against the dpi, side and
    /// pixel limits.
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
                let board = ChalkBoard::from_args(&p.board).map_err(|m| {
                    RenderError::new(ErrorCode::InvalidRequest, bounded(&m, MAX_MESSAGE_CHARS))
                })?;
                let (page_points, dpi) = board.page();
                Ok(PagePlan {
                    page: PlannedPage::Fixed { page_points },
                    dpi: f64::from(dpi),
                })
            }
            GenerateJob::Handwrite(p) => {
                if p.text.len() > limits.max_handwrite_bytes {
                    return Err(RenderError::new(
                        ErrorCode::SourceTooLarge,
                        format!(
                            "text is {} bytes; the limit is {} bytes (split the note)",
                            p.text.len(),
                            limits.max_handwrite_bytes
                        ),
                    ));
                }
                let note = p.resolve().map_err(|m| {
                    RenderError::new(ErrorCode::InvalidRequest, bounded(&m, MAX_MESSAGE_CHARS))
                })?;
                if !(1..=crate::handwrite::MAX_PAGE_POINTS).contains(&note.width) {
                    return Err(RenderError::new(
                        ErrorCode::InvalidDimensions,
                        format!(
                            "width must be between 1 and {} points, got {}",
                            crate::handwrite::MAX_PAGE_POINTS,
                            note.width
                        ),
                    ));
                }
                Ok(PagePlan {
                    page: PlannedPage::FixedWidth(DecidedHeight {
                        width_points: note.width,
                        max_pixels: limits.max_pixels,
                        max_side_px: PSCAT_MAX_SIDE_PX,
                    }),
                    dpi: note.dpi,
                })
            }
        }
    }

    /// Worker side: write the program (and, for a decided height, size the
    /// page and check it against the budget first).
    pub fn generate(&self, cx: &GenerateContext<'_>) -> Result<GeneratedProgram, GenerateError> {
        match self {
            GenerateJob::ChalkboardMath(p) => {
                let board = ChalkBoard::from_args(&p.board).map_err(|message| GenerateFailure {
                    code: GenerateErrorCode::InvalidRequest,
                    detail: "config".into(),
                    token: None,
                    message: bounded(&message, MAX_MESSAGE_CHARS),
                })?;
                let d = board.driver(&p.latex).map_err(GenerateFailure::from)?;
                Ok(GeneratedProgram {
                    source: d.source,
                    page_points: d.page_points,
                    dpi: f64::from(d.dpi),
                    transparent: d.transparent,
                    spent: Spent::default(),
                })
            }
            GenerateJob::Handwrite(p) => {
                let note = p.resolve().map_err(|message| GenerateFailure {
                    code: GenerateErrorCode::InvalidRequest,
                    detail: "arguments".into(),
                    token: None,
                    message: bounded(&message, MAX_MESSAGE_CHARS),
                })?;
                note.generate(cx)
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
            GenerateJob::Handwrite(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LIMITS: InterpLimits = InterpLimits {
        max_steps: Some(1_000_000_000),
        max_time_ms: Some(20_000),
        // The unit-test binary has no counting allocator, which a memory
        // limit needs; tests/handwrite.rs runs the real worker with one.
        max_memory_bytes: None,
        max_page_pixels: 16_000_000,
        max_pages: 10,
        max_output_bytes: 8 << 20,
        max_program_output_bytes: 16 << 10,
    };

    fn cx(decided: Option<&DecidedHeight>) -> GenerateContext<'_> {
        GenerateContext {
            limits: &LIMITS,
            decided,
        }
    }

    fn input_failure(r: Result<GeneratedProgram, GenerateError>) -> GenerateFailure {
        match r.unwrap_err() {
            GenerateError::Input(f) => f,
            other => panic!("expected an input failure, got {other:?}"),
        }
    }

    fn hand(text: &str, params: Value) -> GenerateJob {
        let mut p = params;
        p["text"] = text.into();
        GenerateJob::Handwrite(serde_json::from_value(p).unwrap())
    }

    fn chalk(latex: &str, board: Value) -> GenerateJob {
        GenerateJob::ChalkboardMath(ChalkboardParams {
            latex: latex.into(),
            board: board.as_object().unwrap().clone(),
        })
    }

    #[test]
    fn messages_quoting_the_input_are_bounded() {
        let huge = "x".repeat(100_000);
        let job = chalk("x", json!({"board": huge}));
        let err = job.plan(&LimitsConfig::default()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.chars().count() <= MAX_MESSAGE_CHARS + 1);
        assert!(err.message.ends_with('…'));
        let fail = input_failure(job.generate(&cx(None)));
        assert!(fail.message.chars().count() <= MAX_MESSAGE_CHARS + 1);
        let long_cmd = format!("mk-unsupported-command:\\{}", "a".repeat(5000));
        let e = job.explain(&pscat::PsError::Undefined(long_cmd)).unwrap();
        assert!(e.token.unwrap().chars().count() <= MAX_TOKEN_CHARS + 1);
        assert!(e.message.chars().count() <= MAX_MESSAGE_CHARS + 1);
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
                page: PlannedPage::Fixed {
                    page_points: (800, 400)
                },
                dpi: 144.0
            }
        );
    }

    #[test]
    fn generate_writes_a_program_and_names_row_errors() {
        let p = chalk("a^2+b^2=c^2", json!({"board": false}))
            .generate(&cx(None))
            .unwrap();
        assert!(p.transparent);
        assert_eq!(p.page_points, (800, 400));
        assert!(p.source.contains("mathkit"));
        let err = input_failure(chalk(r"\(x", json!({})).generate(&cx(None)));
        assert_eq!(err.code, GenerateErrorCode::PostscriptError);
        let err = input_failure(chalk("% only a comment", json!({})).generate(&cx(None)));
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

    fn budget(width_points: u32, max_pixels: u64) -> DecidedHeight {
        DecidedHeight {
            width_points,
            max_pixels,
            max_side_px: 8000,
        }
    }

    #[test]
    fn a_decided_height_is_checked_side_first_then_area() {
        let b = budget(1000, 2_000_000);
        assert_eq!(b.admits(1000, 72.0), Ok((1000, 1000)));
        // 1000 x 2001 = 2.001 M pixels
        let r = b.admits(2001, 72.0).unwrap_err();
        assert_eq!(r.code, GenerateErrorCode::TooManyPixels);
        assert_eq!(r.pixels, (1000, 2001));
        // over the side limit, which is also over the area: the side first
        let r = b.admits(8001, 72.0).unwrap_err();
        assert_eq!(r.code, GenerateErrorCode::InvalidDimensions);
        // dpi scales both sides
        assert_eq!(b.admits(500, 144.0), Ok((2000, 1000)));
        assert_eq!(b.admits(501, 144.0).unwrap_err().pixels, (2000, 1002));
        let f = b
            .admits(8001, 72.0)
            .unwrap_err()
            .failure("the note", "99 lines", "Shorten it");
        assert!(
            f.message
                .contains("the note (99 lines) would be 1000x8001 pixels"),
            "{}",
            f.message
        );
        assert!(
            f.message.contains("8000 pixels a side. Shorten it"),
            "{}",
            f.message
        );
        assert_eq!(f.detail, "height");
    }

    #[test]
    fn handwrite_plans_a_width_and_checks_the_text_size() {
        let limits = LimitsConfig {
            max_handwrite_bytes: 8,
            ..LimitsConfig::default()
        };
        let code = |job: GenerateJob| job.plan(&limits).unwrap_err().code;
        assert_eq!(code(hand("", json!({}))), ErrorCode::InvalidRequest);
        assert_eq!(
            code(hand("a b c d e", json!({}))),
            ErrorCode::SourceTooLarge
        );
        assert_eq!(
            code(hand("hi", json!({"paper": "x"}))),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            code(hand("hi", json!({"width": 0}))),
            ErrorCode::InvalidDimensions
        );
        assert_eq!(
            code(hand("hi", json!({"width": 8001}))),
            ErrorCode::InvalidDimensions
        );
        // The height is not planned: only the width is.
        let plan = hand("hi", json!({"width": 300, "dpi": 144}))
            .plan(&limits)
            .unwrap();
        assert_eq!(
            plan,
            PagePlan {
                page: PlannedPage::FixedWidth(budget(300, 16_000_000)),
                dpi: 144.0
            }
        );
        let wire = serde_json::to_value(hand("hi", json!({"seed": 3}))).unwrap();
        assert_eq!(wire["kind"], "handwrite");
        assert_eq!(wire["params"]["text"], "hi");
    }

    #[test]
    fn handwrite_sizes_the_page_and_checks_the_budget_before_drawing() {
        let b = budget(612, 16_000_000);
        let one = hand("hello", json!({})).generate(&cx(Some(&b))).unwrap();
        assert_eq!(one.page_points, (612, 146));
        assert_eq!(one.dpi, 72.0);
        assert!(one.source.contains("hs-write showpage"));
        assert!(one.spent.steps > 0);
        let three = hand("a\nb\nc", json!({})).generate(&cx(Some(&b))).unwrap();
        assert_eq!(three.page_points, (612, 254));
        // 10 lines = 631.8 -> 632 points; the cap is 612 x 631 pixels.
        let tight = budget(612, 612 * 631);
        let ten = "a\n".repeat(10);
        let err = input_failure(hand(&ten, json!({})).generate(&cx(Some(&tight))));
        assert_eq!(err.code, GenerateErrorCode::TooManyPixels);
        assert!(err.message.contains("10 lines"), "{}", err.message);
        // Without a budget the generator refuses to guess.
        assert!(matches!(
            hand("hi", json!({})).generate(&cx(None)),
            Err(GenerateError::Internal(_))
        ));
        // A step limit hit in the measuring pass is a PostScript diagnostic.
        let mut limits = LIMITS;
        limits.max_steps = Some(500);
        let cx = GenerateContext {
            limits: &limits,
            decided: Some(&b),
        };
        match hand("hello", json!({})).generate(&cx) {
            Err(GenerateError::PostScript(d)) => assert_eq!(d.limit.as_deref(), Some("steps")),
            other => panic!("expected a limit, got {other:?}"),
        }
    }
}
