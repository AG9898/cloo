//! One tab: a named layout tree with a focused pane.
//!
//! A tab is the unit a [`Session`](crate::session::Session) switches between. It
//! owns exactly one [`Layout`] — its own tree of panes — and a single focused
//! pane within that tree. Both are tab-local: splitting, zooming, or focusing in
//! one tab never touches another, which is the whole point of the abstraction.
//!
//! A tab always holds at least one pane, because its [`Layout`] always does.
//! There is no empty tab; closing the last pane in a tab is the caller's cue to
//! close the tab itself rather than to reach an empty layout.
//!
//! ```
//! use cloo_core::tab::{Tab, TabName};
//! use cloo_proto::{PaneId, TabId};
//!
//! let mut tab = Tab::new(
//!     TabId::new(0),
//!     TabName::new("build").expect("valid name"),
//!     PaneId::new(0),
//! );
//! assert_eq!(tab.focused(), PaneId::new(0));
//! assert_eq!(tab.name().as_str(), "build");
//! ```

use cloo_proto::{PaneId, TabId};

use crate::error::{LayoutError, MetadataError};
use crate::layout::Layout;
use crate::pane::validate_text;

/// The longest a tab name may be.
///
/// A tab name shares the single tab-bar row with every other tab, so a name
/// longer than this could never be shown whole regardless.
pub const MAX_TAB_NAME: usize = 64;

/// A tab's user-visible name.
///
/// Validated exactly as a [`PaneName`](crate::pane::PaneName): non-empty,
/// bounded, and free of control characters, because it too reaches chrome that
/// an escape sequence could repaint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabName(String);

impl TabName {
    /// Validates and wraps a tab name.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`], [`MetadataError::TooLong`], or
    /// [`MetadataError::BadChar`].
    pub fn new(name: impl Into<String>) -> Result<Self, MetadataError> {
        let name = name.into();
        validate_text("tab name", &name, MAX_TAB_NAME)?;
        Ok(Self(name))
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for TabName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A named layout tree with a focused pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    id: TabId,
    name: TabName,
    layout: Layout,
    focused: PaneId,
}

impl Tab {
    /// A tab holding a single full-area pane, focused.
    ///
    /// The pane is both the tab's only leaf and its focused pane, mirroring
    /// [`Layout::new`] — a tab is never born empty.
    #[must_use]
    pub fn new(id: TabId, name: TabName, pane: PaneId) -> Self {
        Self {
            id,
            name,
            layout: Layout::new(pane),
            focused: pane,
        }
    }

    /// The tab's stable identity.
    #[must_use]
    pub const fn id(&self) -> TabId {
        self.id
    }

    /// The tab's name.
    #[must_use]
    pub const fn name(&self) -> &TabName {
        &self.name
    }

    /// Replaces the tab's name.
    pub fn set_name(&mut self, name: TabName) {
        self.name = name;
    }

    /// The tab's layout tree, for a caller that needs to read or mutate it.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The tab's layout tree, mutably.
    ///
    /// Splitting or closing panes through this can leave [`Tab::focused`]
    /// pointing at a pane that is gone; call [`Tab::focus`] afterwards to move it
    /// somewhere still present.
    pub const fn layout_mut(&mut self) -> &mut Layout {
        &mut self.layout
    }

    /// The focused pane. Always a pane the layout still holds at construction;
    /// see [`Tab::layout_mut`] for the one way it can drift.
    #[must_use]
    pub const fn focused(&self) -> PaneId {
        self.focused
    }

    /// Moves focus to `pane`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if `pane` is not in this tab's layout. Focus
    /// is unchanged in that case.
    pub fn focus(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        if !self.layout.contains(pane) {
            return Err(LayoutError::UnknownPane(pane));
        }
        self.focused = pane;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> Tab {
        Tab::new(
            TabId::new(0),
            TabName::new("build").expect("valid name"),
            PaneId::new(0),
        )
    }

    #[test]
    fn a_new_tab_is_one_focused_pane() {
        let tab = tab();
        assert_eq!(tab.id(), TabId::new(0));
        assert_eq!(tab.name().as_str(), "build");
        assert_eq!(tab.focused(), PaneId::new(0));
        assert_eq!(tab.layout().len(), 1);
    }

    #[test]
    fn renaming_replaces_the_name() {
        let mut tab = tab();
        tab.set_name(TabName::new("test").expect("valid name"));
        assert_eq!(tab.name().as_str(), "test");
    }

    #[test]
    fn a_tab_name_is_validated_like_a_pane_name() {
        assert_eq!(TabName::new(""), Err(MetadataError::Empty("tab name")));
        assert!(matches!(
            TabName::new("esc\u{1b}[31m"),
            Err(MetadataError::BadChar { .. })
        ));
        assert!(matches!(
            TabName::new("a".repeat(MAX_TAB_NAME + 1)),
            Err(MetadataError::TooLong { .. })
        ));
        assert!(TabName::new("日本語").is_ok());
    }

    #[test]
    fn focus_moves_only_to_a_pane_the_tab_holds() {
        let mut tab = tab();
        tab.layout_mut()
            .split_even(
                PaneId::new(0),
                cloo_proto::Direction::Horizontal,
                PaneId::new(1),
                cloo_proto::Size::new(120, 40),
            )
            .expect("the split fits");

        tab.focus(PaneId::new(1)).expect("pane 1 is in the tab");
        assert_eq!(tab.focused(), PaneId::new(1));

        assert_eq!(
            tab.focus(PaneId::new(9))
                .expect_err("pane 9 does not exist"),
            LayoutError::UnknownPane(PaneId::new(9))
        );
        assert_eq!(
            tab.focused(),
            PaneId::new(1),
            "a refused focus changes nothing"
        );
    }
}
