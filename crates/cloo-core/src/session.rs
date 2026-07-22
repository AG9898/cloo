//! The session: an ordered set of tabs with exactly one active.
//!
//! A session is the top of the pure model. It owns its [`Tab`]s in tab-bar
//! order, allocates their [`TabId`]s, and tracks which one is active. Everything
//! here is pure and I/O-free — the server drives PTYs and the client renders, but
//! the shape of "what tabs exist and which is showing" lives in this one place.
//!
//! Two invariants mirror the layout tree one level up:
//!
//! - **A session is never empty.** It is born with one tab and always keeps at
//!   least one; closing the last tab is [refused](SessionError::LastTab) rather
//!   than being a way to reach a session with no active tab to render.
//! - **The active tab always exists.** Every operation that removes a tab leaves
//!   `active` pointing at a tab still present, so [`Session::active_tab`] can
//!   return a reference rather than an `Option`.
//!
//! Creating a tab makes it active — the tmux `new-window` reflex, and what a user
//! who just asked for a fresh tab expects to be looking at. Closing the active
//! tab moves activation to the tab that took its place in the bar: the right
//! neighbour, or the new rightmost tab when the closed one was last. Closing any
//! other tab leaves the active one untouched.
//!
//! ```
//! use cloo_core::session::Session;
//! use cloo_core::tab::TabName;
//! use cloo_proto::{PaneId, SessionId};
//!
//! let mut session = Session::new(
//!     SessionId::new(0),
//!     TabName::new("build").expect("valid name"),
//!     PaneId::new(0),
//! );
//! let second = session.create_tab(
//!     TabName::new("test").expect("valid name"),
//!     PaneId::new(1),
//! );
//! assert_eq!(session.active(), second, "a new tab becomes active");
//! ```

use cloo_proto::{PaneId, SessionId, TabId};

use crate::error::SessionError;
use crate::id::TabIdAllocator;
use crate::tab::{Tab, TabName};

/// An ordered set of tabs with exactly one active, plus its ID allocator.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    id: SessionId,
    /// Tabs in tab-bar order. Never empty.
    tabs: Vec<Tab>,
    /// The active tab. Always the ID of a tab in `tabs`.
    active: TabId,
    tab_ids: TabIdAllocator,
}

impl Session {
    /// A session holding a single tab, active, with one full-area pane.
    ///
    /// The first tab's ID is allocated here, so it is never
    /// [`TabId::new(0)`](TabId::new) by assumption but by the allocator handing
    /// out zero first — the same allocator every later tab draws from.
    #[must_use]
    pub fn new(id: SessionId, first_tab: TabName, first_pane: PaneId) -> Self {
        let mut tab_ids = TabIdAllocator::new();
        let tab_id = tab_ids.allocate();
        Self {
            id,
            tabs: vec![Tab::new(tab_id, first_tab, first_pane)],
            active: tab_id,
            tab_ids,
        }
    }

    /// The session's stable identity.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// The active tab's ID. Always names a tab this session holds.
    #[must_use]
    pub const fn active(&self) -> TabId {
        self.active
    }

    /// How many tabs the session holds. Always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Always `false` — a session cannot be empty. Present because clippy asks
    /// for it alongside [`Session::len`], and answering honestly beats hiding it.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The tabs, in tab-bar order.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// The tab with this ID, or `None`.
    #[must_use]
    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id() == id)
    }

    /// The tab with this ID, mutably, or `None`.
    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id() == id)
    }

    /// The active tab. Always present.
    #[must_use]
    pub fn active_tab(&self) -> &Tab {
        self.tab(self.active)
            .expect("the active tab is always present")
    }

    /// The active tab, mutably. Always present.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        let active = self.active;
        self.tab_mut(active)
            .expect("the active tab is always present")
    }

    /// Appends a new tab holding one full-area pane and makes it active.
    ///
    /// Returns the freshly allocated [`TabId`]. Activation follows the new tab
    /// because a user who just created one means to be looking at it; call
    /// [`Session::select_tab`] to go back.
    pub fn create_tab(&mut self, name: TabName, first_pane: PaneId) -> TabId {
        let id = self.tab_ids.allocate();
        self.tabs.push(Tab::new(id, name, first_pane));
        self.active = id;
        id
    }

    /// Renames the tab with this ID.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTab`] if no tab has this ID. The session is
    /// unchanged.
    pub fn rename_tab(&mut self, id: TabId, name: TabName) -> Result<(), SessionError> {
        let tab = self.tab_mut(id).ok_or(SessionError::UnknownTab(id))?;
        tab.set_name(name);
        Ok(())
    }

    /// Makes the tab with this ID active.
    ///
    /// Idempotent when `id` is already active.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTab`] if no tab has this ID. The active tab is
    /// unchanged.
    pub fn select_tab(&mut self, id: TabId) -> Result<(), SessionError> {
        if self.tab(id).is_none() {
            return Err(SessionError::UnknownTab(id));
        }
        self.active = id;
        Ok(())
    }

    /// Removes the tab with this ID.
    ///
    /// When the removed tab was active, activation moves to the tab that slid
    /// into its place — the former right neighbour, or the new rightmost tab when
    /// the closed one was last. Closing any other tab leaves the active one where
    /// it was.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownTab`] if no tab has this ID.
    /// - [`SessionError::LastTab`] if it is the only tab. A session keeps at
    ///   least one tab; the last tab is closed by tearing the session down, not
    ///   by reaching an empty session.
    ///
    /// The session is unchanged in either error case, unknown checked first so a
    /// bad ID never masquerades as the last-tab rule.
    pub fn close_tab(&mut self, id: TabId) -> Result<(), SessionError> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id() == id)
            .ok_or(SessionError::UnknownTab(id))?;
        if self.tabs.len() == 1 {
            return Err(SessionError::LastTab(id));
        }

        self.tabs.remove(index);
        if self.active == id {
            // The tab now at `index` is the former right neighbour; if the
            // closed tab was last, clamp to the new rightmost.
            let next = index.min(self.tabs.len() - 1);
            self.active = self.tabs[next].id();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> TabName {
        TabName::new(text).expect("valid name")
    }

    fn session() -> Session {
        Session::new(SessionId::new(0), name("one"), PaneId::new(0))
    }

    /// Grows a session to `names.len() + 1` tabs and returns their IDs in bar
    /// order, the first being the tab `new` created.
    fn with_tabs(session: &mut Session, names: &[&str]) -> Vec<TabId> {
        let mut ids = vec![session.tabs()[0].id()];
        for (offset, text) in names.iter().enumerate() {
            let pane = PaneId::new((offset + 1) as u64);
            ids.push(session.create_tab(name(text), pane));
        }
        ids
    }

    #[test]
    fn a_new_session_is_one_active_tab() {
        let session = session();
        assert_eq!(session.len(), 1);
        assert!(!session.is_empty());
        assert_eq!(session.id(), SessionId::new(0));
        assert_eq!(session.active(), session.tabs()[0].id());
        assert_eq!(session.active_tab().name().as_str(), "one");
    }

    #[test]
    fn creating_a_tab_appends_it_and_activates_it() {
        let mut session = session();
        let first = session.active();
        let second = session.create_tab(name("two"), PaneId::new(1));

        assert_eq!(session.len(), 2);
        assert_ne!(second, first, "IDs are never reused");
        assert_eq!(session.active(), second, "a new tab becomes active");
        assert_eq!(
            session.tabs().iter().map(Tab::id).collect::<Vec<_>>(),
            vec![first, second],
            "the new tab lands at the end of the bar"
        );
        assert_eq!(session.active_tab().focused(), PaneId::new(1));
    }

    #[test]
    fn renaming_a_tab_changes_only_that_tab() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two"]);

        session
            .rename_tab(ids[0], name("renamed"))
            .expect("tab exists");
        assert_eq!(session.tab(ids[0]).expect("tab").name().as_str(), "renamed");
        assert_eq!(
            session.tab(ids[1]).expect("tab").name().as_str(),
            "two",
            "the other tab is untouched"
        );
    }

    #[test]
    fn renaming_an_unknown_tab_is_refused() {
        let mut session = session();
        assert_eq!(
            session
                .rename_tab(TabId::new(9), name("nope"))
                .expect_err("tab 9 does not exist"),
            SessionError::UnknownTab(TabId::new(9))
        );
    }

    #[test]
    fn selecting_moves_activation_without_reordering() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two", "three"]);
        assert_eq!(session.active(), ids[2]);

        session.select_tab(ids[0]).expect("tab exists");
        assert_eq!(session.active(), ids[0]);
        assert_eq!(
            session.tabs().iter().map(Tab::id).collect::<Vec<_>>(),
            ids,
            "selecting never reorders the bar"
        );

        // Idempotent.
        session.select_tab(ids[0]).expect("tab exists");
        assert_eq!(session.active(), ids[0]);
    }

    #[test]
    fn selecting_an_unknown_tab_is_refused_and_keeps_the_active_one() {
        let mut session = session();
        let before = session.active();
        assert_eq!(
            session
                .select_tab(TabId::new(9))
                .expect_err("tab 9 does not exist"),
            SessionError::UnknownTab(TabId::new(9))
        );
        assert_eq!(session.active(), before);
    }

    #[test]
    fn closing_a_non_active_tab_leaves_activation_alone() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two", "three"]);
        session.select_tab(ids[2]).expect("tab exists");

        session.close_tab(ids[0]).expect("tab exists");
        assert_eq!(session.len(), 2);
        assert_eq!(session.active(), ids[2], "activation is unaffected");
        assert!(session.tab(ids[0]).is_none());
    }

    #[test]
    fn closing_the_active_tab_activates_its_right_neighbour() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two", "three"]);
        session.select_tab(ids[1]).expect("tab exists");

        session.close_tab(ids[1]).expect("tab exists");
        assert_eq!(
            session.active(),
            ids[2],
            "the tab that slid left into the gap becomes active"
        );
    }

    #[test]
    fn closing_the_active_rightmost_tab_falls_back_to_the_left() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two", "three"]);
        assert_eq!(session.active(), ids[2], "the last-created tab is active");

        session.close_tab(ids[2]).expect("tab exists");
        assert_eq!(
            session.active(),
            ids[1],
            "with no right neighbour, the new rightmost tab becomes active"
        );
    }

    #[test]
    fn closing_the_last_tab_is_refused() {
        let mut session = session();
        let only = session.active();
        assert_eq!(
            session
                .close_tab(only)
                .expect_err("the last tab must survive"),
            SessionError::LastTab(only)
        );
        assert_eq!(session.len(), 1);
        assert_eq!(session.active(), only);
    }

    #[test]
    fn closing_an_unknown_tab_is_refused_even_when_one_remains() {
        // The unknown check comes first: a bad ID must not be reported as the
        // last-tab rule just because there is a single tab.
        let mut session = session();
        assert_eq!(
            session
                .close_tab(TabId::new(9))
                .expect_err("tab 9 does not exist"),
            SessionError::UnknownTab(TabId::new(9))
        );
        assert_eq!(session.len(), 1);
    }

    #[test]
    fn tabs_can_be_closed_down_to_the_last_survivor() {
        let mut session = session();
        let ids = with_tabs(&mut session, &["two", "three", "four"]);

        session.close_tab(ids[1]).expect("tab exists");
        session.close_tab(ids[0]).expect("tab exists");
        session.close_tab(ids[3]).expect("tab exists");
        assert_eq!(session.len(), 1);
        assert_eq!(session.active(), ids[2], "the survivor is the active tab");
        assert_eq!(
            session.close_tab(ids[2]).expect_err("last tab"),
            SessionError::LastTab(ids[2])
        );
    }
}
