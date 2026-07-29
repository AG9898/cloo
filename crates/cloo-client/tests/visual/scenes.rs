//! Reusable scene builders for visual fixtures.
//!
//! A scene is everything the attached client needs to compose one frame —
//! terminal geometry, the tab bar, each pane's rectangle, cached grid and header
//! description, the attention queue, and the prefix hint — assembled without a
//! socket, a daemon, or a pseudoterminal. [`Scene::frame`] runs the production
//! [`compose_frame`] and captures the result, so a golden asserts the composed
//! picture rather than one chrome helper's output.
//!
//! A scene owns its grids and hands out only shared references while composing,
//! which is what makes "rendering does not mutate the client cache" checkable:
//! [`Scene::grids`] can be compared before and after a frame is captured.

// Shared fixture scaffolding; see `harness.rs` for why unused builders are
// expected here.
#![allow(dead_code)]

use cloo_client::chrome::{Attention, AttentionQueue, ChromeOptions, PaneChrome, PrefixHint};
use cloo_client::input::PaneArea;
use cloo_client::renderer::{FramePane, Grid, Span, compose_frame};
use cloo_client::theme::Theme;
use cloo_proto::{Cell, CellAttrs, Color, PaneId, RowUpdate, SessionId, Size, TabId, TabSummary};

use crate::harness::FrameMatrix;

/// One pane in a scene: where it sits, what its child wrote, and what its header
/// says.
#[derive(Debug, Clone)]
pub struct ScenePane {
    area: PaneArea,
    grid: Grid,
    header: PaneChrome,
}

impl ScenePane {
    /// A pane's grid at `(x, y)`, with a header row above it.
    #[must_use]
    pub fn new(pane: PaneId, x: u16, y: u16, size: Size, header: PaneChrome) -> Self {
        Self {
            area: PaneArea::new(pane, x, y, size),
            grid: Grid::new(size),
            header,
        }
    }

    /// The same pane with no header row drawn above it.
    #[must_use]
    pub fn headerless(mut self) -> Self {
        self.area = self.area.headerless();
        self
    }

    /// Fills the cached grid with application-owned text.
    ///
    /// The cells carry the terminal's own default colours, exactly as an
    /// unstyled child's output arrives: a scene never pre-themes pane contents,
    /// because deciding what a child's default cell looks like is the renderer's
    /// job and therefore something a golden has to be able to catch.
    ///
    /// # Panics
    ///
    /// If a line does not fit the pane, or there are more lines than rows.
    #[must_use]
    pub fn text(mut self, lines: &[&str]) -> Self {
        let size = self.grid.size();
        assert!(
            lines.len() <= usize::from(size.rows),
            "{} lines do not fit a pane of {} rows",
            lines.len(),
            size.rows
        );
        for (row, line) in lines.iter().enumerate() {
            let mut cells: Vec<Cell> = line
                .chars()
                .map(|ch| Cell {
                    ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    attrs: CellAttrs::NONE,
                })
                .collect();
            assert!(
                cells.len() <= usize::from(size.cols),
                "{:?} does not fit a pane of {} columns",
                line,
                size.cols
            );
            cells.resize(usize::from(size.cols), Cell::default());
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            self.grid
                .apply(&RowUpdate { row, cells })
                .expect("a scene row is built at the pane's own width");
        }
        self
    }

    /// Where the pane's grid sits, which is also what the hit-tester answers
    /// against.
    #[must_use]
    pub fn area(&self) -> PaneArea {
        self.area
    }

    /// The pane's cached grid.
    #[must_use]
    pub fn grid(&self) -> &Grid {
        &self.grid
    }
}

/// A complete attached picture, composable without a daemon.
#[derive(Debug, Clone)]
pub struct Scene {
    size: Size,
    session: SessionId,
    tabs: Vec<TabSummary>,
    panes: Vec<ScenePane>,
    queue: AttentionQueue,
    hint: PrefixHint,
    dim_unfocused: bool,
}

impl Scene {
    /// An empty terminal of `size`, on session 1, with the default prefix hint.
    #[must_use]
    pub fn new(size: Size) -> Self {
        Self {
            size,
            session: SessionId::new(1),
            tabs: Vec::new(),
            panes: Vec::new(),
            queue: AttentionQueue::new(),
            hint: PrefixHint::default(),
            dim_unfocused: true,
        }
    }

    /// Sets the session the status row names.
    #[must_use]
    pub fn session(mut self, session: u64) -> Self {
        self.session = SessionId::new(session);
        self
    }

    /// Appends one tab to the bar.
    #[must_use]
    pub fn tab(mut self, title: &str, active: bool) -> Self {
        let tab = TabId::new(u64::try_from(self.tabs.len()).unwrap_or(0) + 1);
        self.tabs.push(TabSummary {
            tab,
            title: title.to_owned(),
            active,
        });
        self
    }

    /// Adds one visible pane.
    #[must_use]
    pub fn pane(mut self, pane: ScenePane) -> Self {
        self.panes.push(pane);
        self
    }

    /// Replaces the prefix hint the status row draws.
    #[must_use]
    pub fn hint(mut self, hint: PrefixHint) -> Self {
        self.hint = hint;
        self
    }

    /// Turns the unfocused-pane dimming off, as the accessibility fallback does.
    #[must_use]
    pub fn no_dim(mut self) -> Self {
        self.dim_unfocused = false;
        self
    }

    /// Records one pane as needing attention, for the status summary and queue.
    #[must_use]
    pub fn attention(mut self, index: u16, title: &str, attention: Attention) -> Self {
        self.queue.record(index, title, attention);
        self
    }

    /// The chrome options this scene composes under.
    #[must_use]
    pub fn options(&self, theme: Theme) -> ChromeOptions {
        ChromeOptions {
            dim_unfocused: self.dim_unfocused,
            theme,
        }
    }

    /// Composes the frame through the production frame composer.
    #[must_use]
    pub fn spans(&self, theme: Theme) -> Vec<Span> {
        let panes: Vec<FramePane<'_>> = self
            .panes
            .iter()
            .map(|pane| FramePane::new(pane.area, &pane.grid, pane.header.clone()))
            .collect();
        compose_frame(
            self.size,
            &self.tabs,
            self.session,
            &panes,
            &self.queue,
            &self.hint,
            self.options(theme),
        )
    }

    /// Composes and captures the complete cell matrix.
    #[must_use]
    pub fn frame(&self, theme: Theme) -> FrameMatrix {
        FrameMatrix::capture(self.size, &self.spans(theme))
    }

    /// Copies of every pane's cached grid, for a before/after comparison.
    #[must_use]
    pub fn grids(&self) -> Vec<Grid> {
        self.panes.iter().map(|pane| pane.grid.clone()).collect()
    }
}
