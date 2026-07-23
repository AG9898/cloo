//! The pure layout tree: a binary tree of splits whose leaves are panes.
//!
//! Two rules govern everything here, and both are easy to get wrong:
//!
//! - **Ratios, never cell counts.** A split stores the fraction of its parent's
//!   extent that goes to the first child. Cell counts are derived on every
//!   layout pass, which is what makes a layout survive an outer-terminal resize.
//! - **Minimum pane size is enforced at split time.** A split that would produce
//!   a pane below [`MIN_PANE_SIZE`] is rejected and the tree is left untouched.
//!   Skipping this creates zero-width PTYs and correspondingly baffling shell
//!   behavior.
//!
//! Nothing in this module performs I/O or knows a PTY exists. [`Layout::resolve`]
//! is the single layout pass: it flattens the tree into one [`PaneRect`] per
//! leaf, which is what the server hands to `TIOCSWINSZ` and puts on the wire.
//!
//! Two things sit on top of that tree without changing it. Directional focus —
//! [`Layout::neighbor`] — is a pure query over one layout pass, so "the pane to
//! the left" means what a user sees rather than wherever the tree happens to put
//! a sibling. Zoom is a *view* flag, [`Layout::zoom`]: it makes [`Layout::resolve`]
//! answer with the zoomed pane alone at the full area, and it touches no split
//! and no ratio, which is what makes unzoom exact rather than approximate.

use cloo_proto::{Direction, PaneId, PaneRect, Size};

use crate::error::LayoutError;

/// The smallest pane cloo will create.
///
/// Chosen so a shell prompt and a line of output remain legible. Chrome is drawn
/// client-side and costs nothing here — these are grid cells the child program
/// actually gets.
pub const MIN_PANE_SIZE: Size = Size::new(20, 3);

/// One of the four directions focus moves in.
///
/// Not `cloo_proto::Direction`, which names a split *axis* and has two variants.
/// Nor a wire type: the client sends `Action::FocusLeft` and the server turns it
/// into one of these, so adding a direction is never a protocol change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Toward smaller columns.
    Left,
    /// Toward larger columns.
    Right,
    /// Toward smaller rows.
    Up,
    /// Toward larger rows.
    Down,
}

impl Side {
    /// The axis this side moves along.
    #[must_use]
    pub const fn axis(self) -> Direction {
        match self {
            Self::Left | Self::Right => Direction::Horizontal,
            Self::Up | Self::Down => Direction::Vertical,
        }
    }
}

/// A node in the layout tree: either a pane or a split of two children.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A pane. The leaf of the tree, backed by exactly one PTY.
    Leaf(PaneId),
    /// A split of the available area between two children.
    Split {
        /// Which axis the area is divided along.
        dir: Direction,
        /// Fraction of the parent's extent given to `first`, in `(0.0, 1.0)`.
        ratio: f32,
        /// The left or top child, depending on `dir`.
        first: Box<Node>,
        /// The right or bottom child, depending on `dir`.
        second: Box<Node>,
    },
}

impl Node {
    /// Appends every pane in this subtree, in left-to-right traversal order.
    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(pane) => out.push(*pane),
            Self::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }
}

/// The layout of one tab: a tree of ratio splits over a single pane area.
///
/// A layout always holds at least one pane — there is no empty layout, and
/// closing the last pane is an error rather than a way to reach one.
///
/// Zoom rides along as a view flag rather than as a shape: the tree of a zoomed
/// layout is the tree of the same layout unzoomed, cell for cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    root: Node,
    /// The pane shown alone at the full area, if any. Never part of the tree.
    zoomed: Option<PaneId>,
}

impl Layout {
    /// A layout holding a single full-area pane.
    #[must_use]
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: Node::Leaf(pane),
            zoomed: None,
        }
    }

    /// The tree, for callers that need to walk it (serialization, tests).
    #[must_use]
    pub const fn root(&self) -> &Node {
        &self.root
    }

    /// Every pane in the layout, in left-to-right traversal order.
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.root.collect_panes(&mut out);
        out
    }

    /// How many panes the layout holds. Always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panes().len()
    }

    /// Always `false` — a layout cannot be empty. Present because clippy asks
    /// for it alongside [`Layout::len`], and answering honestly beats hiding it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `pane` is in this layout.
    #[must_use]
    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes().contains(&pane)
    }

    /// Flattens the tree into one rectangle per pane — the single layout pass.
    ///
    /// Rectangles tile `area` exactly: no gaps, no overlap, no borders. Chrome is
    /// the client's business, so every cell here belongs to a child program.
    ///
    /// When `area` is too small to honor [`MIN_PANE_SIZE`] — an outer terminal
    /// that shrank after the splits were made — panes are squeezed rather than
    /// dropped, down to a floor of one cell per axis. Rejection happens at
    /// [`Layout::split`] time; a resize must still produce a drawable answer.
    ///
    /// While a pane is [zoomed](Layout::zoom) this answers with that pane alone,
    /// filling `area`. Everything else is hidden, not resized: a hidden pane's
    /// child keeps the `winsize` it already had, which is why unzoom costs a
    /// geometry pass and never a restart.
    #[must_use]
    pub fn resolve(&self, area: Size) -> Vec<PaneRect> {
        if let Some(pane) = self.zoomed {
            return vec![PaneRect {
                pane,
                x: 0,
                y: 0,
                size: area,
            }];
        }
        self.tree_rects(area)
    }

    /// The resolved rectangle of a single pane as drawn, or `None` if it is not
    /// visible. A pane hidden behind a zoom has no rectangle.
    #[must_use]
    pub fn rect_of(&self, pane: PaneId, area: Size) -> Option<PaneRect> {
        self.resolve(area).into_iter().find(|r| r.pane == pane)
    }

    /// One layout pass over the tree, ignoring zoom.
    ///
    /// The geometry the panes *would* have, which is what a split checks against
    /// and what directional focus reads. Private, because a caller who wanted
    /// this when they meant [`Layout::resolve`] would be drawing hidden panes.
    fn tree_rects(&self, area: Size) -> Vec<PaneRect> {
        let mut out = Vec::new();
        assign(&self.root, 0, 0, area, &mut out);
        out
    }

    /// The pane immediately `side` of `pane`, or `None` if there is none.
    ///
    /// Geometric, not structural: the answer comes from one layout pass, so it
    /// is the pane a user sees in that direction rather than whichever sibling
    /// the tree happens to hold. A candidate must lie wholly on that side and
    /// share some extent on the perpendicular axis; among those, the nearest
    /// wins, ties going to the one nearest the origin's own leading edge and
    /// then to traversal order. That last tie-break is what makes the answer
    /// deterministic, and it is why moving right and then left can land on a
    /// different pane than it started from — a property tmux shares.
    ///
    /// Zoom is deliberately ignored. Focus is a property of the layout and zoom
    /// is a property of the view, so moving focus while zoomed is meaningful;
    /// what the view does with the new focus is the caller's policy.
    #[must_use]
    pub fn neighbor(&self, pane: PaneId, side: Side, area: Size) -> Option<PaneId> {
        let rects = self.tree_rects(area);
        let origin = *rects.iter().find(|r| r.pane == pane)?;
        rects
            .iter()
            .filter(|r| r.pane != pane)
            .filter_map(|r| {
                distance(&origin, r, side).map(|d| (d, offset(&origin, r, side), r.pane))
            })
            .min_by_key(|&(distance, offset, _)| (distance, offset))
            .map(|(_, _, pane)| pane)
    }

    /// The pane shown alone at the full area, if any.
    #[must_use]
    pub const fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// Shows `pane` alone at the full area.
    ///
    /// Idempotent, and free: no split is touched and no ratio is recomputed, so
    /// [`Layout::unzoom`] restores the previous picture exactly rather than
    /// approximately.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if `pane` is not in the layout. The zoom
    /// state is unchanged.
    pub fn zoom(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        if !self.contains(pane) {
            return Err(LayoutError::UnknownPane(pane));
        }
        self.zoomed = Some(pane);
        Ok(())
    }

    /// Shows every pane again. Idempotent.
    pub const fn unzoom(&mut self) {
        self.zoomed = None;
    }

    /// Zooms `pane`, or unzooms if anything is zoomed already.
    ///
    /// Returns whether a pane is zoomed afterwards. Toggling off does not
    /// require the zoomed pane to be the one named: a keybinding means "undo
    /// the zoom", whichever pane it was on.
    ///
    /// # Errors
    ///
    /// As [`Layout::zoom`], and only when there was nothing to unzoom.
    pub fn toggle_zoom(&mut self, pane: PaneId) -> Result<bool, LayoutError> {
        if self.zoomed.is_some() {
            self.unzoom();
            return Ok(false);
        }
        self.zoom(pane)?;
        Ok(true)
    }

    /// Splits `target` along `dir`, giving `ratio` of its area to `target` and
    /// the remainder to `new_pane`.
    ///
    /// # Errors
    ///
    /// - [`LayoutError::UnknownPane`] if `target` is not in the layout.
    /// - [`LayoutError::DuplicatePane`] if `new_pane` already is.
    /// - [`LayoutError::InvalidRatio`] if `ratio` is not finite and inside
    ///   `(0.0, 1.0)`.
    /// - [`LayoutError::TooSmall`] if either resulting pane would fall below
    ///   [`MIN_PANE_SIZE`]. The layout is unchanged in every error case.
    ///
    /// A successful split unzooms. The area it is checked against is the target
    /// pane's real geometry, never the whole area a zoom lends it — accepting a
    /// split at the zoomed size would produce panes that do not fit the moment
    /// the zoom ends.
    pub fn split(
        &mut self,
        target: PaneId,
        dir: Direction,
        ratio: f32,
        new_pane: PaneId,
        area: Size,
    ) -> Result<(), LayoutError> {
        if !ratio.is_finite() || ratio <= 0.0 || ratio >= 1.0 {
            return Err(LayoutError::InvalidRatio(ratio));
        }
        if self.contains(new_pane) {
            return Err(LayoutError::DuplicatePane(new_pane));
        }
        let rect = self
            .tree_rects(area)
            .into_iter()
            .find(|r| r.pane == target)
            .ok_or(LayoutError::UnknownPane(target))?;

        let (first, second) = halves(rect.size, dir, ratio);
        if !fits(first) || !fits(second) {
            return Err(LayoutError::TooSmall {
                pane: target,
                available: rect.size,
                minimum: MIN_PANE_SIZE,
            });
        }

        let replaced = replace_leaf(&mut self.root, target, dir, ratio, new_pane);
        debug_assert!(replaced, "target was resolved, so it must be a leaf");
        self.unzoom();
        Ok(())
    }

    /// Splits `target` down the middle. The common case behind a keybinding.
    ///
    /// # Errors
    ///
    /// As [`Layout::split`].
    pub fn split_even(
        &mut self,
        target: PaneId,
        dir: Direction,
        new_pane: PaneId,
        area: Size,
    ) -> Result<(), LayoutError> {
        self.split(target, dir, 0.5, new_pane, area)
    }

    /// Removes `pane` and collapses its parent split, promoting the sibling
    /// subtree into the parent's place.
    ///
    /// # Errors
    ///
    /// - [`LayoutError::UnknownPane`] if `pane` is not in the layout.
    /// - [`LayoutError::LastPane`] if it is the only pane. A tab with no panes
    ///   is closed by the caller, not represented as an empty layout.
    ///
    /// Closing the zoomed pane unzooms; closing any other leaves the zoom where
    /// it was, since the pane it names is still there.
    pub fn close(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        if let Node::Leaf(only) = self.root {
            return if only == pane {
                Err(LayoutError::LastPane(pane))
            } else {
                Err(LayoutError::UnknownPane(pane))
            };
        }
        if collapse(&mut self.root, pane) {
            if self.zoomed == Some(pane) {
                self.unzoom();
            }
            Ok(())
        } else {
            Err(LayoutError::UnknownPane(pane))
        }
    }

    /// Sets the ratio of the nearest ancestor split of `pane` along `dir`.
    ///
    /// This is the whole of resize: adjust one ratio, then run a layout pass.
    /// Nothing stores cell counts, so nothing else needs updating.
    ///
    /// # Errors
    ///
    /// - [`LayoutError::InvalidRatio`] if `ratio` is not finite and inside
    ///   `(0.0, 1.0)`.
    /// - [`LayoutError::UnknownPane`] if `pane` is not in the layout.
    /// - [`LayoutError::NoSplit`] if no ancestor splits along `dir`.
    pub fn set_ratio(
        &mut self,
        pane: PaneId,
        dir: Direction,
        ratio: f32,
    ) -> Result<(), LayoutError> {
        if !ratio.is_finite() || ratio <= 0.0 || ratio >= 1.0 {
            return Err(LayoutError::InvalidRatio(ratio));
        }
        match adjust(&mut self.root, pane, dir, ratio) {
            Search::Missing => Err(LayoutError::UnknownPane(pane)),
            Search::Found => Err(LayoutError::NoSplit { pane, dir }),
            Search::Applied => Ok(()),
        }
    }

    /// Moves the divider next to `pane`, growing it by `delta` cells along `dir`.
    ///
    /// The dragging form of [`Layout::set_ratio`], and it changes exactly the
    /// same one thing: the ratio of `pane`'s nearest ancestor split along `dir`.
    /// No pane is created, closed, reordered, or moved to another split — a drag
    /// is a ratio and nothing else, which is why the tree of a resized layout is
    /// the tree it had, node for node.
    ///
    /// `delta` is in cells because a pointer is: a drag lands on a column, not on
    /// a fraction. `area` is what turns the two into each other, so the caller
    /// must pass the area the layout is currently resolved against — the same one
    /// [`Layout::resolve`] was given — or the cells will mean something else.
    /// Which side of the divider `pane` sits on is worked out here, so a caller
    /// never has to know whether it is the split's first or second child.
    ///
    /// The result is clamped so both halves keep [`MIN_PANE_SIZE`] on that axis
    /// when the extent can hold two of them, and to one cell each when it cannot.
    /// A drag past the end therefore stops at the end rather than being refused:
    /// a pointer that ran off the screen is not an error a user can act on. Only
    /// the two halves of *this* split are checked, exactly as [`Layout::split`]
    /// checks only the two halves it creates.
    ///
    /// # Errors
    ///
    /// - [`LayoutError::UnknownPane`] if `pane` is not in the layout.
    /// - [`LayoutError::NoSplit`] if no ancestor splits along `dir`.
    pub fn resize(
        &mut self,
        pane: PaneId,
        dir: Direction,
        delta: i16,
        area: Size,
    ) -> Result<(), LayoutError> {
        match drag(&mut self.root, pane, dir, delta, area) {
            Search::Missing => Err(LayoutError::UnknownPane(pane)),
            Search::Found => Err(LayoutError::NoSplit { pane, dir }),
            Search::Applied => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

/// Whether a resolved size honors [`MIN_PANE_SIZE`] on both axes.
fn fits(size: Size) -> bool {
    size.cols >= MIN_PANE_SIZE.cols && size.rows >= MIN_PANE_SIZE.rows
}

/// Divides one axis at `ratio`, keeping both sides at least one cell when the
/// extent allows it. The single source of truth for how a ratio becomes cells —
/// [`Layout::resolve`] and the minimum-size check must never disagree.
fn split_extent(extent: u16, ratio: f32) -> (u16, u16) {
    if extent == 0 {
        return (0, 0);
    }
    let raw = (f32::from(extent) * ratio).round();
    // The clamp bounds the value into `u16` range before the cast.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut first = raw.clamp(0.0, f32::from(extent)) as u16;
    if extent >= 2 {
        first = first.clamp(1, extent - 1);
    }
    (first, extent - first)
}

/// The two child sizes a split of `size` along `dir` at `ratio` would produce.
fn halves(size: Size, dir: Direction, ratio: f32) -> (Size, Size) {
    match dir {
        Direction::Horizontal => {
            let (a, b) = split_extent(size.cols, ratio);
            (Size::new(a, size.rows), Size::new(b, size.rows))
        }
        Direction::Vertical => {
            let (a, b) = split_extent(size.rows, ratio);
            (Size::new(size.cols, a), Size::new(size.cols, b))
        }
    }
}

/// The `(start, end)` of a rectangle on the axis `side` moves along, and on the
/// axis it does not.
fn spans(rect: &PaneRect, side: Side) -> ((u16, u16), (u16, u16)) {
    let along = match side.axis() {
        Direction::Horizontal => (rect.x, rect.x.saturating_add(rect.size.cols)),
        Direction::Vertical => (rect.y, rect.y.saturating_add(rect.size.rows)),
    };
    let across = match side.axis() {
        Direction::Horizontal => (rect.y, rect.y.saturating_add(rect.size.rows)),
        Direction::Vertical => (rect.x, rect.x.saturating_add(rect.size.cols)),
    };
    (along, across)
}

/// How far `candidate` sits `side` of `origin`, or `None` if it does not.
///
/// `None` covers both "it is not on that side at all" and "it is, but shares no
/// row or column with the origin" — a pane diagonally across the tab is not what
/// a user means by *left*, and answering with one would make focus jump.
fn distance(origin: &PaneRect, candidate: &PaneRect, side: Side) -> Option<u16> {
    let (origin_along, origin_across) = spans(origin, side);
    let (candidate_along, candidate_across) = spans(candidate, side);

    // A half-open span touching the origin's edge overlaps nothing, so a zero
    // extent on the perpendicular axis never matches. That is the same
    // degenerate case `resolve` squeezes, and silence beats a wrong neighbor.
    if origin_across.0 >= candidate_across.1 || candidate_across.0 >= origin_across.1 {
        return None;
    }

    match side {
        Side::Left | Side::Up => (candidate_along.1 <= origin_along.0)
            .then(|| origin_along.0.saturating_sub(candidate_along.1)),
        Side::Right | Side::Down => (candidate_along.0 >= origin_along.1)
            .then(|| candidate_along.0.saturating_sub(origin_along.1)),
    }
}

/// How far `candidate`'s leading edge is from `origin`'s on the perpendicular
/// axis. The tie-break between two equally near neighbors.
fn offset(origin: &PaneRect, candidate: &PaneRect, side: Side) -> u16 {
    let (_, origin_across) = spans(origin, side);
    let (_, candidate_across) = spans(candidate, side);
    origin_across.0.abs_diff(candidate_across.0)
}

/// Walks the tree assigning each leaf a concrete rectangle.
fn assign(node: &Node, x: u16, y: u16, size: Size, out: &mut Vec<PaneRect>) {
    match node {
        Node::Leaf(pane) => out.push(PaneRect {
            pane: *pane,
            x,
            y,
            size,
        }),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = halves(size, *dir, *ratio);
            match dir {
                Direction::Horizontal => {
                    assign(first, x, y, a, out);
                    assign(second, x.saturating_add(a.cols), y, b, out);
                }
                Direction::Vertical => {
                    assign(first, x, y, a, out);
                    assign(second, x, y.saturating_add(a.rows), b, out);
                }
            }
        }
    }
}

/// Replaces the leaf holding `target` with a split of `target` and `new_pane`.
fn replace_leaf(
    node: &mut Node,
    target: PaneId,
    dir: Direction,
    ratio: f32,
    new_pane: PaneId,
) -> bool {
    match node {
        Node::Leaf(pane) if *pane == target => {
            *node = Node::Split {
                dir,
                ratio,
                first: Box::new(Node::Leaf(target)),
                second: Box::new(Node::Leaf(new_pane)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => {
            replace_leaf(first, target, dir, ratio, new_pane)
                || replace_leaf(second, target, dir, ratio, new_pane)
        }
    }
}

/// Removes `pane` from this subtree, promoting its sibling into the parent slot.
fn collapse(node: &mut Node, pane: PaneId) -> bool {
    let survivor = match node {
        Node::Leaf(_) => return false,
        Node::Split { first, second, .. } => {
            if matches!(**first, Node::Leaf(p) if p == pane) {
                Some((**second).clone())
            } else if matches!(**second, Node::Leaf(p) if p == pane) {
                Some((**first).clone())
            } else {
                None
            }
        }
    };

    if let Some(survivor) = survivor {
        *node = survivor;
        return true;
    }

    match node {
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => collapse(first, pane) || collapse(second, pane),
    }
}

/// The result of hunting for a pane's nearest ancestor split along an axis.
enum Search {
    /// The pane is not in this subtree.
    Missing,
    /// The pane is here, but no ancestor within this subtree splits on the axis.
    Found,
    /// The ratio was applied.
    Applied,
}

/// Finds `pane`, then sets the ratio of the first ancestor splitting on `dir`.
fn adjust(node: &mut Node, pane: PaneId, dir: Direction, ratio: f32) -> Search {
    match node {
        Node::Leaf(p) => {
            if *p == pane {
                Search::Found
            } else {
                Search::Missing
            }
        }
        Node::Split {
            dir: node_dir,
            ratio: node_ratio,
            first,
            second,
        } => {
            let mut found = adjust(first, pane, dir, ratio);
            if matches!(found, Search::Missing) {
                found = adjust(second, pane, dir, ratio);
            }
            if matches!(found, Search::Found) && *node_dir == dir {
                *node_ratio = ratio;
                return Search::Applied;
            }
            found
        }
    }
}

/// The extent of `size` along `dir` — the axis a split on `dir` divides.
const fn extent_along(size: Size, dir: Direction) -> u16 {
    match dir {
        Direction::Horizontal => size.cols,
        Direction::Vertical => size.rows,
    }
}

/// Finds `pane`, then moves the first ancestor divider along `dir` by `delta`
/// cells in whichever direction grows `pane`.
///
/// `size` is the area this node resolves into, carried down the same way
/// [`assign`] carries it, because a ratio only means cells relative to the split
/// it belongs to. Everything above that split is untouched: the divider moves
/// inside its own extent, so the panes on the far side of an outer split do not
/// shift by a cell.
fn drag(node: &mut Node, pane: PaneId, dir: Direction, delta: i16, size: Size) -> Search {
    match node {
        Node::Leaf(p) => {
            if *p == pane {
                Search::Found
            } else {
                Search::Missing
            }
        }
        Node::Split {
            dir: node_dir,
            ratio: node_ratio,
            first,
            second,
        } => {
            let (a, b) = halves(size, *node_dir, *node_ratio);
            let mut in_first = true;
            let mut found = drag(first, pane, dir, delta, a);
            if matches!(found, Search::Missing) {
                in_first = false;
                found = drag(second, pane, dir, delta, b);
            }
            if matches!(found, Search::Found) && *node_dir == dir {
                let extent = extent_along(size, dir);
                if extent >= 2 {
                    let held = extent_along(a, dir);
                    // Growing the second child means shrinking the first, which
                    // is the only asymmetry in a drag: the ratio always names
                    // the first child's share.
                    let signed = if in_first {
                        i32::from(delta)
                    } else {
                        -i32::from(delta)
                    };
                    let minimum = extent_along(MIN_PANE_SIZE, dir);
                    let (low, high) = if extent >= minimum.saturating_mul(2) {
                        (i32::from(minimum), i32::from(extent - minimum))
                    } else {
                        (1, i32::from(extent) - 1)
                    };
                    let target = (i32::from(held) + signed).clamp(low, high);
                    // `target` is inside `[1, extent - 1]`, so the quotient is
                    // strictly inside `(0.0, 1.0)` — the same interval `split`
                    // and `set_ratio` refuse anything outside of.
                    #[allow(clippy::cast_precision_loss)]
                    let next = target as f32 / f32::from(extent);
                    *node_ratio = next;
                }
                // An extent too small to divide leaves the ratio alone. The
                // layout pass squeezes such an area anyway, and inventing a
                // ratio for it would survive into the resize that gives the
                // split room again.
                return Search::Applied;
            }
            found
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Size = Size::new(120, 40);

    fn pane(raw: u64) -> PaneId {
        PaneId::new(raw)
    }

    fn rect(p: u64, x: u16, y: u16, cols: u16, rows: u16) -> PaneRect {
        PaneRect {
            pane: pane(p),
            x,
            y,
            size: Size::new(cols, rows),
        }
    }

    /// Builds a layout by applying a sequence of even splits.
    fn build(splits: &[(u64, Direction, u64)]) -> Layout {
        let mut layout = Layout::new(pane(0));
        for &(target, dir, new) in splits {
            layout
                .split_even(pane(target), dir, pane(new), AREA)
                .expect("fixture split fits");
        }
        layout
    }

    #[test]
    fn a_new_layout_is_one_full_area_pane() {
        let layout = Layout::new(pane(0));
        assert_eq!(layout.len(), 1);
        assert!(!layout.is_empty());
        assert_eq!(layout.resolve(AREA), vec![rect(0, 0, 0, 120, 40)]);
    }

    #[test]
    fn splits_assign_tiling_rectangles() {
        struct Case {
            name: &'static str,
            splits: &'static [(u64, Direction, u64)],
            expected: &'static [(u64, u16, u16, u16, u16)],
        }

        let cases = [
            Case {
                name: "horizontal split halves the columns",
                splits: &[(0, Direction::Horizontal, 1)],
                expected: &[(0, 0, 0, 60, 40), (1, 60, 0, 60, 40)],
            },
            Case {
                name: "vertical split halves the rows",
                splits: &[(0, Direction::Vertical, 1)],
                expected: &[(0, 0, 0, 120, 20), (1, 0, 20, 120, 20)],
            },
            Case {
                name: "splitting the new pane nests to the right",
                splits: &[(0, Direction::Horizontal, 1), (1, Direction::Horizontal, 2)],
                expected: &[(0, 0, 0, 60, 40), (1, 60, 0, 30, 40), (2, 90, 0, 30, 40)],
            },
            Case {
                name: "mixed axes produce a quad",
                splits: &[
                    (0, Direction::Horizontal, 1),
                    (0, Direction::Vertical, 2),
                    (1, Direction::Vertical, 3),
                ],
                expected: &[
                    (0, 0, 0, 60, 20),
                    (2, 0, 20, 60, 20),
                    (1, 60, 0, 60, 20),
                    (3, 60, 20, 60, 20),
                ],
            },
        ];

        for case in cases {
            let layout = build(case.splits);
            let want: Vec<PaneRect> = case
                .expected
                .iter()
                .map(|&(p, x, y, c, r)| rect(p, x, y, c, r))
                .collect();
            assert_eq!(layout.resolve(AREA), want, "{}", case.name);
        }
    }

    #[test]
    fn resolved_rectangles_tile_the_area_exactly() {
        let layout = build(&[
            (0, Direction::Horizontal, 1),
            (0, Direction::Vertical, 2),
            (1, Direction::Vertical, 3),
            (3, Direction::Horizontal, 4),
        ]);
        let area = Size::new(121, 41);
        let covered: u32 = layout
            .resolve(area)
            .iter()
            .map(|r| u32::from(r.size.cols) * u32::from(r.size.rows))
            .sum();
        assert_eq!(covered, u32::from(area.cols) * u32::from(area.rows));
    }

    #[test]
    fn an_uneven_ratio_rounds_without_losing_a_cell() {
        let mut layout = Layout::new(pane(0));
        layout
            .split(pane(0), Direction::Horizontal, 0.25, pane(1), AREA)
            .expect("quarter split fits");
        assert_eq!(
            layout.resolve(AREA),
            vec![rect(0, 0, 0, 30, 40), rect(1, 30, 0, 90, 40)]
        );
    }

    #[test]
    fn splits_that_would_violate_the_minimum_are_rejected() {
        struct Case {
            name: &'static str,
            area: Size,
            dir: Direction,
            ratio: f32,
        }

        let cases = [
            Case {
                name: "zero area leaves nothing to divide",
                area: Size::new(0, 0),
                dir: Direction::Horizontal,
                ratio: 0.5,
            },
            Case {
                name: "one column short of two minimum panes",
                area: Size::new(2 * MIN_PANE_SIZE.cols - 1, 40),
                dir: Direction::Horizontal,
                ratio: 0.5,
            },
            Case {
                name: "one row short of two minimum panes",
                area: Size::new(120, 2 * MIN_PANE_SIZE.rows - 1),
                dir: Direction::Vertical,
                ratio: 0.5,
            },
            Case {
                name: "an extreme ratio starves the first child",
                area: AREA,
                dir: Direction::Horizontal,
                ratio: 0.01,
            },
            Case {
                name: "an extreme ratio starves the second child",
                area: AREA,
                dir: Direction::Horizontal,
                ratio: 0.99,
            },
            Case {
                name: "the cross axis is too short to split at all",
                area: Size::new(120, MIN_PANE_SIZE.rows - 1),
                dir: Direction::Horizontal,
                ratio: 0.5,
            },
        ];

        for case in cases {
            let mut layout = Layout::new(pane(0));
            let before = layout.clone();
            let err = layout
                .split(pane(0), case.dir, case.ratio, pane(1), case.area)
                .expect_err(case.name);
            assert!(
                matches!(err, LayoutError::TooSmall { pane: p, .. } if p == pane(0)),
                "{}: unexpected error {err:?}",
                case.name
            );
            assert_eq!(layout, before, "{}: layout must be unchanged", case.name);
        }
    }

    #[test]
    fn a_split_at_exactly_twice_the_minimum_is_accepted() {
        let area = Size::new(2 * MIN_PANE_SIZE.cols, MIN_PANE_SIZE.rows);
        let mut layout = Layout::new(pane(0));
        layout
            .split_even(pane(0), Direction::Horizontal, pane(1), area)
            .expect("exactly two minimum panes fit");
        assert_eq!(
            layout.resolve(area),
            vec![
                rect(0, 0, 0, MIN_PANE_SIZE.cols, MIN_PANE_SIZE.rows),
                rect(
                    1,
                    MIN_PANE_SIZE.cols,
                    0,
                    MIN_PANE_SIZE.cols,
                    MIN_PANE_SIZE.rows
                ),
            ]
        );
    }

    #[test]
    fn splitting_rejects_unknown_targets_and_duplicate_panes() {
        let mut layout = Layout::new(pane(0));
        let before = layout.clone();

        let err = layout
            .split_even(pane(7), Direction::Horizontal, pane(1), AREA)
            .expect_err("pane 7 does not exist");
        assert_eq!(err, LayoutError::UnknownPane(pane(7)));

        let err = layout
            .split_even(pane(0), Direction::Horizontal, pane(0), AREA)
            .expect_err("pane 0 is already in the layout");
        assert_eq!(err, LayoutError::DuplicatePane(pane(0)));

        assert_eq!(layout, before);
    }

    #[test]
    fn splitting_rejects_ratios_outside_the_open_unit_interval() {
        for ratio in [0.0, 1.0, -0.5, 1.5, f32::NAN, f32::INFINITY] {
            let mut layout = Layout::new(pane(0));
            let before = layout.clone();
            let err = layout
                .split(pane(0), Direction::Horizontal, ratio, pane(1), AREA)
                .expect_err("ratio must be inside (0, 1)");
            assert!(
                matches!(err, LayoutError::InvalidRatio(_)),
                "ratio {ratio} gave {err:?}"
            );
            assert_eq!(layout, before);
        }
    }

    #[test]
    fn closing_a_pane_collapses_its_parent_and_promotes_the_sibling() {
        struct Case {
            name: &'static str,
            splits: &'static [(u64, Direction, u64)],
            close: u64,
            expected: &'static [(u64, u16, u16, u16, u16)],
        }

        let cases = [
            Case {
                name: "closing one of two restores a full-area pane",
                splits: &[(0, Direction::Horizontal, 1)],
                close: 1,
                expected: &[(0, 0, 0, 120, 40)],
            },
            Case {
                name: "closing the first child promotes the second",
                splits: &[(0, Direction::Horizontal, 1)],
                close: 0,
                expected: &[(1, 0, 0, 120, 40)],
            },
            Case {
                name: "closing a nested leaf promotes its sibling subtree",
                splits: &[(0, Direction::Horizontal, 1), (1, Direction::Vertical, 2)],
                close: 0,
                expected: &[(1, 0, 0, 120, 20), (2, 0, 20, 120, 20)],
            },
            Case {
                name: "closing a deep leaf leaves the rest of the tree intact",
                splits: &[
                    (0, Direction::Horizontal, 1),
                    (0, Direction::Vertical, 2),
                    (1, Direction::Vertical, 3),
                ],
                close: 2,
                expected: &[(0, 0, 0, 60, 40), (1, 60, 0, 60, 20), (3, 60, 20, 60, 20)],
            },
        ];

        for case in cases {
            let mut layout = build(case.splits);
            layout.close(pane(case.close)).expect(case.name);
            let want: Vec<PaneRect> = case
                .expected
                .iter()
                .map(|&(p, x, y, c, r)| rect(p, x, y, c, r))
                .collect();
            assert_eq!(layout.resolve(AREA), want, "{}", case.name);
            assert!(!layout.contains(pane(case.close)), "{}", case.name);
        }
    }

    /// The property a caller relies on to undo a split it cannot complete.
    ///
    /// `cloo-server`'s session splits the layout first and spawns the pane's
    /// child second, because the layout is the half that can refuse and a
    /// refusal must not cost a process. When the spawn then fails, closing the
    /// new pane is the rollback — so it has to restore the tree *exactly*,
    /// ratios included, and not merely leave the same panes in it.
    #[test]
    fn closing_a_freshly_split_pane_restores_the_tree_exactly() {
        let starts = [
            &[][..],
            &[(0, Direction::Horizontal, 1)][..],
            &[(0, Direction::Horizontal, 1), (1, Direction::Vertical, 2)][..],
        ];

        for splits in starts {
            for dir in [Direction::Horizontal, Direction::Vertical] {
                for ratio in [0.4, 0.5, 0.6] {
                    let mut layout = build(splits);
                    let before = layout.clone();
                    layout
                        .split(pane(0), dir, ratio, pane(7), AREA)
                        .expect("the fixture areas all have room");
                    assert_ne!(layout, before, "the split must have changed something");

                    layout
                        .close(pane(7))
                        .expect("the new pane must be closable");
                    assert_eq!(
                        layout, before,
                        "rolling back a split at {ratio} on {dir:?} must restore the \
                         tree exactly, not merely the same set of panes"
                    );
                }
            }
        }
    }

    #[test]
    fn closing_the_last_pane_is_refused() {
        let mut layout = Layout::new(pane(0));
        let before = layout.clone();
        assert_eq!(
            layout
                .close(pane(0))
                .expect_err("the last pane must survive"),
            LayoutError::LastPane(pane(0))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn closing_an_unknown_pane_is_refused() {
        for splits in [&[][..], &[(0, Direction::Horizontal, 1)][..]] {
            let mut layout = build(splits);
            let before = layout.clone();
            assert_eq!(
                layout.close(pane(9)).expect_err("pane 9 does not exist"),
                LayoutError::UnknownPane(pane(9))
            );
            assert_eq!(layout, before);
        }
    }

    #[test]
    fn set_ratio_adjusts_the_nearest_ancestor_split_on_that_axis() {
        let mut layout = build(&[(0, Direction::Horizontal, 1), (1, Direction::Vertical, 2)]);

        layout
            .set_ratio(pane(2), Direction::Horizontal, 0.25)
            .expect("pane 2 has a horizontal ancestor");
        assert_eq!(
            layout.resolve(AREA),
            vec![
                rect(0, 0, 0, 30, 40),
                rect(1, 30, 0, 90, 20),
                rect(2, 30, 20, 90, 20),
            ]
        );

        layout
            .set_ratio(pane(2), Direction::Vertical, 0.75)
            .expect("pane 2 has a vertical parent");
        assert_eq!(
            layout.resolve(AREA),
            vec![
                rect(0, 0, 0, 30, 40),
                rect(1, 30, 0, 90, 30),
                rect(2, 30, 30, 90, 10),
            ]
        );
    }

    #[test]
    fn set_ratio_rejects_missing_panes_axes_and_ratios() {
        let mut layout = build(&[(0, Direction::Horizontal, 1)]);
        let before = layout.clone();

        assert_eq!(
            layout
                .set_ratio(pane(9), Direction::Horizontal, 0.4)
                .expect_err("pane 9 does not exist"),
            LayoutError::UnknownPane(pane(9))
        );
        assert_eq!(
            layout
                .set_ratio(pane(0), Direction::Vertical, 0.4)
                .expect_err("no vertical ancestor"),
            LayoutError::NoSplit {
                pane: pane(0),
                dir: Direction::Vertical
            }
        );
        assert!(matches!(
            layout
                .set_ratio(pane(0), Direction::Horizontal, 0.0)
                .expect_err("zero is not a legal ratio"),
            LayoutError::InvalidRatio(_)
        ));
        assert_eq!(layout, before);
    }

    /// A drag is a ratio and nothing else. The whole point of the gutter is that
    /// dragging it can never do what a split or a close does, so this asserts on
    /// the *shape* of the tree as well as on the cells it resolves into.
    #[test]
    fn a_resize_moves_one_divider_and_changes_nothing_else() {
        let mut layout = build(&[(0, Direction::Horizontal, 1), (1, Direction::Vertical, 2)]);
        let before = layout.clone();

        layout
            .resize(pane(0), Direction::Horizontal, 12, AREA)
            .expect("pane 0 has a horizontal ancestor");

        assert_eq!(
            layout.resolve(AREA),
            vec![
                rect(0, 0, 0, 72, 40),
                rect(1, 72, 0, 48, 20),
                rect(2, 72, 20, 48, 20),
            ],
            "the divider moves by exactly the cells asked for, and the far side keeps its own"
        );
        assert_eq!(layout.panes(), before.panes(), "no pane may move or vanish");
        assert_eq!(
            shape(layout.root()),
            shape(before.root()),
            "a drag may not reshape the tree"
        );
        assert_eq!(layout.zoomed(), before.zoomed());
    }

    /// The one asymmetry: the ratio names the *first* child's share, so a pane on
    /// the second side of the divider grows when the ratio falls.
    #[test]
    fn resizing_from_either_side_of_a_divider_moves_it_the_same_way() {
        let mut left = build(&[(0, Direction::Horizontal, 1)]);
        let mut right = left.clone();

        left.resize(pane(0), Direction::Horizontal, 10, AREA)
            .expect("pane 0 grows");
        right
            .resize(pane(1), Direction::Horizontal, -10, AREA)
            .expect("pane 1 shrinks by as much");

        assert_eq!(left.resolve(AREA), right.resolve(AREA));
        assert_eq!(left.resolve(AREA)[0].size.cols, 70);
    }

    /// Vertical stacks are the header-row drag, and they must respect the same
    /// minimum a split does rather than letting a pointer squeeze a pane to
    /// nothing.
    #[test]
    fn a_resize_stops_at_the_minimum_pane_size_rather_than_being_refused() {
        let mut layout = build(&[(0, Direction::Vertical, 1)]);

        layout
            .resize(pane(0), Direction::Vertical, 500, AREA)
            .expect("a drag past the end still applies");
        let rects = layout.resolve(AREA);
        assert_eq!(rects[0].size.rows, AREA.rows - MIN_PANE_SIZE.rows);
        assert_eq!(rects[1].size.rows, MIN_PANE_SIZE.rows);

        layout
            .resize(pane(0), Direction::Vertical, -500, AREA)
            .expect("and so does one past the other end");
        let rects = layout.resolve(AREA);
        assert_eq!(rects[0].size.rows, MIN_PANE_SIZE.rows);
        assert_eq!(rects[1].size.rows, AREA.rows - MIN_PANE_SIZE.rows);
    }

    /// An area no split could have been made in still has to answer. The floor
    /// drops to one cell a side, which is what `resolve` squeezes to anyway.
    #[test]
    fn a_resize_in_an_area_below_the_minimum_keeps_every_pane_drawable() {
        let mut layout = build(&[(0, Direction::Horizontal, 1)]);
        let tiny = Size::new(5, 2);

        layout
            .resize(pane(0), Direction::Horizontal, 99, tiny)
            .expect("a cramped area still resizes");

        let rects = layout.resolve(tiny);
        assert_eq!(rects.len(), 2);
        for r in &rects {
            assert!(
                r.size.cols >= 1,
                "no pane may resolve to zero columns: {r:?}"
            );
        }
    }

    /// A zero-column area cannot express a divider at all, and the ratio it had
    /// must survive so the next resize draws what it did before.
    #[test]
    fn a_resize_in_an_undividable_extent_leaves_the_ratio_alone() {
        let mut layout = build(&[(0, Direction::Horizontal, 1)]);
        let before = layout.clone();

        layout
            .resize(pane(0), Direction::Horizontal, 4, Size::new(1, 40))
            .expect("an undividable extent is not an error");

        assert_eq!(layout, before);
    }

    #[test]
    fn a_resize_rejects_missing_panes_and_missing_axes() {
        let mut layout = build(&[(0, Direction::Horizontal, 1)]);
        let before = layout.clone();

        assert_eq!(
            layout
                .resize(pane(9), Direction::Horizontal, 3, AREA)
                .expect_err("pane 9 does not exist"),
            LayoutError::UnknownPane(pane(9))
        );
        assert_eq!(
            layout
                .resize(pane(0), Direction::Vertical, 3, AREA)
                .expect_err("no vertical ancestor"),
            LayoutError::NoSplit {
                pane: pane(0),
                dir: Direction::Vertical
            }
        );
        assert_eq!(layout, before);
    }

    /// The tree's shape with every ratio erased: what a drag must not change.
    fn shape(node: &Node) -> String {
        match node {
            Node::Leaf(pane) => format!("{pane}"),
            Node::Split {
                dir, first, second, ..
            } => format!("({dir:?} {} {})", shape(first), shape(second)),
        }
    }

    #[test]
    fn a_shrunken_area_squeezes_panes_instead_of_dropping_them() {
        let layout = build(&[(0, Direction::Horizontal, 1), (1, Direction::Horizontal, 2)]);
        let rects = layout.resolve(Size::new(4, 1));
        assert_eq!(rects.len(), 3);
        for r in &rects {
            assert!(
                r.size.cols >= 1,
                "no pane may resolve to zero columns: {r:?}"
            );
            assert!(r.size.rows >= 1, "no pane may resolve to zero rows: {r:?}");
        }
    }

    #[test]
    fn a_zero_area_resolves_without_panicking() {
        let layout = build(&[(0, Direction::Horizontal, 1)]);
        let rects = layout.resolve(Size::new(0, 0));
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|r| r.size == Size::new(0, 0)));
    }

    #[test]
    fn panes_are_listed_in_traversal_order() {
        let layout = build(&[
            (0, Direction::Horizontal, 1),
            (0, Direction::Vertical, 2),
            (1, Direction::Vertical, 3),
        ]);
        assert_eq!(
            layout.panes(),
            vec![pane(0), pane(2), pane(1), pane(3)],
            "traversal order must match resolve order"
        );
        assert_eq!(layout.len(), 4);
    }

    #[test]
    fn rect_of_matches_the_full_layout_pass() {
        let layout = build(&[(0, Direction::Horizontal, 1)]);
        assert_eq!(layout.rect_of(pane(1), AREA), Some(rect(1, 60, 0, 60, 40)));
        assert_eq!(layout.rect_of(pane(9), AREA), None);
    }

    // -- directional focus --------------------------------------------------

    /// The quad every traversal case below is stated against:
    ///
    /// ```text
    ///     0 | 1
    ///     --+--
    ///     2 | 3
    /// ```
    fn quad() -> Layout {
        build(&[
            (0, Direction::Horizontal, 1),
            (0, Direction::Vertical, 2),
            (1, Direction::Vertical, 3),
        ])
    }

    #[test]
    fn focus_moves_to_the_pane_a_user_sees_in_that_direction() {
        struct Case {
            from: u64,
            side: Side,
            expected: Option<u64>,
        }

        let cases = [
            Case {
                from: 0,
                side: Side::Right,
                expected: Some(1),
            },
            Case {
                from: 0,
                side: Side::Down,
                expected: Some(2),
            },
            Case {
                from: 0,
                side: Side::Left,
                expected: None,
            },
            Case {
                from: 0,
                side: Side::Up,
                expected: None,
            },
            Case {
                from: 3,
                side: Side::Left,
                expected: Some(2),
            },
            Case {
                from: 3,
                side: Side::Up,
                expected: Some(1),
            },
            Case {
                from: 3,
                side: Side::Right,
                expected: None,
            },
            Case {
                from: 3,
                side: Side::Down,
                expected: None,
            },
        ];

        let layout = quad();
        for case in cases {
            assert_eq!(
                layout.neighbor(pane(case.from), case.side, AREA),
                case.expected.map(pane),
                "from {} going {:?}",
                case.from,
                case.side
            );
        }
    }

    /// The case a structural traversal gets wrong: pane 1's sibling in the tree
    /// is the *subtree* holding 2 and 3, and only geometry says which of them is
    /// actually below it.
    #[test]
    fn traversal_is_geometric_rather_than_structural() {
        // 0 on the left; 1 above 2 above 3 on the right.
        let layout = build(&[
            (0, Direction::Horizontal, 1),
            (1, Direction::Vertical, 2),
            (2, Direction::Vertical, 3),
        ]);
        assert_eq!(layout.neighbor(pane(1), Side::Down, AREA), Some(pane(2)));
        assert_eq!(layout.neighbor(pane(3), Side::Up, AREA), Some(pane(2)));
        assert_eq!(
            layout.neighbor(pane(2), Side::Left, AREA),
            Some(pane(0)),
            "the pane on the left spans all three, so every one of them reaches it"
        );
        assert_eq!(
            layout.neighbor(pane(0), Side::Right, AREA),
            Some(pane(1)),
            "three panes are equally near; the one nearest the origin's own top \
             edge wins"
        );
    }

    #[test]
    fn a_single_pane_has_no_neighbor_in_any_direction() {
        let layout = Layout::new(pane(0));
        for side in [Side::Left, Side::Right, Side::Up, Side::Down] {
            assert_eq!(layout.neighbor(pane(0), side, AREA), None, "{side:?}");
        }
    }

    #[test]
    fn an_unknown_pane_has_no_neighbors() {
        assert_eq!(quad().neighbor(pane(9), Side::Left, AREA), None);
    }

    #[test]
    fn a_diagonal_pane_is_never_a_neighbor() {
        // Pane 3 is diagonally across from pane 0, and shares neither a row nor
        // a column with it. Going right must find 1, going down must find 2, and
        // neither may find 3.
        let layout = quad();
        for side in [Side::Right, Side::Down] {
            assert_ne!(
                layout.neighbor(pane(0), side, AREA),
                Some(pane(3)),
                "{side:?} must not jump the diagonal"
            );
        }
    }

    #[test]
    fn traversal_never_answers_with_the_pane_it_started_from() {
        let layout = quad();
        for from in [0, 1, 2, 3] {
            for side in [Side::Left, Side::Right, Side::Up, Side::Down] {
                assert_ne!(layout.neighbor(pane(from), side, AREA), Some(pane(from)));
            }
        }
    }

    #[test]
    fn sides_name_the_axis_they_move_along() {
        assert_eq!(Side::Left.axis(), Direction::Horizontal);
        assert_eq!(Side::Right.axis(), Direction::Horizontal);
        assert_eq!(Side::Up.axis(), Direction::Vertical);
        assert_eq!(Side::Down.axis(), Direction::Vertical);
    }

    // -- zoom ---------------------------------------------------------------

    #[test]
    fn zoom_shows_one_pane_at_the_full_area() {
        let mut layout = quad();
        layout.zoom(pane(3)).expect("pane 3 is in the layout");
        assert_eq!(layout.zoomed(), Some(pane(3)));
        assert_eq!(layout.resolve(AREA), vec![rect(3, 0, 0, 120, 40)]);
        assert_eq!(
            layout.rect_of(pane(0), AREA),
            None,
            "a hidden pane has no rectangle to draw"
        );
        assert_eq!(
            layout.panes().len(),
            4,
            "zoom hides panes from the view, never from the layout"
        );
    }

    /// The property the whole feature rests on: zoom is a view flag, so undoing
    /// it restores the tree — ratios included — rather than rebuilding it.
    #[test]
    fn zoom_and_unzoom_preserve_every_split_ratio() {
        let mut layout = quad();
        layout
            .set_ratio(pane(0), Direction::Horizontal, 0.3)
            .expect("pane 0 has a horizontal ancestor");
        layout
            .set_ratio(pane(3), Direction::Vertical, 0.8)
            .expect("pane 3 has a vertical parent");
        let before = layout.clone();
        let geometry = layout.resolve(AREA);

        for target in [0, 1, 2, 3] {
            layout.zoom(pane(target)).expect("every pane is zoomable");
            assert_ne!(layout.resolve(AREA), geometry, "zoom must change the view");
            layout.unzoom();
            assert_eq!(
                layout, before,
                "unzoom must restore the tree exactly, ratios included"
            );
            assert_eq!(layout.resolve(AREA), geometry);
        }
    }

    #[test]
    fn zoom_is_idempotent_and_unzoom_is_too() {
        let mut layout = quad();
        layout.zoom(pane(1)).expect("pane 1 exists");
        let zoomed = layout.clone();
        layout.zoom(pane(1)).expect("pane 1 exists");
        assert_eq!(layout, zoomed);

        layout.unzoom();
        let plain = layout.clone();
        layout.unzoom();
        assert_eq!(layout, plain);
    }

    #[test]
    fn toggling_zoom_off_does_not_care_which_pane_asked() {
        let mut layout = quad();
        assert!(
            layout.toggle_zoom(pane(0)).expect("pane 0 exists"),
            "the first toggle zooms"
        );
        assert_eq!(layout.zoomed(), Some(pane(0)));
        assert!(
            !layout.toggle_zoom(pane(3)).expect("pane 3 exists"),
            "a toggle means undo the zoom, whichever pane it was on"
        );
        assert_eq!(layout.zoomed(), None);
    }

    #[test]
    fn zooming_an_unknown_pane_is_refused_and_changes_nothing() {
        let mut layout = quad();
        let before = layout.clone();
        assert_eq!(
            layout.zoom(pane(9)).expect_err("pane 9 does not exist"),
            LayoutError::UnknownPane(pane(9))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn closing_the_zoomed_pane_unzooms_and_closing_another_does_not() {
        let mut layout = quad();
        layout.zoom(pane(3)).expect("pane 3 exists");
        layout.close(pane(3)).expect("pane 3 is not the last one");
        assert_eq!(layout.zoomed(), None, "the zoomed pane is gone");

        let mut layout = quad();
        layout.zoom(pane(3)).expect("pane 3 exists");
        layout.close(pane(0)).expect("pane 0 is not the last one");
        assert_eq!(
            layout.zoomed(),
            Some(pane(3)),
            "closing some other pane leaves the zoom where it was"
        );
    }

    #[test]
    fn a_split_unzooms_and_is_measured_against_the_real_geometry() {
        // Two 60-column panes; a zoom lends pane 0 all 120. Splitting it must be
        // judged on the 60 it actually has.
        let mut layout = build(&[(0, Direction::Horizontal, 1)]);
        layout.zoom(pane(0)).expect("pane 0 exists");

        let err = layout
            .split_even(pane(0), Direction::Horizontal, pane(2), Size::new(60, 40))
            .expect_err("30 columns is below the 20-column minimum twice over");
        assert!(
            matches!(err, LayoutError::TooSmall { .. }),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            layout.zoomed(),
            Some(pane(0)),
            "a refused split changes nothing at all, zoom included"
        );

        layout
            .split_even(pane(0), Direction::Horizontal, pane(2), AREA)
            .expect("30 columns each fits at the full area");
        assert_eq!(
            layout.zoomed(),
            None,
            "a split that changed the shape must show it"
        );
    }
}
