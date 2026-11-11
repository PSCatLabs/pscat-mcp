//! `handwrite`: text as a handwriting-style note.
//!
//! PSCat draws the note with its `/HandScript` library (`lib/handscript.ps`):
//! a dynamic Type 3 face whose every glyph is generated fresh, word-wrapped
//! to the page width by a layout engine written in PostScript. The PSCat
//! plugin's tool reaches it through the `pscat handwrite` subcommand, which
//! is part of the `pscat` binary and not of the library, so this module
//! does what that subcommand does (`src/subcmd/handwrite.rs` in the pinned
//! checkout), from the library's public pieces only:
//!
//! 1. a *measuring pass* runs `hs-linecount` on a default page and prints how
//!    many lines the text wraps to (the wrap does not depend on the height);
//! 2. the page height follows from the line count ([`auto_height`]);
//! 3. the *render* draws the note on a page of exactly that height.
//!
//! The note's height therefore depends on the untrusted text and is known
//! only in the worker, after step 1. A height budget (see
//! [`crate::generate::DecidedHeight`]) bounds it: the worker checks the
//! pixel cap and the side limit between steps 2 and 3, so an over-tall note
//! is a structured refusal before anything is allocated. Both passes run
//! under the worker's interpreter limits, and the render gets only what the
//! measuring pass left of the time and step limits.
//!
//! Everything that touches the text happens in the worker. The server only
//! checks what needs no layout ([`HandwriteParams::resolve`]): the text's
//! size and the arguments.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::generate::{
    GenerateContext, GenerateError, GenerateErrorCode, GenerateFailure, GeneratedProgram, Spent,
    bounded,
};
use crate::worker::PsDiagnostic;

/// Page margin in points; the plugin's default (`--margin`), not an argument.
const MARGIN: f64 = 54.0;
/// Baseline spacing as a multiple of the size (`--leading`).
const LEADING: f64 = 1.5;
/// Stroke width in glyph units (`--pen`).
const PEN: &str = "22";
/// Largest page side in points that PSCat's subcommand takes (`page_dim`).
pub const MAX_PAGE_POINTS: u32 = 8000;
/// Longest `ink` argument looked at.
const MAX_INK_CHARS: usize = 64;

/// The `handwrite` arguments, as the tool received them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandwriteParams {
    /// The note.
    pub text: String,
    /// Font size in points. Default 36.
    #[serde(default)]
    pub size: Option<f64>,
    /// Page width in points. Default 612.
    #[serde(default)]
    pub width: Option<u32>,
    /// `plain` (default) or `ruled`.
    #[serde(default)]
    pub paper: Option<String>,
    /// Ink colour as `R,G,B` in 0..1. Default `0.13,0.14,0.34`.
    #[serde(default)]
    pub ink: Option<String>,
    /// Ink wobble in glyph units, 0 calm. Default 16.
    #[serde(default)]
    pub jitter: Option<f64>,
    /// Random seed. Default 1985.
    #[serde(default)]
    pub seed: Option<i64>,
    /// Resolution. Default 72.
    #[serde(default)]
    pub dpi: Option<f64>,
}

/// The arguments checked, with the tokens that go into the program.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// The text as the library takes it, escaped for a string literal.
    text: String,
    size: f64,
    /// Number tokens as they are written into the options dictionary.
    size_token: String,
    jitter_token: String,
    ink_token: String,
    seed_token: String,
    paper: &'static str,
    /// Page width in points.
    pub width: u32,
    /// Resolution.
    pub dpi: f64,
}

/// A number as the library should read it: a whole value without a trailing
/// `.0` (`36` is an integer in PostScript and `36.0` a real, and the library
/// may care), anything else in plain decimal (Rust never prints an exponent).
fn number_token(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// `n` if it is a number in `lo..=hi`, else the message for argument `name`.
fn ranged(name: &str, n: Option<f64>, default: f64, lo: f64, hi: f64) -> Result<f64, String> {
    match n {
        None => Ok(default),
        Some(n) if (lo..=hi).contains(&n) => Ok(n),
        Some(n) => Err(format!(
            "{name} must be a number from {lo} to {hi}, got {n}"
        )),
    }
}

/// `R,G,B`: three plain decimals in 0..1, returned as the options
/// dictionary's `a b c`. Nothing else may reach the program through `ink`.
fn ink_token(ink: &str) -> Result<String, String> {
    let bad = || {
        format!(
            "ink must be R,G,B: three numbers from 0 to 1, for example 0.13,0.14,0.34; got {}",
            bounded(ink, 40)
        )
    };
    if ink.chars().count() > MAX_INK_CHARS {
        return Err(bad());
    }
    let parts: Vec<&str> = ink.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    for p in &parts {
        let plain = !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && p.bytes().filter(|b| *b == b'.').count() <= 1
            && p.bytes().any(|b| b.is_ascii_digit());
        if !plain || !p.parse::<f64>().is_ok_and(|v| (0.0..=1.0).contains(&v)) {
            return Err(bad());
        }
    }
    Ok(parts.join(" "))
}

/// Whether the hand has a glyph for `c` (after lowercasing): `a` to `z`,
/// `0` to `9` and `. , - ' ! ?`. Other characters advance invisibly.
fn drawable(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | ',' | '-' | '\'' | '!' | '?')
}

impl HandwriteParams {
    /// Check the text and the arguments. Needs no layout, so it is safe to
    /// run in the server; the text is only scanned for its characters.
    /// An `Err` is a message that names the argument at fault.
    pub fn resolve(&self) -> Result<Note, String> {
        if self.text.trim().is_empty() {
            return Err("text is empty".into());
        }
        if !self.text.chars().any(drawable) {
            return Err(
                "text has no character this hand can draw: it has lowercase and \
                        capital letters a to z, digits, and . , - ' ! ?"
                    .into(),
            );
        }
        let size = ranged("size", self.size, 36.0, 0.0, 2000.0)?;
        let jitter = ranged("jitter", self.jitter, 16.0, 0.0, 10_000.0)?;
        let paper = match self.paper.as_deref() {
            None | Some("plain") => "plain",
            Some("ruled") => "ruled",
            Some(other) => {
                return Err(format!(
                    "paper must be plain or ruled, got {}",
                    bounded(other, 40)
                ));
            }
        };
        let seed = match self.seed {
            None => 1985,
            Some(s) if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&s) => s,
            Some(s) => {
                return Err(format!(
                    "seed must be a whole number from {} to {}, got {s}",
                    i32::MIN,
                    i32::MAX
                ));
            }
        };
        let ink = match self.ink.as_deref() {
            None => "0.13 0.14 0.34".to_string(),
            Some(ink) => ink_token(ink)?,
        };
        Ok(Note {
            text: escape_text(&self.text),
            size,
            size_token: number_token(size),
            jitter_token: number_token(jitter),
            ink_token: ink,
            seed_token: seed.to_string(),
            paper,
            width: self.width.unwrap_or(612),
            dpi: self.dpi.unwrap_or(72.0),
        })
    }
}

/// The text as a PostScript string literal's contents: ASCII lowercased (the
/// face has no capitals), line endings made `\n` (a bare CR is a newline in
/// a literal anyway), `\`, `(` and `)` escaped, trailing newlines dropped.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.replace("\r\n", "\n").chars() {
        match c {
            '\r' => out.push('\n'),
            '\\' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c.to_ascii_lowercase()),
        }
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// The page height in points that holds `lines` lines: two margins, a first
/// line's 1.05 em, and a leading step for each further line, rounded up.
/// The plugin's arithmetic, in the same order.
fn auto_height(lines: u64, size: f64) -> f64 {
    let n = lines.max(1) as f64;
    (2.0 * MARGIN + 1.05 * size + (n - 1.0) * LEADING * size).ceil()
}

impl Note {
    /// The program that sets the library up and fills the options
    /// dictionary for a page `height` points tall; `tail` follows it.
    fn program(&self, height: &str, tail: &str) -> String {
        format!(
            "(lib/handscript.ps) run\n\
             /HandOpts <<\n  /Text ({})\n  /Size {}\n  /Width {}\n  /Height {height}\n  \
             /Margin {MARGIN}\n  /Leading {LEADING}\n  /Jitter {}\n  /Pen {PEN}\n  /Ink [ {} ]\n  \
             /Paper /{}\n  /Seed {}\n>> def\n{tail}\n",
            self.text,
            self.size_token,
            self.width,
            self.jitter_token,
            self.ink_token,
            self.paper,
            self.seed_token
        )
    }

    /// The measuring program: prints the number of lines the text wraps to.
    fn measuring_program(&self) -> String {
        // The wrap does not depend on the height, so a placeholder is fine.
        self.program("792", "HandOpts hs-linecount =")
    }

    /// The rendering program for a page of `height` points.
    fn rendering_program(&self, height: u32) -> String {
        self.program(&height.to_string(), "HandOpts hs-write showpage")
    }

    /// Worker side: measure the text, size the page, check the page against
    /// the budget, and write the program. See the module documentation.
    pub fn generate(&self, cx: &GenerateContext<'_>) -> Result<GeneratedProgram, GenerateError> {
        let Some(budget) = cx.decided else {
            return Err(GenerateError::Internal(
                "a handwrite job needs a height budget".into(),
            ));
        };
        let (lines, spent) = self.count_lines(cx)?;
        let height_pt = auto_height(lines, self.size);
        // Before the page is allocated: the side limit, the pixel cap, and
        // the point limit of the plugin's page.
        let h = height_pt.min(f64::from(u32::MAX)) as u32;
        if let Err(refusal) = budget.admits(h, self.dpi) {
            return Err(GenerateError::Input(refusal.failure(
                "the note",
                &format!("{lines} lines"),
                "Shorten the text, make the note wider, or lower size or dpi",
            )));
        }
        if height_pt > f64::from(MAX_PAGE_POINTS) {
            return Err(GenerateError::Input(GenerateFailure {
                code: GenerateErrorCode::InvalidDimensions,
                detail: "height".into(),
                token: None,
                message: format!(
                    "the note ({lines} lines) would be {height_pt} points tall; a page is at \
                     most {MAX_PAGE_POINTS} points. Shorten the text or lower size"
                ),
            }));
        }
        Ok(GeneratedProgram {
            source: self.rendering_program(h),
            page_points: (self.width, h),
            dpi: self.dpi,
            transparent: false,
            spent,
        })
    }

    /// Run the measuring pass under the worker's interpreter limits.
    fn count_lines(&self, cx: &GenerateContext<'_>) -> Result<(u64, Spent), GenerateError> {
        let mut interp = pscat::Interp::new();
        crate::worker::apply_limits(&mut interp, cx.limits).map_err(GenerateError::Internal)?;
        let started = Instant::now();
        let run = interp.run_source(self.measuring_program().as_bytes());
        let spent = Spent {
            steps: interp.steps_used(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        if let Err(err) = run {
            return Err(GenerateError::PostScript(Box::new(PsDiagnostic {
                name: err.name().to_string(),
                limit: err.limit().map(|l| l.name().to_string()),
                report: interp.error_report(&err),
            })));
        }
        let printed = String::from_utf8_lossy(interp.captured_stdout()).into_owned();
        let lines = printed.trim().parse::<u64>().map_err(|_| {
            GenerateError::Internal(format!(
                "could not count lines (hs-linecount gave: {})",
                bounded(&printed, 40)
            ))
        })?;
        Ok((lines, spent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(text: &str) -> HandwriteParams {
        HandwriteParams {
            text: text.into(),
            size: None,
            width: None,
            paper: None,
            ink: None,
            jitter: None,
            seed: None,
            dpi: None,
        }
    }

    #[test]
    fn the_height_is_the_plugins() {
        // 2*54 + 1.05*36 + 0*1.5*36 = 145.8 -> 146
        assert_eq!(auto_height(1, 36.0), 146.0);
        assert_eq!(auto_height(0, 36.0), 146.0);
        // 2*54 + 37.8 + 2*54 = 253.8 -> 254
        assert_eq!(auto_height(3, 36.0), 254.0);
    }

    #[test]
    fn the_text_is_lowercased_escaped_and_trimmed() {
        assert_eq!(
            escape_text("Hi (There)\\\r\nOK\n\n"),
            "hi \\(there\\)\\\\\nok"
        );
        assert_eq!(escape_text("a\rb"), "a\nb");
        assert_eq!(escape_text("Ünï ΩK"), "Ünï Ωk");
    }

    #[test]
    fn numbers_are_written_as_the_library_expects() {
        assert_eq!(number_token(36.0), "36");
        assert_eq!(number_token(36.5), "36.5");
        assert_eq!(number_token(-0.0), "0");
        assert_eq!(number_token(0.0000001), "0.0000001");
    }

    #[test]
    fn arguments_are_checked() {
        let err = |p: HandwriteParams| p.resolve().unwrap_err();
        let with = |f: &dyn Fn(&mut HandwriteParams)| {
            let mut p = params("hi");
            f(&mut p);
            p
        };
        assert!(err(params("")).contains("text is empty"));
        assert!(err(params(" \n\n")).contains("text is empty"));
        assert!(err(params("你好")).contains("no character"));
        assert!(err(with(&|p| p.size = Some(2001.0))).contains("size must be"));
        assert!(err(with(&|p| p.size = Some(-1.0))).contains("size must be"));
        assert!(err(with(&|p| p.jitter = Some(1e9))).contains("jitter must be"));
        assert!(err(with(&|p| p.seed = Some(1 << 40))).contains("seed must be"));
        assert!(err(with(&|p| p.paper = Some("lined".into()))).contains("plain or ruled"));
        for bad in [
            "1,2",
            "1,2,3,4",
            "1,2,3 ] def (x) =",
            "a,b,c",
            "",
            "1,,2",
            "255,0,0",
            "-1,0,0",
            "1.2.3,0,0",
            "1e0,0,0",
            ".,0,0",
        ] {
            let e = err(with(&|p| p.ink = Some(bad.into())));
            assert!(e.contains("ink must be R,G,B"), "{bad:?}: {e}");
        }
        let note = |ink: &str| {
            let mut p = params("hi");
            p.ink = Some(ink.into());
            p.resolve().unwrap().ink_token
        };
        assert_eq!(note("1,0,.5"), "1 0 .5");
        assert_eq!(note(" 0.1, 0.2 ,0.3"), "0.1 0.2 0.3");
        assert_eq!(params("hi").resolve().unwrap().ink_token, "0.13 0.14 0.34");
    }

    #[test]
    fn the_programs_carry_the_options_and_nothing_else() {
        let mut p = params("Hello (World)");
        p.size = Some(40.0);
        p.paper = Some("ruled".into());
        p.seed = Some(-3);
        let n = p.resolve().unwrap();
        let m = n.measuring_program();
        assert!(
            m.starts_with("(lib/handscript.ps) run\n/HandOpts <<\n  /Text (hello \\(world\\))\n")
        );
        assert!(m.contains("/Size 40\n  /Width 612\n  /Height 792\n"));
        assert!(m.contains("/Paper /ruled\n  /Seed -3\n>> def\nHandOpts hs-linecount =\n"));
        let r = n.rendering_program(300);
        assert!(r.contains("/Height 300\n") && r.ends_with("HandOpts hs-write showpage\n"));
    }
}
