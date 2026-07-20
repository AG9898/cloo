//! Newtype identifiers.
//!
//! These cross the wire and are trivially confused with each other as bare
//! integers, which is why they are not bare integers. See `docs/CONVENTIONS.md`.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Declares an opaque `u64` newtype ID with the same small surface each time.
macro_rules! wire_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Wraps a raw value. Only the allocator of these IDs should call this.
            #[must_use]
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            /// Unwraps to the raw value, for indexing and display.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

wire_id!(
    /// Identifies a session — one daemon-owned collection of tabs.
    SessionId,
    "session:"
);
wire_id!(
    /// Identifies a tab within a session.
    TabId,
    "tab:"
);
wire_id!(
    /// Identifies a pane — the leaf of a layout tree, backed by one PTY.
    PaneId,
    "pane:"
);
wire_id!(
    /// Identifies one attached client connection.
    ClientId,
    "client:"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_their_raw_value() {
        assert_eq!(PaneId::new(7).get(), 7);
        assert_eq!(TabId::new(0).get(), 0);
        assert_eq!(SessionId::new(u64::MAX).get(), u64::MAX);
        assert_eq!(ClientId::new(3).get(), 3);
    }

    #[test]
    fn ids_display_with_a_distinguishing_prefix() {
        assert_eq!(SessionId::new(1).to_string(), "session:1");
        assert_eq!(TabId::new(2).to_string(), "tab:2");
        assert_eq!(PaneId::new(3).to_string(), "pane:3");
        assert_eq!(ClientId::new(4).to_string(), "client:4");
    }

    #[test]
    fn ids_serialize_transparently() {
        let bytes = postcard::to_stdvec(&PaneId::new(9)).expect("PaneId encodes");
        let raw = postcard::to_stdvec(&9u64).expect("u64 encodes");
        assert_eq!(bytes, raw);
    }
}
