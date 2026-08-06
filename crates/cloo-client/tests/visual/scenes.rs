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

use std::time::Instant;

use cloo_client::chrome::{
    Attention, AttentionQueue, ChromeOptions, PaneChrome, PrefixHint, StatusBar, TabBar, Toast,
    resize_affordance_spans, toast_rows, toast_stack_span,
};
use cloo_client::input::PaneArea;
use cloo_client::renderer::{FramePane, Grid, Span, compose_frame};
use cloo_client::status::RepositoryStatus;
use cloo_client::theme::Theme;
use cloo_core::{BorderStyle, StatusMode};
use cloo_proto::{
    Cell, CellAttrs, Color, Direction, PaneId, Point, RowUpdate, Size, TabId, TabSummary,
};

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
    /// The daemon-projected session name the tab row's badge draws, when one has
    /// been received. `None` is the honest pre-`WorkspaceStatus` state.
    name: Option<String>,
    /// The daemon-projected attached-client count, when one has been received.
    clients: Option<u16>,
    tabs: Vec<TabSummary>,
    panes: Vec<ScenePane>,
    queue: AttentionQueue,
    hint: PrefixHint,
    dim_unfocused: bool,
    border_style: BorderStyle,
    /// Which of card 07's two status compositions the client is configured for.
    mode: StatusMode,
    /// The client's own local wall-clock reading, when one was taken.
    clock: Option<String>,
    /// The bounded Git answer for the focused pane's working directory.
    repository: Option<RepositoryStatus>,
    /// The daemon's projected effective minimum size.
    effective: Option<Size>,
    /// The client's bounded notice stack, newest last.
    toasts: Vec<Toast>,
    /// The focused pane's cursor row, which a notice is never drawn over.
    cursor_row: Option<u16>,
    /// An in-flight divider resize: its lit cells and its visible ratio.
    resize: Option<(Vec<Point>, Direction, f32)>,
}

impl Scene {
    /// An empty terminal of `size`, with the default prefix hint.
    #[must_use]
    pub fn new(size: Size) -> Self {
        Self {
            size,
            name: None,
            clients: None,
            tabs: Vec::new(),
            panes: Vec::new(),
            queue: AttentionQueue::new(),
            hint: PrefixHint::default(),
            dim_unfocused: true,
            // The shipped default, so a golden asserts the appearance a user
            // actually gets. `borders()` overrides it where a card is about the
            // preference itself.
            border_style: BorderStyle::default(),
            mode: StatusMode::Minimal,
            clock: None,
            repository: None,
            effective: None,
            toasts: Vec::new(),
            cursor_row: None,
            resize: None,
        }
    }

    /// Supplies the daemon's projected session name, as `WorkspaceStatus` does.
    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    /// Supplies the daemon's projected attached-client count.
    #[must_use]
    pub fn clients(mut self, clients: u16) -> Self {
        self.clients = Some(clients);
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

    /// Composes this scene with one border glyph set.
    #[must_use]
    pub fn borders(mut self, borders: BorderStyle) -> Self {
        self.border_style = borders;
        self
    }

    /// Records one pane as needing attention, for the status summary and queue.
    #[must_use]
    pub fn attention(mut self, index: u16, title: &str, attention: Attention) -> Self {
        self.queue.record(index, title, attention);
        self
    }

    /// Selects card 07's other status composition, as `[visual].status` does.
    #[must_use]
    pub const fn status(mut self, mode: StatusMode) -> Self {
        self.mode = mode;
        self
    }

    /// Supplies the client's local wall-clock reading.
    #[must_use]
    pub fn clock(mut self, clock: &str) -> Self {
        self.clock = Some(clock.to_owned());
        self
    }

    /// Supplies the bounded Git answer for the focused working directory.
    #[must_use]
    pub fn repository(mut self, branch: &str, changes: u32) -> Self {
        self.repository = Some(RepositoryStatus {
            branch: Some(branch.to_owned()),
            changes,
        });
        self
    }

    /// Supplies the daemon's projected effective minimum size.
    #[must_use]
    pub const fn effective_size(mut self, size: Size) -> Self {
        self.effective = Some(size);
        self
    }

    /// Raises one notice on the client's bounded stack, oldest first.
    ///
    /// The notice is built settled rather than mid-entrance, because a golden
    /// describes the chrome's own colours: the entrance is a motion contract
    /// tested in `motion`, not a second appearance for a card to assert.
    #[must_use]
    pub fn toast(mut self, index: u16, title: &str, attention: Attention) -> Self {
        self.toasts
            .push(Toast::new(index, title, attention, Instant::now()));
        self
    }

    /// Names the focused pane's cursor row, which the notice stack skips.
    #[must_use]
    pub const fn cursor_row(mut self, row: u16) -> Self {
        self.cursor_row = Some(row);
        self
    }

    /// Lights a divider at `points` and reports its resulting visible ratio.
    #[must_use]
    pub fn resizing(mut self, points: Vec<Point>, dir: Direction, ratio: f32) -> Self {
        self.resize = Some((points, dir, ratio));
        self
    }

    /// The chrome options this scene composes under.
    #[must_use]
    pub fn options(&self, theme: Theme) -> ChromeOptions {
        ChromeOptions {
            dim_unfocused: self.dim_unfocused,
            theme,
            borders: self.border_style,
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
        let mut bar = TabBar::new(&self.tabs).panes(panes.len());
        if let Some(name) = &self.name {
            bar = bar.session(name);
        }
        if let Some(clients) = self.clients {
            bar = bar.clients(clients);
        }
        let mut status = StatusBar::new(&self.tabs, &self.queue, &self.hint).mode(self.mode);
        if let Some(name) = &self.name {
            status = status.session(name);
        }
        if let Some(clients) = self.clients {
            status = status.clients(clients);
        }
        if let Some(size) = self.effective {
            status = status.effective_size(size);
        }
        if let Some(repository) = &self.repository {
            status = status.repository(repository);
        }
        if let Some(clock) = &self.clock {
            status = status.clock(clock);
        }
        let mut spans = compose_frame(self.size, bar, &panes, status, self.options(theme));

        // The attached loop layers its own client surfaces over the composed
        // frame in exactly this order, and a card that shows one is asserting
        // that layering rather than a helper's isolated output.
        let rows = toast_rows(self.size, self.toasts.len(), self.cursor_row);
        for (toast, row) in self.toasts.iter().zip(rows) {
            spans.push(toast_stack_span(self.size, row, toast, theme));
        }
        if let Some((points, dir, ratio)) = &self.resize {
            spans.extend(resize_affordance_spans(
                points, *dir, *ratio, self.size, theme,
            ));
        }
        spans
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
