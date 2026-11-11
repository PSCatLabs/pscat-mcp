//! `review_sheet`: several programs, each at several sizes, composed into
//! one labelled PNG.
//!
//! Each entry is a row and each item of its dpi list a cell, drawn at its
//! own true pixel size (a 48 px render stays 48 px beside a 512 px one),
//! centred in its column with a hairline around it, the row label above and
//! a pixel-size caption under each cell. An item `{"dpi": 9, "zoom": 4}`
//! draws the 9 dpi render enlarged 4x with nearest-neighbour sampling. The
//! layout, the label and caption strips, the zoom and the frames are PSCat's
//! own (`pscat::contact_sheet`, which `pscat sheet` and the plugin's
//! `review_sheet` use), so the same programs, dpis and labels give the same
//! sheet. Arguments, defaults and caps are the plugin's
//! (`src/mcp_server.rs` in the pinned PSCat checkout).
//!
//! **Where things run.** The programs and the labels are untrusted, so all
//! of the work is one worker job ([`compose`]): the label strips, the exact
//! layout, every cell, the composition and the PNG. The server only runs
//! [`plan`], which reads the arguments and checks every budget before a
//! worker starts: the number of cells, the source bytes, each cell's sides
//! (after zoom), the pixels the cells render in all, and the sheet's sides
//! and pixels, from a layout whose label and caption strips are upper
//! bounds computed from their length (no text is laid out and nothing sized
//! by the input is allocated before the counts are checked). The worker
//! draws the strips, recomputes the layout exactly, and checks the sheet
//! against the same budget again before it allocates the sheet or runs a
//! cell.
//!
//! **One job, one render slot.** A sheet takes one slot, and its cells run
//! one after another in that one worker process, each on a fresh thread
//! (8 MiB of stack, like the worker's main thread; a fresh name interner,
//! so a cell is byte for byte a standalone render) with a fresh interpreter
//! under the worker's sandbox. PSCat's memory limit measures heap growth
//! from when a cell's limits are set, so it is per cell too. The plugin
//! instead starts one `pscat` child per cell; one job keeps the concurrency
//! accounting honest (one slot is one process) and the cells' rasters in
//! the process that composes them. The cost: a crash (a stack overflow, an
//! abort) loses the whole sheet as `worker_failed`, and one long operator
//! that PSCat cannot interrupt runs into the sheet's deadline (`timeout`).
//!
//! **Time.** The worker shares [`SheetBudget::time_ms`] (the sheet deadline
//! less a margin for composing and encoding) out as it goes: each cell may
//! run for at most the render's own time limit and at most an equal share
//! of the time left (the time left divided by the cells left), and at most
//! the render's step limit. A runaway cell stops at its share
//! (`limit_exceeded`) and the cells after it still get theirs; time a fast
//! cell leaves passes to the cells after it. The plugin gives each child
//! whatever is left of the call, so one runaway cell can starve the rest.
//!
//! A cell whose program fails is drawn as far as it got, and its status
//! carries the error; the sheet itself succeeds.

use std::borrow::Cow;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LimitsConfig;
use crate::generate::{
    GenerateContext, GenerateError, GenerateErrorCode, GenerateFailure, bounded,
};
use crate::pscat_extras::{
    SHEET_MAX_SIDE_PX, SheetCanvas, SheetCellSize, SheetImage, SheetLayout, SheetRowSize,
    SheetStrip, render_sheet_cell, sheet_text,
};
use crate::render::ErrorCode;
use crate::worker::{InterpLimits, PsDiagnostic};

/// Longest row label, in characters (the plugin's `MAX_SHEET_LABEL`).
pub const MAX_LABEL_CHARS: usize = 200;
/// Largest zoom (the plugin's `MAX_SHEET_ZOOM`, `pscat sheet --dpi 9x16`).
pub const MAX_ZOOM: u32 = 16;
/// Smallest and largest dpi of a cell (the plugin's).
pub const MIN_CELL_DPI: f64 = 9.0;
/// Largest dpi of a cell.
pub const MAX_CELL_DPI: f64 = 1200.0;
/// Largest page side in points (the plugin's).
pub const MAX_PAGE_POINTS: u32 = 8000;
/// Row label type size in pixels (`pscat sheet`'s).
const LABEL_PX: u32 = 16;
/// Caption type size in pixels (`pscat sheet`'s).
const CAPTION_PX: u32 = 12;
/// The page of an entry that gives none and of a sheet that gives none.
const DEFAULT_PAGE: (u32, u32) = (612, 792);
/// The least time worth giving a cell, in milliseconds (the plugin's
/// `MIN_CELL_BUDGET`). A sheet whose cells cannot each have it is refused
/// before it starts; one that runs short of it stops with `timeout`.
pub const MIN_CELL_MS: u64 = 100;
/// Longest error message kept per cell, in bytes, so that 32 failed cells
/// still fit a reply of about 24 KB.
const MAX_CELL_MESSAGE_BYTES: usize = 240;
/// Stack of a cell's thread: 8 MiB, the macOS main thread's, as `pscat
/// sheet` gives each cell.
const CELL_STACK_BYTES: usize = 8 << 20;

/// Upper bound of a label strip's width per character, in ems, for the
/// plan's layout. PSCat's widest glyphs in the faces a label can use are one
/// em (bold `Æ`, CJK ideographs; the tests in this module measure them);
/// the worker checks the exact layout anyway.
const LABEL_WIDTH_EM: f64 = 1.1;
/// The same for a caption, which is only digits, `x`, ` px` and ` (shown
/// at Nx)` in regular Helvetica (`w`, the widest, is 0.72 em).
const CAPTION_WIDTH_EM: f64 = 0.75;
/// Height of a strip, in ems: PSCat's is 1.3 for Latin and 1.4 with a
/// Korean, Japanese, Chinese or Thai run.
const STRIP_HEIGHT_EM: f32 = 1.4;
/// The height of a caption strip (always Latin: `48x48 px (shown at 4x)`).
const CAPTION_HEIGHT_EM: f32 = 1.3;

/// The `review_sheet` arguments, as the tool received them. Travels in the
/// worker job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SheetParams {
    /// The rows, top to bottom.
    pub entries: Vec<SheetEntryParams>,
    /// Page width in points for entries that give none. Default 612.
    #[serde(default)]
    pub width: Option<u32>,
    /// Page height in points for entries that give none. Default 792.
    #[serde(default)]
    pub height: Option<u32>,
    /// The cells of entries that give no list. Default `[72]`.
    #[serde(default)]
    pub dpi: Option<Value>,
    /// Draw the row labels. Default true.
    #[serde(default)]
    pub labels: Option<bool>,
    /// Draw the pixel-size caption under each cell. Default true.
    #[serde(default)]
    pub sizes: Option<bool>,
}

/// One row of a sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SheetEntryParams {
    /// The PostScript program.
    pub source: String,
    /// The row label. Default `program N`, N counting from 1.
    #[serde(default)]
    pub label: Option<String>,
    /// Page width in points. Default: the sheet's.
    #[serde(default)]
    pub width: Option<u32>,
    /// Page height in points. Default: the sheet's.
    #[serde(default)]
    pub height: Option<u32>,
    /// The cells, left to right. Default: the sheet's.
    #[serde(default)]
    pub dpi: Option<Value>,
}

/// The budgets a sheet must fit, from the server's limits. Travels in the
/// worker job, which checks the exact sheet against it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheetBudget {
    /// Most cells.
    pub max_cells: usize,
    /// Most bytes of source, all entries together.
    pub max_source_bytes: usize,
    /// Most bytes of one entry's source.
    pub max_entry_source_bytes: usize,
    /// Most device pixels the cells render, all together (before zoom).
    pub max_cell_pixels: u64,
    /// Most device pixels of the sheet.
    pub max_pixels: u64,
    /// Most pixels on one side of the sheet or of a (zoomed) cell.
    pub max_side_px: u32,
    /// Milliseconds the worker shares out between the cells.
    pub time_ms: u64,
}

impl SheetBudget {
    /// The budget `limits` set.
    pub fn from_limits(limits: &LimitsConfig) -> Self {
        SheetBudget {
            max_cells: limits.max_sheet_cells,
            max_source_bytes: limits.sheet_source_bytes(),
            max_entry_source_bytes: limits.max_source_bytes,
            max_cell_pixels: limits.sheet_cell_pixels(),
            max_pixels: limits.sheet_pixels(),
            max_side_px: SHEET_MAX_SIDE_PX,
            time_ms: limits.sheet_time_budget().as_millis() as u64,
        }
    }
}

/// One cell: a dpi and a zoom (1 for none).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    /// The dpi as given, for reports.
    dpi: f64,
    /// The dpi as PSCat takes it (the plugin and `pscat sheet` parse it as
    /// `f32`, and the page scale is `dpi / 72` in `f32`).
    dpi32: f32,
    zoom: u32,
}

impl Cell {
    fn scale(&self) -> f32 {
        self.dpi32 / 72.0
    }

    /// Device pixels of `page` at this dpi, rounded as PSCat rounds them.
    fn pixels(&self, page: (u32, u32)) -> (u64, u64) {
        let px = |pt: u32| ((pt as f32 * self.scale()).round() as u64).max(1);
        (px(page.0), px(page.1))
    }
}

/// A row, checked.
#[derive(Debug)]
struct Row<'a> {
    source: &'a str,
    /// The label as given; `None` for the default.
    label: Option<&'a str>,
    page: (u32, u32),
    cells: Vec<Cell>,
}

impl Row<'_> {
    /// The label to draw for row `i`.
    fn label(&self, i: usize) -> Cow<'_, str> {
        match self.label {
            Some(l) => Cow::Borrowed(l),
            None => Cow::Owned(format!("program {}", i + 1)),
        }
    }
}

/// A sheet, checked.
#[derive(Debug)]
struct Sheet<'a> {
    rows: Vec<Row<'a>>,
    labels: bool,
    sizes: bool,
    cells: usize,
}

fn fail(code: GenerateErrorCode, detail: &str, message: String) -> GenerateFailure {
    GenerateFailure {
        code,
        detail: detail.into(),
        token: None,
        message,
    }
}

fn invalid(message: String) -> GenerateFailure {
    fail(GenerateErrorCode::InvalidRequest, "arguments", message)
}

/// How many cells a dpi list asks for, without reading its items: a
/// non-list counts one (and is refused when it is read).
fn list_len(v: Option<&Value>) -> Option<usize> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.len()),
        Some(_) => Some(1),
    }
}

/// A dpi list: `[96, {"dpi": 9, "zoom": 4}]`. The plugin's messages.
fn cells(key: &str, v: &Value) -> Result<Vec<Cell>, GenerateFailure> {
    let items = v
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid(format!("{key} must be a non-empty list of cells")))?;
    let dpi = |v: Option<&Value>| match v.and_then(Value::as_f64) {
        Some(d) if (MIN_CELL_DPI..=MAX_CELL_DPI).contains(&d) => Ok(d),
        _ => Err(fail(
            GenerateErrorCode::InvalidDimensions,
            "dpi",
            format!(
                "{key}: a dpi must be a number from 9 to 1200, got {}",
                v.map_or("nothing".to_string(), |v| bounded(&v.to_string(), 40))
            ),
        )),
    };
    let cell = |dpi: f64, zoom: u32| Cell {
        dpi,
        dpi32: dpi as f32,
        zoom,
    };
    items
        .iter()
        .map(|item| match item {
            Value::Number(_) => Ok(cell(dpi(Some(item))?, 1)),
            Value::Object(o) => {
                let zoom = match o.get("zoom") {
                    None | Some(Value::Null) => 1,
                    Some(z) => z
                        .as_u64()
                        .filter(|z| (2..=u64::from(MAX_ZOOM)).contains(z))
                        .ok_or_else(|| {
                            invalid(format!(
                                "{key}: zoom must be a whole number from 2 to {MAX_ZOOM}, got {}",
                                bounded(&z.to_string(), 40)
                            ))
                        })? as u32,
                };
                Ok(cell(dpi(o.get("dpi"))?, zoom))
            }
            other => Err(invalid(format!(
                "{key}: a cell is a dpi number or {{\"dpi\", \"zoom\"}}, got {}",
                bounded(&other.to_string(), 40)
            ))),
        })
        .collect()
}

/// A page side: 1 to 8000 points.
fn side(key: &str, v: Option<u32>, default: u32) -> Result<u32, GenerateFailure> {
    match v {
        None => Ok(default),
        Some(n) if (1..=MAX_PAGE_POINTS).contains(&n) => Ok(n),
        Some(n) => Err(fail(
            GenerateErrorCode::InvalidDimensions,
            "page",
            format!("{key} must be a whole number from 1 to {MAX_PAGE_POINTS}, got {n}"),
        )),
    }
}

impl SheetParams {
    /// Check the arguments and the budgets that need nothing drawn: the
    /// cell count and the source bytes first (counted without reading or
    /// copying anything), then the rows, then each cell's sides and the
    /// cells' pixels. Copies no source or label. Safe in the server.
    fn check(&self, budget: &SheetBudget) -> Result<Sheet<'_>, GenerateFailure> {
        if self.entries.is_empty() {
            return Err(invalid("entries must be a non-empty list".into()));
        }
        // Counted before anything is allocated per entry or per cell. An
        // entry counts at least one cell (an empty list is refused when it
        // is read), so the entries are bounded by the cell cap too; and the
        // top-level list is read even when no entry uses it, so it is held
        // to the cap on its own.
        let default_len = list_len(self.dpi.as_ref()).unwrap_or(1);
        let mut n_cells = default_len;
        if default_len <= budget.max_cells {
            n_cells = 0;
            for e in &self.entries {
                let n = list_len(e.dpi.as_ref()).unwrap_or(default_len).max(1);
                n_cells = n_cells.saturating_add(n);
            }
        }
        let source_bytes = self
            .entries
            .iter()
            .fold(0usize, |n, e| n.saturating_add(e.source.len()));
        if n_cells > budget.max_cells {
            return Err(fail(
                GenerateErrorCode::InvalidRequest,
                "cells",
                format!(
                    "{n_cells} cells is over this server's limit of {} cells per sheet; split \
                     the sheet or use fewer dpi entries",
                    budget.max_cells
                ),
            ));
        }
        if source_bytes > budget.max_source_bytes {
            return Err(fail(
                GenerateErrorCode::SourceTooLarge,
                "sources",
                format!(
                    "the sources are {source_bytes} bytes in all; the limit is {} bytes per sheet",
                    budget.max_source_bytes
                ),
            ));
        }
        if (n_cells as u64).saturating_mul(MIN_CELL_MS) > budget.time_ms {
            return Err(fail(
                GenerateErrorCode::InvalidRequest,
                "deadline",
                format!(
                    "{n_cells} cells cannot each have {MIN_CELL_MS} ms within this server's {} ms \
                     for a sheet; use fewer cells",
                    budget.time_ms
                ),
            ));
        }
        let default_page = (
            side("width", self.width, DEFAULT_PAGE.0)?,
            side("height", self.height, DEFAULT_PAGE.1)?,
        );
        let default_cells = match &self.dpi {
            None | Some(Value::Null) => vec![Cell {
                dpi: 72.0,
                dpi32: 72.0,
                zoom: 1,
            }],
            Some(v) => cells("dpi", v)?,
        };
        let mut rows = Vec::with_capacity(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            let which = format!("entries[{i}]");
            if e.source.len() > budget.max_entry_source_bytes {
                return Err(fail(
                    GenerateErrorCode::SourceTooLarge,
                    "source",
                    format!(
                        "{which}.source is {} bytes; the limit is {} bytes",
                        e.source.len(),
                        budget.max_entry_source_bytes
                    ),
                ));
            }
            if let Some(l) = &e.label
                && l.chars().nth(MAX_LABEL_CHARS).is_some()
            {
                return Err(invalid(format!(
                    "{which}.label is over {MAX_LABEL_CHARS} characters"
                )));
            }
            let page = (
                side(&format!("{which}.width"), e.width, default_page.0)?,
                side(&format!("{which}.height"), e.height, default_page.1)?,
            );
            let cells = match &e.dpi {
                None | Some(Value::Null) => default_cells.clone(),
                Some(v) => cells(&format!("{which}.dpi"), v)?,
            };
            rows.push(Row {
                source: &e.source,
                label: e.label.as_deref(),
                page,
                cells,
            });
        }
        let sheet = Sheet {
            rows,
            labels: self.labels.unwrap_or(true),
            sizes: self.sizes.unwrap_or(true),
            cells: n_cells,
        };
        sheet.check_cells(budget)?;
        Ok(sheet)
    }
}

impl Sheet<'_> {
    /// Each cell's sides after zoom, then the pixels of all the renders.
    fn check_cells(&self, budget: &SheetBudget) -> Result<(), GenerateFailure> {
        let side = u64::from(budget.max_side_px);
        let mut rendered = 0u64;
        for (i, row) in self.rows.iter().enumerate() {
            for (c, cell) in row.cells.iter().enumerate() {
                let (w, h) = cell.pixels(row.page);
                let z = u64::from(cell.zoom);
                if w * z > side || h * z > side {
                    let shown = if cell.zoom > 1 {
                        format!(", {}x{} shown at {}x", w * z, h * z, cell.zoom)
                    } else {
                        String::new()
                    };
                    return Err(fail(
                        GenerateErrorCode::InvalidDimensions,
                        "cell_side",
                        format!(
                            "entries[{i}] cell {c}: a {}x{} pt page at {} dpi is {w}x{h} \
                             pixels{shown}; a cell is at most {side} pixels on a side",
                            row.page.0, row.page.1, cell.dpi
                        ),
                    ));
                }
                rendered = rendered.saturating_add(w * h);
                if rendered > budget.max_cell_pixels {
                    return Err(fail(
                        GenerateErrorCode::TooManyPixels,
                        "cell_pixels",
                        format!(
                            "the cells are over {} pixels in all (reached {rendered} at \
                             entries[{i}] cell {c}), this server's limit per sheet. Lower a dpi, \
                             a page size or the number of cells",
                            budget.max_cell_pixels
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The caption of a cell of `w`x`h` render pixels.
    fn caption(w: u64, h: u64, zoom: u32) -> String {
        if zoom > 1 {
            format!("{w}x{h} px (shown at {zoom}x)")
        } else {
            format!("{w}x{h} px")
        }
    }

    /// The layout with each strip at `strip(text, px, height_em)`'s size.
    fn layout_with(
        &self,
        mut strip: impl FnMut(&str, u32, bool) -> Result<(u32, u32), GenerateError>,
    ) -> Result<Vec<SheetRowSize>, GenerateError> {
        let mut rows = Vec::with_capacity(self.rows.len());
        for (i, row) in self.rows.iter().enumerate() {
            let header = if self.labels {
                strip(&row.label(i), LABEL_PX, true)?
            } else {
                (0, 0)
            };
            let mut cells = Vec::with_capacity(row.cells.len());
            for cell in &row.cells {
                let (w, h) = cell.pixels(row.page);
                let caption = if self.sizes {
                    strip(&Self::caption(w, h, cell.zoom), CAPTION_PX, false)?
                } else {
                    (0, 0)
                };
                cells.push(SheetCellSize {
                    image: ((w as u32) * cell.zoom, (h as u32) * cell.zoom),
                    caption,
                });
            }
            rows.push(SheetRowSize { header, cells });
        }
        Ok(rows)
    }

    /// The upper bound of a strip for `text`, as the plan sizes it: no text
    /// is laid out. `bold` strips are labels (any script), the others
    /// captions (ASCII).
    fn strip_bound(text: &str, px: u32, bold: bool) -> (u32, u32) {
        let chars = text.chars().count();
        if chars == 0 {
            return (0, 0);
        }
        let (width_em, height_em) = if bold {
            (LABEL_WIDTH_EM, STRIP_HEIGHT_EM)
        } else {
            (CAPTION_WIDTH_EM, CAPTION_HEIGHT_EM)
        };
        let w = (chars as f64 * f64::from(px) * width_em).ceil() as u64 + 2;
        let h = (px as f32 * height_em).ceil() as u32;
        (w.min(u64::from(SHEET_MAX_SIDE_PX)) as u32, h)
    }
}

/// The sheet's size against the budget: the side limit, then the area.
fn check_sheet(
    rows: &[SheetRowSize],
    budget: &SheetBudget,
    what: &str,
) -> Result<SheetLayout, GenerateFailure> {
    let layout = SheetLayout::new(rows).map_err(|m| {
        fail(
            GenerateErrorCode::InvalidDimensions,
            "sheet_side",
            format!(
                "{m} ({what}); lower a dpi, a page size, a zoom or the number of cells, or \
                 turn labels off"
            ),
        )
    })?;
    let (w, h) = layout.size();
    if w > budget.max_side_px || h > budget.max_side_px {
        return Err(fail(
            GenerateErrorCode::InvalidDimensions,
            "sheet_side",
            format!(
                "the sheet would be {w}x{h} pixels ({what}); this server draws at most {} \
                 pixels a side",
                budget.max_side_px
            ),
        ));
    }
    let pixels = u64::from(w) * u64::from(h);
    if pixels > budget.max_pixels {
        return Err(fail(
            GenerateErrorCode::TooManyPixels,
            "sheet_pixels",
            format!(
                "the sheet would be {w}x{h} = {pixels} pixels ({what}); this server draws at \
                 most {} pixels. Lower a dpi, a page size, a zoom or the number of cells",
                budget.max_pixels
            ),
        ));
    }
    Ok(layout)
}

/// Server side, before any worker starts: check the arguments and every
/// budget, and return the sheet's size at most (the plan's layout, with the
/// label and caption strips at their upper bounds).
pub fn plan(p: &SheetParams, budget: &SheetBudget) -> Result<(u32, u32), GenerateFailure> {
    let sheet = p.check(budget)?;
    let rows = sheet
        .layout_with(|t, px, bold| Ok(Sheet::strip_bound(t, px, bold)))
        .map_err(|_| invalid("the sheet could not be laid out".into()))?;
    Ok(check_sheet(&rows, budget, "with labels and captions at their widest")?.size())
}

/// The status of one cell, in the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SheetCellReport {
    /// The entry (row), from 0: `entries[row]`.
    pub row: usize,
    /// The item of the row's dpi list (column), from 0.
    pub column: usize,
    /// The cell's dpi.
    pub dpi: f64,
    /// How many times the render is enlarged on the sheet (1 for none).
    pub zoom: u32,
    /// The render's width in pixels (shown `zoom` times wider).
    pub width_px: u32,
    /// The render's height in pixels.
    pub height_px: u32,
    /// Left of the cell's image on the sheet, in pixels.
    pub x: u32,
    /// Top of the cell's image on the sheet, in pixels.
    pub y: u32,
    /// True when the program ran to the end.
    pub ok: bool,
    /// Why not, when `ok` is false. The cell shows the page as far as the
    /// program got.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<SheetCellError>,
}

/// Why a cell's program did not finish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SheetCellError {
    /// `postscript_error`, `limit_exceeded` (the cell's own step, time or
    /// memory limit), or `worker_failed` (the renderer failed on this
    /// cell; it is drawn blank).
    pub code: ErrorCode,
    /// Which limit, for `limit_exceeded`: `steps`, `time` or `memory`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<String>,
    /// PSCat's error report (error name, offending command, line), cut to
    /// a few hundred bytes.
    pub message: String,
}

/// What a composed sheet holds, in the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SheetReport {
    /// The sheet's width in pixels.
    pub width_px: u32,
    /// The sheet's height in pixels.
    pub height_px: u32,
    /// Cells whose program did not finish.
    pub failed_cells: usize,
    /// Every cell, row by row, left to right.
    pub cells: Vec<SheetCellReport>,
}

/// A composed sheet.
#[derive(Debug, Clone)]
pub struct ComposedSheet {
    /// The PNG.
    pub png: Vec<u8>,
    /// What it holds.
    pub report: SheetReport,
    /// Interpreter steps, all cells together.
    pub steps: u64,
    /// Milliseconds the cells' programs ran, all together.
    pub elapsed_ms: u64,
}

/// `s` cut to at most `max` bytes at a character boundary, `…` added when
/// cut, with control characters other than newlines made spaces (a JSON
/// escape would make each one six bytes).
fn cell_message(s: &str, max: usize) -> String {
    let clean: String = s
        .trim_end()
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    if clean.len() <= max {
        return clean;
    }
    let mut end = max;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &clean[..end])
}

/// The cell error for a program's PostScript error or passed limit.
fn ps_cell_error(d: &PsDiagnostic, limits: &InterpLimits) -> SheetCellError {
    match d.limit.as_deref() {
        Some(limit) => {
            let given = match limit {
                "time" => limits
                    .max_time_ms
                    .map(|ms| format!(" ({ms} ms for this cell)")),
                "steps" => limits.max_steps.map(|n| format!(" ({n} steps)")),
                "memory" => limits.max_memory_bytes.map(|n| format!(" ({n} bytes)")),
                _ => None,
            };
            SheetCellError {
                code: ErrorCode::LimitExceeded,
                limit: Some(limit.to_string()),
                message: format!(
                    "the program was stopped at the {limit} limit{}",
                    given.unwrap_or_default()
                ),
            }
        }
        None => SheetCellError {
            code: ErrorCode::PostscriptError,
            limit: None,
            message: cell_message(
                &format!("PostScript error: {}", d.report),
                MAX_CELL_MESSAGE_BYTES,
            ),
        },
    }
}

/// Run one cell's program on a thread of its own (see the module docs).
/// `Err` for a failure that is not the cell's (no thread, an unusable page).
fn run_cell(
    source: &[u8],
    page: (u32, u32),
    scale: f32,
    limits: &InterpLimits,
) -> Result<std::thread::Result<crate::pscat_extras::SheetCellRender>, String> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("sheet-cell".into())
            .stack_size(CELL_STACK_BYTES)
            .spawn_scoped(scope, || {
                // Count what this thread frees, its thread-locals too.
                pscat::memtrack::fold_at_thread_exit();
                render_sheet_cell(source, page, scale, limits)
            })
            .map_err(|e| format!("cannot start a cell's thread: {e}"))?;
        match handle.join() {
            Ok(Ok(render)) => Ok(Ok(render)),
            Ok(Err(message)) => Err(message),
            Err(panic) => Ok(Err(panic)),
        }
    })
}

/// Worker side: draw the strips, lay the sheet out exactly and check it
/// against the budget, then render every cell and compose the PNG. See the
/// module documentation.
pub fn compose(p: &SheetParams, cx: &GenerateContext<'_>) -> Result<ComposedSheet, GenerateError> {
    let started = Instant::now();
    let Some(budget) = cx.sheet else {
        return Err(GenerateError::Internal(
            "a review_sheet job needs a sheet budget".into(),
        ));
    };
    let sheet = p.check(budget)?;
    // The strips, kept to draw after the check. Each is bounded by the
    // label cap and the side limit.
    let mut strips: Vec<Option<SheetStrip>> = Vec::new();
    let rows = sheet.layout_with(|text, px, bold| {
        let strip = sheet_text(text, px, bold)
            .map_err(|m| GenerateError::Internal(format!("drawing a label failed: {m}")))?;
        let size = strip.as_ref().map_or((0, 0), SheetStrip::size);
        strips.push(strip);
        Ok(size)
    })?;
    let layout = check_sheet(&rows, budget, "with its labels and captions")?;
    let (sheet_w, sheet_h) = layout.size();
    let mut canvas = SheetCanvas::new(&layout).map_err(GenerateError::Internal)?;
    let mut strips = strips.into_iter();

    let mut report = SheetReport {
        width_px: sheet_w,
        height_px: sheet_h,
        failed_cells: 0,
        cells: Vec::with_capacity(sheet.cells),
    };
    let (mut steps, mut elapsed_ms) = (0u64, 0u64);
    let mut cells_left = sheet.cells as u64;
    for (r, row) in sheet.rows.iter().enumerate() {
        if sheet.labels
            && let Some(Some(strip)) = strips.next()
        {
            canvas.put_strip(layout.header_xy(r), &strip);
        }
        for (c, cell) in row.cells.iter().enumerate() {
            let used = started.elapsed().as_millis() as u64;
            let share = budget.time_ms.saturating_sub(used) / cells_left.max(1);
            cells_left = cells_left.saturating_sub(1);
            if share < MIN_CELL_MS {
                return Err(GenerateError::Input(fail(
                    GenerateErrorCode::Timeout,
                    "deadline",
                    format!(
                        "the sheet was not finished within this server's {} ms for a sheet \
                         ({} of {} cells drawn). Use fewer or cheaper cells",
                        budget.time_ms,
                        report.cells.len(),
                        sheet.cells
                    ),
                )));
            }
            let (w, h) = cell.pixels(row.page);
            let limits = InterpLimits {
                max_time_ms: Some(cx.limits.max_time_ms.map_or(share, |t| t.min(share))),
                max_page_pixels: w * h,
                ..*cx.limits
            };
            let outcome = run_cell(row.source.as_bytes(), row.page, cell.scale(), &limits)
                .map_err(GenerateError::Internal)?;
            let blank = || {
                SheetImage::blank(w as u32, h as u32)
                    .ok_or_else(|| GenerateError::Internal("a blank cell failed".into()))
            };
            let (image, error) = match outcome {
                Ok(render) => {
                    steps = steps.saturating_add(render.steps);
                    elapsed_ms = elapsed_ms.saturating_add(render.elapsed_ms);
                    let error = render.error.as_ref().map(|d| ps_cell_error(d, &limits));
                    let got = render.image.size();
                    if (u64::from(got.0), u64::from(got.1)) == (w, h) {
                        (render.image, error)
                    } else {
                        (
                            blank()?,
                            Some(SheetCellError {
                                code: ErrorCode::InvalidDimensions,
                                limit: None,
                                message: format!(
                                    "the program's page came back {}x{} pixels, not the cell's \
                                     {w}x{h}; it is drawn blank",
                                    got.0, got.1
                                ),
                            }),
                        )
                    }
                }
                Err(_panic) => (
                    blank()?,
                    Some(SheetCellError {
                        code: ErrorCode::WorkerFailed,
                        limit: None,
                        message: "the renderer failed on this cell; it is drawn blank".into(),
                    }),
                ),
            };
            let image = if cell.zoom > 1 {
                image
                    .zoomed(cell.zoom)
                    .ok_or_else(|| GenerateError::Internal("a cell is too large to zoom".into()))?
            } else {
                image
            };
            let at = layout.image_xy(r, c);
            canvas.put_image(at, &image);
            drop(image);
            if sheet.sizes
                && let Some(Some(strip)) = strips.next()
            {
                canvas.put_strip(layout.caption_xy(r, c), &strip);
            }
            if error.is_some() {
                report.failed_cells += 1;
            }
            report.cells.push(SheetCellReport {
                row: r,
                column: c,
                dpi: cell.dpi,
                zoom: cell.zoom,
                width_px: w as u32,
                height_px: h as u32,
                x: at.0,
                y: at.1,
                ok: error.is_none(),
                error,
            });
        }
    }
    let png = canvas
        .to_png()
        .map_err(|e| GenerateError::Internal(format!("encoding the sheet failed: {e}")))?;
    drop(canvas);
    if png.len() > cx.limits.max_output_bytes {
        return Err(GenerateError::Input(fail(
            GenerateErrorCode::OutputTooLarge,
            "sheet_bytes",
            format!(
                "the sheet is a {sheet_w}x{sheet_h} PNG of {} bytes; at most {} bytes are \
                 returned inline. Lower a dpi, a page size, a zoom or the number of cells",
                png.len(),
                cx.limits.max_output_bytes
            ),
        )));
    }
    Ok(ComposedSheet {
        png,
        report,
        steps,
        elapsed_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(v: Value) -> SheetParams {
        serde_json::from_value(v).unwrap()
    }

    fn budget() -> SheetBudget {
        SheetBudget::from_limits(&LimitsConfig::default())
    }

    fn refusal(v: Value, b: &SheetBudget) -> GenerateFailure {
        plan(&params(v), b).unwrap_err()
    }

    #[test]
    fn defaults_are_the_plugins() {
        let p = params(json!({"entries": [{"source": "showpage"}]}));
        let s = p.check(&budget()).unwrap();
        assert!(s.labels && s.sizes);
        assert_eq!(s.cells, 1);
        assert_eq!(s.rows[0].page, (612, 792));
        assert_eq!(s.rows[0].cells[0].pixels((612, 792)), (612, 792));
        assert_eq!(s.rows[0].label(0), "program 1");
        let p = params(json!({
            "entries": [{"source": "", "label": "a", "dpi": [144, {"dpi": 9, "zoom": 4}]},
                        {"source": "", "width": 100}],
            "width": 50, "height": 60, "dpi": [36], "labels": false
        }));
        let s = p.check(&budget()).unwrap();
        assert_eq!(s.cells, 3);
        assert_eq!(s.rows[0].page, (50, 60));
        assert_eq!(s.rows[1].page, (100, 60));
        assert_eq!(s.rows[0].cells[1].zoom, 4);
        assert_eq!(s.rows[0].cells[1].pixels((50, 60)), (6, 8));
        assert_eq!(s.rows[1].cells[0].dpi, 36.0);
        assert!(!s.labels && s.sizes);
    }

    #[test]
    fn arguments_are_checked_with_the_plugins_messages() {
        let b = budget();
        for (args, code, needle) in [
            (
                json!({"entries": []}),
                GenerateErrorCode::InvalidRequest,
                "entries must be a non-empty list",
            ),
            (
                json!({"entries": [{"source": "", "dpi": []}]}),
                GenerateErrorCode::InvalidRequest,
                "entries[0].dpi must be a non-empty list of cells",
            ),
            (
                json!({"entries": [{"source": ""}], "dpi": 72}),
                GenerateErrorCode::InvalidRequest,
                "dpi must be a non-empty list",
            ),
            (
                json!({"entries": [{"source": "", "dpi": [5]}]}),
                GenerateErrorCode::InvalidDimensions,
                "a dpi must be a number from 9 to 1200, got 5",
            ),
            (
                json!({"entries": [{"source": "", "dpi": [{"zoom": 2}]}]}),
                GenerateErrorCode::InvalidDimensions,
                "got nothing",
            ),
            (
                json!({"entries": [{"source": "", "dpi": [{"dpi": 9, "zoom": 1}]}]}),
                GenerateErrorCode::InvalidRequest,
                "zoom must be a whole number from 2 to 16, got 1",
            ),
            (
                json!({"entries": [{"source": "", "dpi": [{"dpi": 9, "zoom": 17}]}]}),
                GenerateErrorCode::InvalidRequest,
                "zoom must be",
            ),
            (
                json!({"entries": [{"source": "", "dpi": ["72"]}]}),
                GenerateErrorCode::InvalidRequest,
                "a cell is a dpi number or",
            ),
            (
                json!({"entries": [{"source": "", "label": "x".repeat(201)}]}),
                GenerateErrorCode::InvalidRequest,
                "entries[0].label is over 200 characters",
            ),
            (
                json!({"entries": [{"source": "", "width": 0}]}),
                GenerateErrorCode::InvalidDimensions,
                "entries[0].width must be a whole number from 1 to 8000, got 0",
            ),
            (
                json!({"entries": [{"source": ""}], "height": 8001}),
                GenerateErrorCode::InvalidDimensions,
                "height must be",
            ),
        ] {
            let f = refusal(args.clone(), &b);
            assert_eq!(f.code, code, "{args}: {}", f.message);
            assert!(f.message.contains(needle), "{args}: {}", f.message);
        }
        // 200 characters of any script is fine.
        plan(
            &params(json!({"entries": [{"source": "", "label": "한".repeat(200)}]})),
            &b,
        )
        .unwrap();
    }

    #[test]
    fn every_budget_is_checked_in_the_plan() {
        let b = SheetBudget {
            max_cells: 4,
            max_source_bytes: 20,
            max_entry_source_bytes: 12,
            max_cell_pixels: 100_000,
            max_pixels: 200_000,
            max_side_px: 8000,
            time_ms: 1000,
        };
        let detail = |args: Value| {
            let f = refusal(args, &b);
            (f.detail.clone(), f.code)
        };
        let src = |n: usize| "%".repeat(n);
        // cells: 5 > 4, counted from the lists without reading them
        assert_eq!(
            detail(
                json!({"entries": [{"source": "", "dpi": [9, 9, 9]}, {"source": ""}],
                          "dpi": [9, 9]})
            ),
            ("cells".into(), GenerateErrorCode::InvalidRequest)
        );
        // a list of a million cells is refused by its length
        let huge: Vec<Value> = vec![json!("x"); 1_000_000];
        assert_eq!(
            detail(json!({"entries": [{"source": "", "dpi": huge}]})).0,
            "cells"
        );
        // so is a top-level list no entry uses
        assert_eq!(
            detail(json!({"entries": [{"source": "", "dpi": [9]}], "dpi": huge})).0,
            "cells"
        );
        // and entries with empty lists count a cell each
        let empty: Vec<Value> = vec![json!({"source": "", "dpi": []}); 5];
        assert_eq!(detail(json!({"entries": empty})).0, "cells");
        assert_eq!(
            detail(json!({"entries": [{"source": src(10)}, {"source": src(11)}]})),
            ("sources".into(), GenerateErrorCode::SourceTooLarge)
        );
        assert_eq!(
            detail(json!({"entries": [{"source": src(13)}]})),
            ("source".into(), GenerateErrorCode::SourceTooLarge)
        );
        // 4 cells x 100 ms fit 1000 ms; with 300 ms they do not
        let tight = SheetBudget { time_ms: 300, ..b };
        let f = refusal(
            json!({"entries": [{"source": "", "dpi": [9, 9, 9, 9], "width": 10, "height": 10}]}),
            &tight,
        );
        assert_eq!(f.detail, "deadline", "{}", f.message);
        // a cell over the side limit, before or after zoom
        assert_eq!(
            detail(json!({"entries": [{"source": "", "width": 8000, "dpi": [73]}]})),
            ("cell_side".into(), GenerateErrorCode::InvalidDimensions)
        );
        let f = refusal(
            json!({"entries": [{"source": "", "width": 1000, "height": 10,
                                "dpi": [{"dpi": 72, "zoom": 9}]}]}),
            &b,
        );
        assert_eq!(f.detail, "cell_side");
        assert!(f.message.contains("9000x90 shown at 9x"), "{}", f.message);
        // the renders: 2 x 250x250 = 125000 > 100000
        assert_eq!(
            detail(
                json!({"entries": [{"source": "", "width": 250, "height": 250,
                                        "dpi": [72, 72]}]})
            ),
            ("cell_pixels".into(), GenerateErrorCode::TooManyPixels)
        );
        // zoom does not count toward the renders, but does toward the sheet
        assert_eq!(
            detail(
                json!({"entries": [{"source": "", "width": 100, "height": 100,
                                        "dpi": [{"dpi": 72, "zoom": 5}]}]})
            ),
            ("sheet_pixels".into(), GenerateErrorCode::TooManyPixels)
        );
        // a sheet over the side limit: four cells 2001 px wide in a row
        let wide = SheetBudget {
            max_cell_pixels: u64::MAX,
            max_pixels: u64::MAX,
            ..b
        };
        let f = refusal(
            json!({"entries": [{"source": "", "width": 2001, "height": 10,
                                "dpi": [72, 72, 72, 72]}]}),
            &wide,
        );
        assert_eq!(
            (f.detail.as_str(), f.code),
            ("sheet_side", GenerateErrorCode::InvalidDimensions),
            "{}",
            f.message
        );
        assert!(f.message.contains("8000px-per-side"), "{}", f.message);
        // Four cells 1960 px wide: 4 x (1960 + 2) + 3 x 14 + 2 x 12 = 7914.
        let ok = plan(
            &params(
                json!({"entries": [{"source": "", "width": 1960, "height": 10,
                                        "dpi": [72, 72, 72, 72]}],
                           "labels": false, "sizes": false}),
            ),
            &wide,
        )
        .unwrap();
        assert_eq!(ok, (7914, 10 + 2 + 24));
    }

    #[test]
    fn the_plan_bounds_the_strips() {
        assert_eq!(Sheet::strip_bound("", 16, true), (0, 0));
        // 10 chars x 16 px x 1.1 em + 2
        assert_eq!(Sheet::strip_bound("0123456789", 16, true), (178, 23));
        // 8 chars x 12 px x 0.75 em + 2
        assert_eq!(Sheet::strip_bound("48x48 px", 12, false), (74, 16));
        assert_eq!(
            Sheet::strip_bound(&"x".repeat(1000), 16, true),
            (SHEET_MAX_SIDE_PX, 23)
        );
    }

    /// The plan's strip bound holds for the widest glyphs PSCat draws, so
    /// a sheet the plan admits is admitted in the worker too.
    #[test]
    fn the_strip_bound_holds_for_the_widest_labels() {
        for text in [
            "W".repeat(200),
            "@".repeat(200),
            "Æ".repeat(200),
            "M".repeat(200),
            "—".repeat(200),
            "%".repeat(200),
            "한".repeat(200),
            "漢".repeat(200),
            "ก".repeat(200),
            "\u{1F600}".repeat(200),
        ] {
            let strip = sheet_text(&text, LABEL_PX, true).unwrap().unwrap();
            let bound = Sheet::strip_bound(&text, LABEL_PX, true);
            let got = strip.size();
            assert!(
                got.0 <= bound.0 && got.1 <= bound.1,
                "{:?}: {got:?} over {bound:?}",
                text.chars().next()
            );
        }
        for zoom in [1, 16] {
            let text = Sheet::caption(8000, 8000, zoom);
            let strip = sheet_text(&text, CAPTION_PX, false).unwrap().unwrap();
            let bound = Sheet::strip_bound(&text, CAPTION_PX, false);
            assert!(strip.size().0 <= bound.0 && strip.size().1 <= bound.1);
        }
    }

    #[test]
    fn cell_messages_are_bounded_and_clean() {
        assert_eq!(cell_message("a\u{1}b\nc  \n", 100), "a b\nc");
        let long = "é".repeat(500);
        let m = cell_message(&long, MAX_CELL_MESSAGE_BYTES);
        assert!(m.len() <= MAX_CELL_MESSAGE_BYTES + 3, "{}", m.len());
        assert!(m.ends_with('…'));
    }

    #[test]
    fn the_wire_shape_round_trips() {
        let p = params(json!({
            "entries": [{"source": "showpage", "label": "a", "dpi": [72, {"dpi": 9, "zoom": 2}]}],
            "sizes": false
        }));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(serde_json::from_value::<SheetParams>(v).unwrap(), p);
    }
}
