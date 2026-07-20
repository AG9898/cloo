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

use cloo_proto::{Direction, PaneId, PaneRect, Size};

use crate::error::LayoutError;

/// The smallest pane cloo will create.
///
/// Chosen so a shell prompt and a line of output remain legible. Chrome is drawn
/// client-side and costs nothing here — these are grid cells the child program
/// actually gets.
pub const MIN_PANE_SIZE: Size = Size::new(20, 3);

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
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    root: Node,
}

impl Layout {
    /// A layout holding a single full-area pane.
    #[must_use]
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: Node::Leaf(pane),
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
    #[must_use]
    pub fn resolve(&self, area: Size) -> Vec<PaneRect> {
        let mut out = Vec::new();
        assign(&self.root, 0, 0, area, &mut out);
        out
    }

    /// The resolved rectangle of a single pane, or `None` if it is not present.
    #[must_use]
    pub fn rect_of(&self, pane: PaneId, area: Size) -> Option<PaneRect> {
        self.resolve(area).into_iter().find(|r| r.pane == pane)
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
            .rect_of(target, area)
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
    pub fn close(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        if let Node::Leaf(only) = self.root {
            return if only == pane {
                Err(LayoutError::LastPane(pane))
            } else {
                Err(LayoutError::UnknownPane(pane))
            };
        }
        if collapse(&mut self.root, pane) {
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
}
