//! Monotonic allocators for the newtype IDs defined in `cloo-proto`.
//!
//! IDs are never reused within a session. A reused `PaneId` would let a stale
//! client message land on a pane the sender never meant, and the wire carries no
//! generation counter to catch it.

use cloo_proto::{PaneId, SessionId, TabId};

/// Declares a monotonic allocator for one wire ID newtype.
macro_rules! id_allocator {
    ($(#[$meta:meta])* $name:ident, $id:ty) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            next: u64,
        }

        impl $name {
            /// A fresh allocator, handing out IDs from zero.
            #[must_use]
            pub const fn new() -> Self {
                Self { next: 0 }
            }

            /// Resumes allocation after `last`, for a session restored from disk.
            #[must_use]
            pub const fn resuming_after(last: $id) -> Self {
                Self {
                    next: last.get().saturating_add(1),
                }
            }

            /// Hands out the next unused ID.
            ///
            /// Saturates rather than wrapping at `u64::MAX`, so exhaustion is a
            /// stuck allocator rather than a silent wrap back to live IDs.
            /// Allocating that many IDs is not reachable in a real session.
            pub fn allocate(&mut self) -> $id {
                let id = <$id>::new(self.next);
                self.next = self.next.saturating_add(1);
                id
            }

            /// The ID [`Self::allocate`] will hand out next, without consuming it.
            #[must_use]
            pub const fn peek(&self) -> $id {
                <$id>::new(self.next)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_allocator!(
    /// Allocates [`PaneId`]s within a session.
    PaneIdAllocator,
    PaneId
);
id_allocator!(
    /// Allocates [`TabId`]s within a session.
    TabIdAllocator,
    TabId
);
id_allocator!(
    /// Allocates [`SessionId`]s within a daemon.
    SessionIdAllocator,
    SessionId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_monotonic_from_zero() {
        let mut panes = PaneIdAllocator::new();
        assert_eq!(panes.allocate(), PaneId::new(0));
        assert_eq!(panes.allocate(), PaneId::new(1));
        assert_eq!(panes.allocate(), PaneId::new(2));
    }

    #[test]
    fn peek_does_not_consume() {
        let mut tabs = TabIdAllocator::new();
        assert_eq!(tabs.peek(), TabId::new(0));
        assert_eq!(tabs.peek(), TabId::new(0));
        assert_eq!(tabs.allocate(), TabId::new(0));
        assert_eq!(tabs.peek(), TabId::new(1));
    }

    #[test]
    fn resuming_continues_past_the_last_used_id() {
        let mut panes = PaneIdAllocator::resuming_after(PaneId::new(41));
        assert_eq!(panes.allocate(), PaneId::new(42));
    }

    #[test]
    fn allocation_saturates_instead_of_wrapping() {
        let mut sessions = SessionIdAllocator::resuming_after(SessionId::new(u64::MAX));
        assert_eq!(sessions.allocate(), SessionId::new(u64::MAX));
        assert_eq!(sessions.allocate(), SessionId::new(u64::MAX));
    }
}
