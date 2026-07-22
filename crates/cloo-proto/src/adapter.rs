//! The local adapter control protocol.
//!
//! An adapter is an opt-in local helper — a wrapper script, a hook, a small
//! daemon of the user's own — that knows something about a pane cloo cannot
//! observe for itself, and says so. It is deliberately *not* a client: it has
//! its own message enums, spoken on its own socket, and the vocabulary here is
//! the whole of what an adapter may say.
//!
//! Two narrowings are types rather than rules, because a rule is only as good
//! as the branch that remembers to check it:
//!
//! - An adapter cannot send keystrokes, resize a session, or read a grid.
//!   [`AdapterMessage`] has no variant for any of it, so no server-side refusal
//!   has to exist for the case.
//! - An adapter cannot claim [`AttentionState::Quiet`](crate::AttentionState::Quiet)
//!   or erase a state back to
//!   [`Unknown`](crate::AttentionState::Unknown). [`AdapterState`] has exactly
//!   the four states `docs/AGENT_WORKFLOWS.md` permits an advisory source, so
//!   "nothing to do" and "nothing has reported" stay claims only cloo's own
//!   observations can make.
//!
//! Everything an adapter reports stays attributed to it: the server stamps the
//! provenance from the name announced in [`AdapterMessage::Hello`], so a client
//! renders an advisory claim as an advisory claim and never as fact.

use serde::{Deserialize, Serialize};

use crate::ids::{PaneId, SessionId};
use crate::message::AttentionState;

/// The attention states an opt-in adapter is permitted to report.
///
/// The four of `docs/AGENT_WORKFLOWS.md`, and no others. `quiet` is a claim
/// that there is nothing to do and `unknown` is the absence of a claim; letting
/// an advisory source assert either would let it *clear* something cloo
/// observed directly, such as a child that exited non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterState {
    /// The adapter says the pane is making progress.
    Working,
    /// The adapter says the pane needs a decision or a response.
    NeedsInput,
    /// The adapter says the pane finished with a result nobody has looked at.
    Ready,
    /// The adapter says the pane's work failed.
    Failed,
}

impl From<AdapterState> for AttentionState {
    fn from(state: AdapterState) -> Self {
        match state {
            AdapterState::Working => Self::Working,
            AdapterState::NeedsInput => Self::NeedsInput,
            AdapterState::Ready => Self::Ready,
            AdapterState::Failed => Self::Failed,
        }
    }
}

/// Adapter → daemon, on the control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterMessage {
    /// The first frame on any control connection.
    ///
    /// The name is what the server stamps as provenance and what it checks a
    /// pane's profile against, so an adapter that never says who it is can
    /// never report anything.
    Hello {
        /// The protocol version this adapter speaks.
        protocol_version: u16,
        /// The adapter's ID, as a profile would name it.
        adapter: String,
    },
    /// One advisory report about one pane.
    Report {
        /// The pane the adapter is speaking about.
        pane: PaneId,
        /// What it claims.
        state: AdapterState,
    },
}

/// Daemon → adapter, on the control socket.
///
/// Every report is answered. An adapter is usually a script, and a silent drop
/// is indistinguishable from success to one — a refusal it can print is the
/// difference between a misconfigured profile being found and being lived with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterReply {
    /// The control connection is open.
    Ready {
        /// The protocol version the server speaks.
        protocol_version: u16,
        /// The session the adapter is now talking to.
        session: SessionId,
    },
    /// The connection was refused, with a rendered reason. Terminal: the server
    /// closes the connection afterwards.
    Refused {
        /// A human-readable explanation.
        reason: String,
    },
    /// The report was applied to the named pane.
    Applied {
        /// The pane whose attention now carries this adapter's claim.
        pane: PaneId,
    },
    /// The report was not applied, and why. Not terminal: the connection stays
    /// open, because the next pane may well be one this adapter owns.
    Rejected {
        /// The pane the report named.
        pane: PaneId,
        /// Why nothing changed.
        reason: AdapterRejection,
    },
}

/// Why one adapter report changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterRejection {
    /// No pane with that ID — it closed, or it never existed.
    UnknownPane,
    /// The pane's profile did not name this adapter.
    ///
    /// This is what "opt-in" means: naming an adapter in a profile is the
    /// user's consent for it to speak about panes launched from that profile,
    /// and a pane that named no adapter is reachable by none.
    NotPermitted,
    /// The session ended while the report was in flight.
    SessionEnded,
}

impl core::fmt::Display for AdapterRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownPane => f.write_str("no such pane in this session"),
            Self::NotPermitted => f.write_str("the pane's profile did not opt into this adapter"),
            Self::SessionEnded => f.write_str("the session ended"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PROTOCOL_VERSION, decode, encode};

    #[test]
    fn every_permitted_state_maps_to_its_own_attention_state() {
        let permitted = [
            (AdapterState::Working, AttentionState::Working),
            (AdapterState::NeedsInput, AttentionState::NeedsInput),
            (AdapterState::Ready, AttentionState::Ready),
            (AdapterState::Failed, AttentionState::Failed),
        ];
        for (adapter, expected) in permitted {
            assert_eq!(AttentionState::from(adapter), expected);
        }
    }

    #[test]
    fn no_adapter_state_can_claim_quiet_or_erase_one_to_unknown() {
        // The exhaustive list is the point: an adapter is advisory, so it may
        // not assert "there is nothing to do", and it may not withdraw a state
        // cloo observed for itself back to "nothing has reported".
        for adapter in [
            AdapterState::Working,
            AdapterState::NeedsInput,
            AdapterState::Ready,
            AdapterState::Failed,
        ] {
            let state = AttentionState::from(adapter);
            assert_ne!(state, AttentionState::Quiet);
            assert_ne!(state, AttentionState::Unknown);
        }
    }

    #[test]
    fn adapter_messages_round_trip() {
        let messages = [
            AdapterMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                adapter: "claude-adapter".to_owned(),
            },
            AdapterMessage::Report {
                pane: PaneId::new(7),
                state: AdapterState::NeedsInput,
            },
        ];
        for message in messages {
            let frame = encode(&message).expect("an adapter message encodes");
            let (decoded, used) =
                decode::<AdapterMessage>(&frame).expect("an adapter message decodes");
            assert_eq!(decoded, message);
            assert_eq!(used, frame.len());
        }
    }

    #[test]
    fn adapter_replies_round_trip() {
        let replies = [
            AdapterReply::Ready {
                protocol_version: PROTOCOL_VERSION,
                session: SessionId::new(1),
            },
            AdapterReply::Refused {
                reason: "version mismatch".to_owned(),
            },
            AdapterReply::Applied {
                pane: PaneId::new(3),
            },
            AdapterReply::Rejected {
                pane: PaneId::new(3),
                reason: AdapterRejection::NotPermitted,
            },
        ];
        for reply in replies {
            let frame = encode(&reply).expect("an adapter reply encodes");
            let (decoded, used) = decode::<AdapterReply>(&frame).expect("an adapter reply decodes");
            assert_eq!(decoded, reply);
            assert_eq!(used, frame.len());
        }
    }

    #[test]
    fn every_rejection_explains_itself() {
        for reason in [
            AdapterRejection::UnknownPane,
            AdapterRejection::NotPermitted,
            AdapterRejection::SessionEnded,
        ] {
            assert!(!reason.to_string().is_empty(), "{reason:?} must explain");
        }
    }
}
