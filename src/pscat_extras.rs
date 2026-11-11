//! The one place this crate calls PSCat APIs that are `#[doc(hidden)]`.
//!
//! PSCat exports these modules only so its own binaries and tests can reach
//! them; they are not part of its documented API, so they may change in any
//! release. Wrapping every use here means a PSCat bump that breaks one is
//! fixed in this file alone. Tracking issue for a supported API:
//! <https://github.com/PSCatLabs/pscat/issues/1098>.
//!
//! Hidden APIs in use:
//!
//! * `pscat::lexer` ([`syntax_scan`]), the tokenizer pass of
//!   `validate_postscript` (PSCatLabs/pscat#1096).
//! * `pscat::gfx::PageSizeSource` ([`allow_program_page_size`]), so a
//!   program's `setpagedevice` may replace a default-sized page
//!   (PSCatLabs/pscat#1097).
//! * `pscat::capabilities` ([`capabilities_json`]), the catalog behind
//!   `pscat --capabilities` and the plugin's `describe_art_capabilities`.
//!
//! * `pscat::halftone`, `pscat::png` and the page rasters of `pscat::gfx`
//!   (`Interp::gfx`, `Gfx::pages`, `Gfx::pixmap`, `Gfx::has_trailing_art`)
//!   ([`halftone_pngs`]), the screening behind `pscat --halftone` and the
//!   plugin's `halftone` option.
//! * `pscat::chalk` ([`ChalkBoard`], [`chalk_explain`], [`CHALK_COLORS`]),
//!   the LaTeX-to-chalkboard generator behind `pscat chalk` and the plugin's
//!   `chalkboard_math`; plus `Gfx::set_transparent_background` and
//!   `pscat::pdf::scan_document_info` with `Gfx::set_pdf_info`
//!   ([`set_transparent_background`], [`set_pdf_info_from_source`]), which a
//!   generated board needs the way `pscat chalk` uses them.
//! * `pscat::contact_sheet` ([`SheetLayout`], [`sheet_text`], [`SheetImage`],
//!   [`SheetCanvas`]) and `pscat::png::encode`, the layout, label strips,
//!   zoom, hairline frames and PNG behind `pscat sheet` and the plugin's
//!   `review_sheet`; plus the page rasters of `pscat::gfx` again
//!   ([`render_sheet_cell`]: a cell is its program's last page).
//!
//! Nothing outside this module may name a `#[doc(hidden)]` PSCat item.
//! Everything here runs inside the worker process, with two exceptions:
//!
//! * [`ChalkBoard::from_args`] and [`ChalkBoard::page`] also run in the
//!   server, before a job is started, to refuse a bad board config and to
//!   size the board for the pixel limits. They read only the board config (a
//!   bounded JSON object of fixed depth, already parsed by rmcp), never the
//!   LaTeX.
//! * [`SheetLayout::new`] also runs in the server, to size a sheet against
//!   its budgets before any worker starts. It is arithmetic on sizes (at
//!   most the cell cap's worth of them, checked first), with the label
//!   strips at an upper bound computed from the label's length; no text is
//!   laid out and no pixel is allocated.

use serde_json::{Map, Value, json};

use crate::worker::SyntaxScan;

/// Scan `source` with PSCat's tokenizer without executing anything, and
/// check that procedure braces balance (the tokenizer reports `{`/`}` as
/// separate tokens and leaves balancing to the interpreter).
pub fn syntax_scan(source: &[u8]) -> SyntaxScan {
    let mut lexer = pscat::lexer::Lexer::new(source.to_vec());
    let mut tokens = 0usize;
    let mut depth = 0usize;
    loop {
        match lexer.next_token() {
            Ok(Some(token)) => {
                tokens += 1;
                match token {
                    pscat::lexer::Token::LBrace => depth += 1,
                    pscat::lexer::Token::RBrace => {
                        if depth == 0 {
                            return SyntaxScan {
                                ok: false,
                                tokens,
                                error: Some(format!("unmatched '}}' (token {tokens})")),
                            };
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                let error = (depth > 0).then(|| {
                    format!("unterminated procedure: {depth} '{{' without a matching '}}'")
                });
                return SyntaxScan {
                    ok: error.is_none(),
                    tokens,
                    error,
                };
            }
            Err(e) => {
                return SyntaxScan {
                    ok: false,
                    tokens,
                    error: Some(e.to_string()),
                };
            }
        }
    }
}

/// Mark `interp`'s page size as a default that nothing chose, like
/// `Interp::new()` does, so the program's `setpagedevice` may replace it
/// (bounded by `max_page_pixels`).
pub fn allow_program_page_size(interp: &mut pscat::Interp) {
    interp
        .gfx_mut()
        .set_page_size_source(pscat::gfx::PageSizeSource::Default);
}

/// One halftoned page, encoded as PNG.
pub struct HalftonePng {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
    /// The PNG file bytes (with the page's `pHYs` resolution chunk).
    pub png: Vec<u8>,
}

/// Every page `interp` produced, screened like a mono laser printer and
/// encoded as PNG, lazily, one page at a time. The pages are the ones
/// `Interp::pages()` returns, in the same order: each `showpage`, plus the
/// canvas when art was painted after the last one (or when there was none),
/// as `pscat --halftone --png` writes them. The screen is PSCat's default
/// (45 degree clustered dot, pure black and white pixels). `scale` is the
/// interpreter's device pixels per point (`dpi / 72`), recorded in each PNG's
/// resolution chunk.
pub fn halftone_pngs(
    interp: &pscat::Interp,
    scale: f32,
) -> impl Iterator<Item = std::io::Result<HalftonePng>> + '_ {
    let gfx = interp.gfx();
    let mut pages: Vec<_> = gfx.pages().iter().collect();
    if pages.is_empty() || gfx.has_trailing_art() {
        pages.push(&gfx.pixmap);
    }
    pages.into_iter().map(move |page| {
        let screened = pscat::halftone::screen(page);
        pscat::png::encode(&screened, scale).map(|png| HalftonePng {
            width: screened.width(),
            height: screened.height(),
            png,
        })
    })
}

/// The art catalog of the linked PSCat build, as `pscat --capabilities`
/// prints it: `pscat_version`, `catalog_signature`, `font_packs` and a
/// `capabilities` array (fonts, Type 3 faces, palettes, page templates,
/// artkit and style-pack procedures, native operators).
///
/// Fonts are listed by scanning font directories on disk, so call this in
/// the process whose environment renders use (the worker). Absolute
/// filesystem paths in the font-pack status are removed, so the reply does
/// not disclose where the server keeps its files.
pub fn capabilities_json() -> Value {
    let mut payload = pscat::capabilities::payload_json();
    scrub_paths(&mut payload);
    payload
}

/// Remove every `path` key from `font_packs.installed[]` and
/// `font_packs.overrides[]`; those are the only host paths in the catalog.
fn scrub_paths(payload: &mut Value) {
    let Some(packs) = payload.get_mut("font_packs") else {
        return;
    };
    for section in ["installed", "overrides"] {
        if let Some(Value::Array(items)) = packs.get_mut(section) {
            for item in items {
                if let Value::Object(map) = item {
                    map.remove("path");
                }
            }
        }
    }
}

/// Make the page background transparent instead of white (PNG alpha, no
/// SVG backdrop). Call after `enable_svg`/`enable_pdf` and before anything is
/// painted, as `pscat chalk` does for a board without a board.
pub fn set_transparent_background(interp: &mut pscat::Interp) {
    interp.gfx_mut().set_transparent_background();
}

/// Set the PDF document's title and author from the program's DSC header
/// comments (`%%Title:`, `%%For:`), as the `pscat` CLI and `pscat chalk` do.
pub fn set_pdf_info_from_source(interp: &mut pscat::Interp, source: &[u8]) {
    let (title, author) = pscat::pdf::scan_document_info(source);
    interp.gfx_mut().set_pdf_info(title, author);
}

/// The chalk colours a board can be written in (`pscat::chalk::CHALK_COLORS`).
pub const CHALK_COLORS: [&str; 6] = pscat::chalk::CHALK_COLORS;

/// The flat `chalkboard_math` arguments that map onto one key of the board
/// config: (argument, group, key). The same table as the plugin's.
const CHALK_FLAT_KEYS: [(&str, &str, &str); 9] = [
    ("size", "layout", "size"),
    ("irregular", "layout", "irregular"),
    ("slope", "layout", "slope"),
    ("jitter", "layout", "jitter"),
    ("align", "layout", "align"),
    ("width", "layout", "width"),
    ("height", "layout", "height"),
    ("dpi", "layout", "dpi"),
    ("color", "chalk", "color"),
];

/// Why a board could not be made: `pscat::chalk::ChalkError`, flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct ChalkFailure {
    /// The input is wrong in a way no run is needed to see: a config key or
    /// value, or no formula at all. False for a TeX error found while the
    /// program ran (or while its rows were split) and for a render failure.
    pub invalid_input: bool,
    /// PSCat's stable code: `config`, `empty`, `render`, or the LaTeX
    /// error's kind (`unsupported_command`, `missing_argument`, ...).
    pub code: String,
    /// The command, character or environment a LaTeX error named.
    pub token: Option<String>,
    /// One line for the person or agent that made the request.
    pub message: String,
}

impl From<pscat::chalk::ChalkError> for ChalkFailure {
    fn from(e: pscat::chalk::ChalkError) -> Self {
        use pscat::chalk::ChalkError;
        let code = e.code();
        let (invalid_input, token, message) = match &e {
            ChalkError::Config(_) | ChalkError::Empty => (true, None, e.to_string()),
            // PSCat's text points at `pscat chalk --help`, which a client of
            // this server cannot run.
            ChalkError::Latex { kind, token } if kind == "unsupported-command" => (
                false,
                Some(token.clone()),
                format!(
                    "unsupported LaTeX command {token}: the supported subset is PSCat mathkit's \
                     (fractions, roots, sums, integrals, Greek letters, matrices, cases, accents)"
                ),
            ),
            ChalkError::Latex { token, .. } => (
                false,
                Some(token.clone()).filter(|t| !t.is_empty()),
                e.to_string(),
            ),
            ChalkError::Render(_) => (false, None, e.to_string()),
        };
        ChalkFailure {
            invalid_input,
            code,
            token,
            message,
        }
    }
}

/// A PostScript error raised by a generated board, as the TeX error it
/// stands for: mathkit reports one as `undefined` naming
/// `mk-<kind>:<token>`. `None` for any other error.
pub fn chalk_explain(err: &pscat::PsError) -> Option<ChalkFailure> {
    pscat::chalk::explain(err).map(ChalkFailure::from)
}

/// A generated board program and its page.
#[derive(Debug, Clone)]
pub struct ChalkDriver {
    /// The PostScript program.
    pub source: String,
    /// Page size in points.
    pub page_points: (u32, u32),
    /// Resolution.
    pub dpi: f32,
    /// The page background is transparent (`board: false`).
    pub transparent: bool,
}

/// A resolved board config (`pscat::chalk::Config`): everything about a
/// board but the LaTeX.
#[derive(Debug, Clone)]
pub struct ChalkBoard(pscat::chalk::Config);

impl ChalkBoard {
    /// The board the `chalkboard_math` arguments in `args` describe, resolved
    /// as the plugin does: `config` (the `pscat chalk --json` schema) first,
    /// then the flat keys over it, then `board`, `ghosting` and `seed`. An
    /// error is a config message that names the argument at fault. Keys of
    /// `args` other than these are ignored (the caller passes only these).
    pub fn from_args(args: &Map<String, Value>) -> Result<ChalkBoard, String> {
        use pscat::chalk::{ChalkError, Config};
        let present = |key: &str| args.get(key).filter(|v| !v.is_null());
        let mut cfg = match present("config") {
            None => Config::default(),
            Some(v @ Value::Object(_)) => Config::from_value(v).map_err(|e| e.to_string())?,
            Some(_) => return Err("config must be an object".into()),
        };
        // `path` is how the config reader names the key; the message says `arg`.
        fn overlay(cfg: &mut Config, arg: &str, path: &str, v: Value) -> Result<(), String> {
            cfg.apply(&v).map_err(|e| match e {
                ChalkError::Config(m) => m.replace(path, arg),
                e => e.to_string(),
            })
        }
        for (arg, group, key) in CHALK_FLAT_KEYS {
            if let Some(v) = present(arg) {
                overlay(
                    &mut cfg,
                    arg,
                    &format!("{group}.{key}"),
                    json!({ group: { key: v } }),
                )?;
            }
        }
        if let Some(b) = present("board") {
            match b {
                Value::String(_) | Value::Array(_) => overlay(
                    &mut cfg,
                    "board",
                    "board.tone",
                    json!({ "board": { "tone": b } }),
                )?,
                Value::Bool(_) | Value::Object(_) => {
                    overlay(&mut cfg, "board", "board", json!({ "board": b }))?
                }
                _ => {
                    return Err(format!(
                        "board must be true, false, \"slate\", \"greenboard\", [r, g, b] or an \
                         object, got {b}"
                    ));
                }
            }
        }
        if let Some(g) = present("ghosting") {
            // Laid over no board, the config reader would bring a default one
            // back: refuse the contradiction instead.
            if cfg.board.is_none() {
                return Err("ghosting needs a board; drop it or the board: false".into());
            }
            overlay(
                &mut cfg,
                "ghosting",
                "board.ghosting",
                json!({ "board": { "ghosting": g } }),
            )?;
        }
        if let Some(s) = present("seed") {
            overlay(&mut cfg, "seed", "seed", json!({ "seed": s }))?;
        }
        cfg.check().map_err(|e| e.to_string())?;
        Ok(ChalkBoard(cfg))
    }

    /// The board's page in points and its resolution.
    pub fn page(&self) -> ((u32, u32), f32) {
        ((self.0.width, self.0.height), self.0.dpi)
    }

    /// The PostScript program that writes `latex` on this board. Splits the
    /// LaTeX into rows (its wrappers, delimiters and row breaks); the TeX
    /// itself is laid out when the program runs. Worker only: the LaTeX is
    /// untrusted.
    pub fn driver(&self, latex: &str) -> Result<ChalkDriver, ChalkFailure> {
        let d = pscat::chalk::driver(latex, &self.0)?;
        Ok(ChalkDriver {
            source: d.source,
            page_points: (d.width, d.height),
            dpi: d.dpi,
            transparent: d.transparent,
        })
    }
}

// --- review_sheet: pscat::contact_sheet --------------------------------

/// Most pixels on one side of a sheet or of one (zoomed) cell
/// (`pscat::contact_sheet::MAX_SIDE_PX`).
pub const SHEET_MAX_SIDE_PX: u32 = pscat::contact_sheet::MAX_SIDE_PX;

/// One cell's footprint, for [`SheetLayout::new`]: the image as shown (after
/// any zoom) and its caption strip, `(0, 0)` for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SheetCellSize {
    /// The image in pixels.
    pub image: (u32, u32),
    /// The caption strip in pixels.
    pub caption: (u32, u32),
}

/// One row, for [`SheetLayout::new`]: its label strip (`(0, 0)` for none)
/// and its cells, left to right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetRowSize {
    /// The label strip in pixels.
    pub header: (u32, u32),
    /// The cells.
    pub cells: Vec<SheetCellSize>,
}

/// Where everything goes on a review sheet (`pscat::contact_sheet::Layout`,
/// the geometry `pscat sheet` and the plugin's `review_sheet` use). Pure
/// arithmetic on sizes: it allocates no pixels.
#[derive(Debug)]
pub struct SheetLayout(pscat::contact_sheet::Layout);

impl SheetLayout {
    /// Place `rows`. `Err` names the size when the sheet would be over
    /// [`SHEET_MAX_SIDE_PX`] on a side (`contact sheet would be WxHpx, over
    /// the 8000px-per-side limit`); nothing is allocated either way.
    pub fn new(rows: &[SheetRowSize]) -> Result<SheetLayout, String> {
        use pscat::contact_sheet::{CellSize, Layout, RowSize};
        let rows: Vec<RowSize> = rows
            .iter()
            .map(|r| RowSize {
                header: r.header,
                cells: r
                    .cells
                    .iter()
                    .map(|c| CellSize {
                        image: c.image,
                        caption: c.caption,
                    })
                    .collect(),
            })
            .collect();
        // PSCat's message ends with advice for its CLI flags; keep the size.
        Layout::new(&rows)
            .map(SheetLayout)
            .map_err(|m| m.split(" -- ").next().unwrap_or_default().to_string())
    }

    /// The sheet's size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    /// Top-left of row `r`'s label strip.
    pub fn header_xy(&self, r: usize) -> (u32, u32) {
        self.0.header_xy[r]
    }

    /// Top-left of the image of row `r`, cell `c` (inside its hairline).
    pub fn image_xy(&self, r: usize, c: usize) -> (u32, u32) {
        self.0.image_xy[r][c]
    }

    /// Top-left of the caption of row `r`, cell `c`.
    pub fn caption_xy(&self, r: usize, c: usize) -> (u32, u32) {
        self.0.caption_xy[r][c]
    }
}

/// A strip of text (a row label or a caption), drawn by
/// [`sheet_text`].
pub struct SheetStrip(tiny_skia::Pixmap);

impl SheetStrip {
    /// Its size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.0.width(), self.0.height())
    }
}

/// `text` as a tight, opaque strip, `px` pixels tall type, bold or not,
/// set by PSCat's own interpreter and fonts with the face picked per
/// character by script (`pscat::contact_sheet::render_text`): Latin
/// (accents included) in Helvetica, Korean, Japanese, Chinese and Thai in
/// the `cjk-thai` font pack's faces, `?` for a character no installed face
/// has. `None` for an empty text. Worker only: the label is untrusted, and
/// laying it out runs PostScript (a program PSCat writes, with the text as
/// an escaped string literal) on an interpreter without limits; the text is
/// at most a few hundred characters, and the worker's deadline and rlimits
/// bound it.
pub fn sheet_text(text: &str, px: u32, bold: bool) -> Result<Option<SheetStrip>, String> {
    pscat::contact_sheet::render_text(text, px, bold).map(|p| p.map(SheetStrip))
}

/// One cell's image: the program's last page, or a blank stand-in.
pub struct SheetImage(tiny_skia::Pixmap);

impl SheetImage {
    /// Its size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.0.width(), self.0.height())
    }

    /// A white image of `w`x`h` pixels, for a cell whose program left none
    /// that can be shown. `None` for an unusable size.
    pub fn blank(w: u32, h: u32) -> Option<SheetImage> {
        let mut p = tiny_skia::Pixmap::new(w, h)?;
        p.fill(tiny_skia::Color::WHITE);
        Some(SheetImage(p))
    }

    /// This image enlarged `k` times with nearest-neighbour sampling (every
    /// pixel a `k`x`k` block; `pscat::contact_sheet::zoom_nearest`). `None`
    /// past [`SHEET_MAX_SIDE_PX`] on a side.
    pub fn zoomed(&self, k: u32) -> Option<SheetImage> {
        pscat::contact_sheet::zoom_nearest(&self.0, k).map(SheetImage)
    }
}

/// What one cell's program left.
pub struct SheetCellRender {
    /// The last page, as `pscat sheet` takes it: a program that shows
    /// several pages contributes its final one, one that fails the page as
    /// far as it got.
    pub image: SheetImage,
    /// The error the program raised or the limit it passed, if any.
    pub error: Option<crate::worker::PsDiagnostic>,
    /// Interpreter steps.
    pub steps: u64,
    /// Milliseconds the program ran.
    pub elapsed_ms: u64,
}

/// Run one cell's program on a fresh `page` (points) at `scale` device
/// pixels per point, under `limits` and the worker's sandbox
/// ([`crate::worker::apply_limits`]), on the calling thread, as `pscat
/// sheet` renders a cell. Worker only. `Err` when the page or the limits
/// cannot be set up (not the program's fault).
pub fn render_sheet_cell(
    source: &[u8],
    page: (u32, u32),
    scale: f32,
    limits: &crate::worker::InterpLimits,
) -> Result<SheetCellRender, String> {
    let mut interp = pscat::Interp::with_page_scaled(page.0, page.1, scale).ok_or_else(|| {
        format!(
            "a {}x{} pt page at scale {scale} is unusable",
            page.0, page.1
        )
    })?;
    crate::worker::apply_limits(&mut interp, limits)?;
    let started = std::time::Instant::now();
    let run = interp.run_source(source);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let error = run.err().map(|err| crate::worker::PsDiagnostic {
        name: err.name().to_string(),
        limit: err.limit().map(|l| l.name().to_string()),
        report: interp.error_report(&err),
    });
    let gfx = interp.gfx();
    let last = match gfx.pages().last() {
        Some(page) if !gfx.has_trailing_art() => page.clone(),
        _ => gfx.pixmap.clone(),
    };
    Ok(SheetCellRender {
        image: SheetImage(last),
        error,
        steps: interp.steps_used(),
        elapsed_ms,
    })
}

/// The sheet being composed: white, the size of its [`SheetLayout`].
pub struct SheetCanvas(tiny_skia::Pixmap);

impl SheetCanvas {
    /// A blank white sheet for `layout`. The caller has checked its size
    /// against the budget; this allocates it.
    pub fn new(layout: &SheetLayout) -> Result<SheetCanvas, String> {
        layout.0.new_sheet().map(SheetCanvas)
    }

    /// Copy `strip` with its top-left at `at`.
    pub fn put_strip(&mut self, at: (u32, u32), strip: &SheetStrip) {
        pscat::contact_sheet::blit_at(&mut self.0, at.0, at.1, &strip.0);
    }

    /// Draw the hairline frame around where `image` goes, then copy it with
    /// its top-left at `at`.
    pub fn put_image(&mut self, at: (u32, u32), image: &SheetImage) {
        let (w, h) = image.size();
        pscat::contact_sheet::frame(&mut self.0, at.0, at.1, w, h);
        pscat::contact_sheet::blit_at(&mut self.0, at.0, at.1, &image.0);
    }

    /// The sheet as a PNG, recording 72 dpi (cells of any dpi share it), as
    /// `pscat sheet` writes it.
    pub fn to_png(&self) -> std::io::Result<Vec<u8>> {
        pscat::png::encode(&self.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_removes_only_paths() {
        let mut v = json!({
            "font_packs": {
                "installed": [{"name": "a", "path": "/srv/x", "status": "pinned"}],
                "overrides": [{"path": "/srv/y", "status": "user-selected"}],
                "available": ["a"]
            },
            "capabilities": []
        });
        scrub_paths(&mut v);
        assert_eq!(
            v,
            json!({
                "font_packs": {
                    "installed": [{"name": "a", "status": "pinned"}],
                    "overrides": [{"status": "user-selected"}],
                    "available": ["a"]
                },
                "capabilities": []
            })
        );
    }

    fn board(args: Value) -> Result<ChalkBoard, String> {
        ChalkBoard::from_args(args.as_object().unwrap())
    }

    #[test]
    fn chalk_board_defaults_and_overlays() {
        let b = board(json!({})).unwrap();
        assert_eq!(b.page(), ((800, 400), 144.0));
        let b =
            board(json!({"config": {"layout": {"width": 400}}, "height": 300, "dpi": 72})).unwrap();
        assert_eq!(b.page(), ((400, 300), 72.0));
        // a flat key wins over config
        let b = board(json!({"config": {"layout": {"width": 400}}, "width": 500})).unwrap();
        assert_eq!(b.page().0, (500, 400));
        for ok in [
            json!({"board": false}),
            json!({"board": true}),
            json!({"board": "greenboard"}),
            json!({"board": [0.1, 0.2, 0.3]}),
            json!({"board": {"haze": 0.1, "swipes": 0}}),
            json!({"ghosting": 0}),
            json!({"ghosting": false}),
            json!({"seed": -5, "color": "yellow", "align": "left"}),
        ] {
            board(ok.clone()).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
    }

    #[test]
    fn chalk_board_errors_name_the_argument() {
        let err = |args: Value| board(args).unwrap_err();
        assert!(
            err(json!({"size": 0})).contains("size must be"),
            "{}",
            err(json!({"size": 0}))
        );
        assert!(err(json!({"color": "red"})).starts_with("color must be one of"));
        assert!(err(json!({"board": "chalky"})).contains("board must be"));
        assert!(err(json!({"board": 3})).contains("board must be true, false"));
        assert!(err(json!({"board": false, "ghosting": 0.5})).contains("ghosting needs a board"));
        assert!(err(json!({"config": {"nope": 1}})).contains("unknown key nope"));
        assert!(err(json!({"config": {"layout": {"width": 9000}}})).contains("layout.width"));
        assert!(err(json!({"config": 3})).contains("config must be an object"));
        assert!(err(json!({"seed": 2_000_000_000_i64})).contains("seed must be"));
        assert!(err(json!({"width": 50, "height": 50})).contains("no room to write"));
    }

    #[test]
    fn chalk_colours_are_pscats() {
        assert_eq!(
            CHALK_COLORS,
            ["white", "yellow", "pink", "blue", "green", "orange"]
        );
    }

    #[test]
    fn syntax_scan_checks_braces() {
        assert!(syntax_scan(b"{ 1 2 add } exec").ok);
        assert!(!syntax_scan(b"{ 1").ok);
        assert!(!syntax_scan(b"1 }").ok);
    }
}
