//! The deterministic frame harness: capture, expectation, and diff.
//!
//! A composed frame is a complete fixed-size matrix of cells, so a visual
//! fixture asserts the whole matrix rather than one helper's output. Three
//! pieces make that reviewable:
//!
//! - [`FrameMatrix`] paints already-composed [`Span`]s onto a blank terminal of
//!   a known size. It is a pure function of the spans it is given — it holds no
//!   renderer, writes no bytes, and never touches the grids a scene composed
//!   from.
//! - [`ExpectedFrame`] is the golden, written as text: one line of characters
//!   and one line of single-character style keys per row, over a legend of
//!   [`SemanticStyle`] entries. Roles are named, never spelled as raw colour, so
//!   one expectation covers both a truecolor and a 16-colour resolution of the
//!   same theme.
//! - [`check_frame`] compares the two and reports the *first* differing cell by
//!   row, column, character, and semantic style, with the row drawn either side
//!   of it.
//!
//! Nothing here regenerates a golden. An expected frame is authored and reviewed
//! against `docs/STYLEGUIDE.md`; a harness that could rewrite one until a
//! failing implementation passed would not be an acceptance process.

// The harness is shared fixture scaffolding. Its full surface is consumed as the
// M9 cards land, so an unused builder here is expected rather than dead weight.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use cloo_client::renderer::Span;
use cloo_client::theme::{Theme, ThemeToken};
use cloo_proto::{Cell, CellAttrs, Color, Size};

/// Where one expected colour comes from.
///
/// A golden names roles rather than colours, which is what lets the same
/// expectation be checked against a truecolor theme and its 16-colour
/// resolution: [`Paint::Token`] resolves through whichever [`Theme`] the fixture
/// is asserting against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// A style-guide role, resolved through the theme under test.
    Token(ThemeToken),
    /// A role at its reference Storm value regardless of the theme under test.
    ///
    /// Chrome that is still authored against the reference palette rather than
    /// routed through the client theme is expected with this, so the gap is
    /// visible in the golden instead of hidden behind a raw RGB triple. A card
    /// that themes such a row turns its `Reference` entries into [`Paint::Token`]
    /// ones.
    Reference(ThemeToken),
    /// The outer terminal's own default, which chrome padding and unstyled child
    /// cells keep.
    Terminal,
    /// An explicit palette index, as an application may set.
    Ansi(u8),
    /// An explicit 24-bit colour, as an application may set.
    Rgb(u8, u8, u8),
}

impl Paint {
    /// The colour this expectation resolves to under `theme`.
    #[must_use]
    pub fn resolve(self, theme: Theme) -> Color {
        match self {
            Self::Token(token) => theme.color(token),
            Self::Reference(token) => Theme::storm().color(token),
            Self::Terminal => Color::Default,
            Self::Ansi(index) => Color::Indexed(index),
            Self::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
        }
    }
}

/// One expected cell rendition, in semantic terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticStyle {
    /// Expected foreground role.
    pub fg: Paint,
    /// Expected background role.
    pub bg: Paint,
    /// Expected rendition flags.
    pub attrs: CellAttrs,
}

impl SemanticStyle {
    /// A style with no rendition flags.
    #[must_use]
    pub const fn new(fg: Paint, bg: Paint) -> Self {
        Self {
            fg,
            bg,
            attrs: CellAttrs::NONE,
        }
    }

    /// The same style with rendition flags.
    #[must_use]
    pub const fn attrs(mut self, attrs: CellAttrs) -> Self {
        self.attrs = attrs;
        self
    }

    /// The cell this style expects for `ch` under `theme`.
    #[must_use]
    pub fn cell(self, ch: char, theme: Theme) -> Cell {
        Cell {
            ch,
            fg: self.fg.resolve(theme),
            bg: self.bg.resolve(theme),
            attrs: self.attrs,
        }
    }
}

/// A complete captured frame: every cell of a fixed-size terminal.
///
/// Cells no span painted stay [`Cell::default`], which is what an attached
/// client actually leaves on screen, so "complete" means complete rather than
/// "wherever chrome happened to draw".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameMatrix {
    size: Size,
    cells: Vec<Cell>,
}

impl FrameMatrix {
    /// Paints `spans` onto a blank terminal of `size`.
    ///
    /// Cells outside the terminal are clipped rather than rejected: a violent
    /// resize can legitimately leave a span overhanging, and a harness that
    /// panicked on one would hide the frame it was asked to describe.
    #[must_use]
    pub fn capture(size: Size, spans: &[Span]) -> Self {
        let mut matrix = Self {
            cells: vec![Cell::default(); usize::from(size.rows) * usize::from(size.cols)],
            size,
        };
        for span in spans {
            for (offset, cell) in span.cells.iter().enumerate() {
                let Ok(offset) = u16::try_from(offset) else {
                    break;
                };
                let Some(col) = span.at.col.checked_add(offset) else {
                    break;
                };
                matrix.set(col, span.at.row, *cell);
            }
        }
        matrix
    }

    /// The terminal geometry this frame describes.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// One cell, or [`Cell::default`] outside the frame.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Cell {
        self.index(col, row)
            .and_then(|index| self.cells.get(index).copied())
            .unwrap_or_default()
    }

    /// One row's characters, for a readable diff.
    #[must_use]
    pub fn text_row(&self, row: u16) -> String {
        (0..self.size.cols)
            .map(|col| self.cell(col, row).ch)
            .collect()
    }

    /// One complete row of this frame, as a frame of its own.
    ///
    /// A row of a live composed frame is still every cell of that row, so a
    /// single-row golden is a complete expectation rather than a partial one.
    /// It is what lets the tab row be asserted at many widths without also
    /// re-authoring the pane and status rows beneath it at each one.
    #[must_use]
    pub fn only_row(&self, row: u16) -> Self {
        Self {
            size: Size::new(self.size.cols, 1),
            cells: (0..self.size.cols).map(|col| self.cell(col, row)).collect(),
        }
    }

    fn set(&mut self, col: u16, row: u16, cell: Cell) {
        let Some(index) = self.index(col, row) else {
            return;
        };
        if let Some(slot) = self.cells.get_mut(index) {
            *slot = cell;
        }
    }

    fn index(&self, col: u16, row: u16) -> Option<usize> {
        if col >= self.size.cols || row >= self.size.rows {
            return None;
        }
        Some(usize::from(row) * usize::from(self.size.cols) + usize::from(col))
    }
}

/// A golden frame, authored as text.
///
/// Each row is a line of characters and a parallel line of style keys, so the
/// expectation reads as the picture it describes. A key must be registered with
/// [`style`](Self::style) before a row uses it; a row whose two lines disagree in
/// length, or that names an unregistered key, is an authoring error and panics
/// immediately rather than turning into a frame mismatch later.
#[derive(Debug, Clone, Default)]
pub struct ExpectedFrame {
    legend: BTreeMap<char, SemanticStyle>,
    chars: Vec<Vec<char>>,
    keys: Vec<Vec<char>>,
}

impl ExpectedFrame {
    /// An empty expectation.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one style key.
    #[must_use]
    pub fn style(mut self, key: char, style: SemanticStyle) -> Self {
        self.legend.insert(key, style);
        self
    }

    /// Appends one row: its characters and its style keys.
    ///
    /// # Panics
    ///
    /// If the two lines differ in length, if the row's width differs from an
    /// earlier row's, or if a key has no legend entry.
    #[must_use]
    pub fn row(mut self, text: &str, keys: &str) -> Self {
        let row = self.chars.len();
        let text: Vec<char> = text.chars().collect();
        let keys: Vec<char> = keys.chars().collect();
        assert_eq!(
            text.len(),
            keys.len(),
            "expected row {row} has {} characters and {} style keys",
            text.len(),
            keys.len()
        );
        if let Some(first) = self.chars.first() {
            assert_eq!(
                text.len(),
                first.len(),
                "expected row {row} is {} cells wide, but row 0 is {} cells wide",
                text.len(),
                first.len()
            );
        }
        for key in &keys {
            assert!(
                self.legend.contains_key(key),
                "expected row {row} uses style key {key:?}, which the legend does not define"
            );
        }
        self.chars.push(text);
        self.keys.push(keys);
        self
    }

    /// The geometry this expectation describes.
    #[must_use]
    pub fn size(&self) -> Size {
        let cols = self.chars.first().map_or(0, Vec::len);
        Size::new(
            u16::try_from(cols).unwrap_or(u16::MAX),
            u16::try_from(self.chars.len()).unwrap_or(u16::MAX),
        )
    }

    /// The character and semantic style expected at one position.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<(char, SemanticStyle)> {
        let row = usize::from(row);
        let col = usize::from(col);
        let ch = *self.chars.get(row)?.get(col)?;
        let key = self.keys.get(row)?.get(col)?;
        Some((ch, *self.legend.get(key)?))
    }

    /// One row's characters, for a readable diff.
    #[must_use]
    pub fn text_row(&self, row: u16) -> String {
        self.chars
            .get(usize::from(row))
            .map(|row| row.iter().collect())
            .unwrap_or_default()
    }
}

/// Compares a captured frame against its golden, resolved through `theme`.
///
/// Returns the readable report as an error rather than panicking, so a fixture
/// can assert on what a failure says. [`assert_frame`] is the ordinary entry
/// point.
///
/// # Errors
///
/// Reports a geometry difference, or the first differing cell in row-major
/// order — the top-left-most difference a reader would look for first.
pub fn check_frame(
    actual: &FrameMatrix,
    expected: &ExpectedFrame,
    theme: Theme,
) -> Result<(), String> {
    let want = expected.size();
    let got = actual.size();
    if want != got {
        return Err(format!(
            "visual frame geometry mismatch: expected {}x{} (cols x rows), drew {}x{}",
            want.cols, want.rows, got.cols, got.rows
        ));
    }

    for row in 0..want.rows {
        for col in 0..want.cols {
            let Some((ch, style)) = expected.cell(col, row) else {
                continue;
            };
            let wanted = style.cell(ch, theme);
            let drawn = actual.cell(col, row);
            if wanted != drawn {
                return Err(report(actual, expected, theme, row, col, wanted, drawn));
            }
        }
    }
    Ok(())
}

/// Asserts a captured frame against its golden.
///
/// # Panics
///
/// With the [`check_frame`] report when the frames differ.
pub fn assert_frame(actual: &FrameMatrix, expected: &ExpectedFrame, theme: Theme) {
    if let Err(report) = check_frame(actual, expected, theme) {
        panic!("{report}");
    }
}

/// Builds the readable first-difference report.
fn report(
    actual: &FrameMatrix,
    expected: &ExpectedFrame,
    theme: Theme,
    row: u16,
    col: u16,
    wanted: Cell,
    drawn: Cell,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "visual frame mismatch at row {row}, col {col}");
    let _ = writeln!(
        out,
        "  char   expected {:?}   actual {:?}",
        wanted.ch, drawn.ch
    );
    let _ = writeln!(
        out,
        "  fg     expected {}   actual {}",
        describe_color(wanted.fg, theme),
        describe_color(drawn.fg, theme)
    );
    let _ = writeln!(
        out,
        "  bg     expected {}   actual {}",
        describe_color(wanted.bg, theme),
        describe_color(drawn.bg, theme)
    );
    let _ = writeln!(
        out,
        "  attrs  expected {}   actual {}",
        describe_attrs(wanted.attrs),
        describe_attrs(drawn.attrs)
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "  expected row {row} |{}|", expected.text_row(row));
    let _ = writeln!(out, "  actual   row {row} |{}|", actual.text_row(row));
    // `"  actual   row {row} |"` is 17 characters plus the row number, so the
    // caret lands under the very cell the report just described.
    let indent = 17 + row.to_string().len() + usize::from(col);
    let _ = writeln!(out, "{}^", " ".repeat(indent));
    out
}

/// Names a colour by the role it fills, falling back to its literal spelling.
///
/// Several roles can share one colour once a theme has resolved to sixteen
/// colours — frame and surface are both black there — so this reports the first
/// matching role in the style guide's documented order and always prints the
/// literal value beside it.
fn describe_color(color: Color, theme: Theme) -> String {
    let literal = describe_literal(color);
    if let Some(token) = role_of(color, theme) {
        return format!("{token:?}({literal})");
    }
    if let Some(token) = role_of(color, Theme::storm()) {
        return format!("storm {token:?}({literal})");
    }
    literal
}

fn role_of(color: Color, theme: Theme) -> Option<ThemeToken> {
    ThemeToken::ALL
        .into_iter()
        .find(|token| theme.color(*token) == color)
}

fn describe_literal(color: Color) -> String {
    match color {
        Color::Default => "terminal-default".to_owned(),
        Color::Indexed(index) => format!("ansi:{index}"),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
    }
}

fn describe_attrs(attrs: CellAttrs) -> String {
    let names = [
        (CellAttrs::BOLD, "bold"),
        (CellAttrs::DIM, "dim"),
        (CellAttrs::ITALIC, "italic"),
        (CellAttrs::UNDERLINE, "underline"),
        (CellAttrs::REVERSE, "reverse"),
        (CellAttrs::HIDDEN, "hidden"),
        (CellAttrs::STRIKETHROUGH, "strikethrough"),
    ];
    let set: Vec<&str> = names
        .into_iter()
        .filter(|(flag, _)| attrs.contains(*flag))
        .map(|(_, name)| name)
        .collect();
    if set.is_empty() {
        "none".to_owned()
    } else {
        set.join("|")
    }
}
