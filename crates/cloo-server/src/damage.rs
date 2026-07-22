//! Coalesced, row-granular snapshots for attached clients.
//!
//! A PTY read says only that the emulator *might* have changed.  Capturing and
//! comparing at the frame boundary turns a burst of those reads into one
//! [`DamageFrame`], containing just the rows that differ from the last frame.
//! The daemon publishes that frame through a bounded `broadcast` channel; it
//! never waits for an individual socket while the session task is running.

use cloo_proto::{
    CursorShape, LayoutSnapshot, OuterTerminalEffect, PaneId, Point, ServerMessage, TabId,
};

use crate::session::SessionSnapshot;

/// One atomic batch of server messages describing a new session picture.
///
/// The messages must stay together and in order: geometry precedes rows, and
/// modes precede the next mouse decision.  A client that misses a batch drops
/// its receiver backlog and obtains a full snapshot instead of trying to
/// stitch together partial frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamageFrame {
    messages: Vec<ServerMessage>,
}

impl DamageFrame {
    /// The wire messages in the order a client must receive them.
    #[must_use]
    pub fn messages(&self) -> &[ServerMessage] {
        &self.messages
    }

    /// Whether this frame ends the session.
    #[must_use]
    pub fn ends_session(&self) -> bool {
        self.messages
            .iter()
            .any(|message| matches!(message, ServerMessage::Exit(_)))
    }

    /// Tells clients that no more session updates will arrive.
    #[must_use]
    pub fn exit(code: i32) -> Self {
        Self {
            messages: vec![ServerMessage::Exit(code)],
        }
    }

    /// Carries one typed outer-terminal request without claiming it is grid
    /// damage. A client may suppress it under local policy, but it must never
    /// be folded into a snapshot or authoritative session state.
    #[must_use]
    pub fn effect(pane: PaneId, effect: OuterTerminalEffect) -> Self {
        Self {
            messages: vec![ServerMessage::Effect { pane, effect }],
        }
    }
}

/// Remembers the last frame published to clients.
///
/// This cache is transport state, not session state.  The session task remains
/// authoritative: when a client lags, it asks that task for a fresh snapshot
/// rather than trusting this comparison cache to reconstruct history.
#[derive(Debug, Default)]
pub struct DamageTracker {
    previous: Option<SessionSnapshot>,
}

impl DamageTracker {
    /// Compares `current` with the last published snapshot.
    ///
    /// Returns `None` when the notification did not produce a visible change.
    /// A new focused pane or pane geometry always produces every row, because
    /// a row comparison across two different grids is not meaningful.
    #[must_use]
    pub fn update(&mut self, tab: TabId, current: &SessionSnapshot) -> Option<DamageFrame> {
        let previous = self.previous.as_ref();
        let layout_changed = previous.is_none_or(|before| {
            before.area != current.area
                || before.panes != current.panes
                || before.focused != current.focused
                || before.zoomed != current.zoomed
        });
        let replace_all_rows = previous.is_none_or(|before| {
            before.focused != current.focused || before.pane.size != current.pane.size
        });

        let rows = if replace_all_rows {
            current.pane.rows.clone()
        } else {
            // `replace_all_rows` already established that the grids agree. A
            // different row count is still defensive full damage: a partial
            // comparison could leave a stale trailing row in a client cache.
            if let Some(before) = previous {
                if before.pane.rows.len() != current.pane.rows.len() {
                    current.pane.rows.clone()
                } else {
                    current
                        .pane
                        .rows
                        .iter()
                        .zip(&before.pane.rows)
                        .filter(|(row, old)| row != old)
                        .map(|(row, _)| row.clone())
                        .collect()
                }
            } else {
                // Kept defensive even though `replace_all_rows` above is true
                // when there is no prior snapshot.
                current.pane.rows.clone()
            }
        };

        // Identity moves on a different clock from geometry: a resize is not a
        // rename, so a full-screen drag must not resend every pane's name.
        let metas_changed = previous.is_none_or(|before| before.metas != current.metas);
        // Attention moves on yet another clock: a state change is not a rename
        // and a rename is not a state change, so each is resent only for itself.
        let attention_changed = previous.is_none_or(|before| before.attention != current.attention);
        let modes_changed = previous.is_none_or(|before| before.modes != current.modes);
        let cursor_changed =
            previous.is_none_or(|before| before.pane.cursor != current.pane.cursor);

        let mut messages = Vec::new();
        if layout_changed {
            messages.push(ServerMessage::Layout(LayoutSnapshot {
                tab,
                panes: current.panes.clone(),
                focused: Some(current.focused),
                zoomed: current.zoomed,
            }));
        }
        if metas_changed {
            messages.push(ServerMessage::Panes(current.metas.clone()));
        }
        if attention_changed {
            messages.push(ServerMessage::Attention(current.attention.clone()));
        }
        if !rows.is_empty() {
            messages.push(ServerMessage::Damage {
                pane: current.focused,
                rows,
            });
        }
        if modes_changed {
            messages.push(ServerMessage::Modes {
                pane: current.focused,
                modes: current.modes,
            });
        }
        if cursor_changed {
            messages.push(cursor_message(current.focused, current.pane.cursor));
        }

        self.previous = Some(current.clone());
        (!messages.is_empty()).then_some(DamageFrame { messages })
    }
}

/// Converts the optional emulator cursor into the explicit wire state.
fn cursor_message(pane: PaneId, cursor: Option<(Point, CursorShape)>) -> ServerMessage {
    match cursor {
        Some((pos, shape)) => ServerMessage::CursorMoved {
            pane,
            pos,
            shape,
            visible: true,
        },
        None => ServerMessage::CursorMoved {
            pane,
            pos: Point::new(0, 0),
            shape: CursorShape::default(),
            visible: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::PaneSnapshot;
    use cloo_proto::{Cell, PaneRect, RowUpdate, Size};

    fn snapshot(rows: &[&str]) -> SessionSnapshot {
        named_snapshot(rows, "shell")
    }

    /// The same picture with the pane under a given name, so a rename can be
    /// told apart from grid damage.
    fn named_snapshot(rows: &[&str], name: &str) -> SessionSnapshot {
        let pane = PaneId::new(1);
        let cols = u16::try_from(rows.first().map_or(0, |row| row.len())).unwrap_or(0);
        let size = Size::new(cols, u16::try_from(rows.len()).unwrap_or(0));
        SessionSnapshot {
            area: size,
            panes: vec![PaneRect {
                pane,
                x: 0,
                y: 0,
                size,
            }],
            metas: vec![cloo_proto::PaneInfo {
                pane,
                profile: "generic".into(),
                name: name.to_owned(),
                task: None,
                cwd: "/home/dev".into(),
            }],
            attention: vec![cloo_proto::PaneAttention {
                pane,
                state: cloo_proto::AttentionState::Unknown,
                source: cloo_proto::AttentionSource::None,
                acknowledged: false,
            }],
            focused: pane,
            zoomed: None,
            pane: PaneSnapshot {
                size,
                rows: rows
                    .iter()
                    .enumerate()
                    .map(|(index, text)| RowUpdate {
                        row: u16::try_from(index).unwrap_or(0),
                        cells: text
                            .chars()
                            .map(|ch| Cell {
                                ch,
                                ..Cell::default()
                            })
                            .collect(),
                    })
                    .collect(),
                cursor: None,
            },
            modes: cloo_proto::PaneModes::default(),
        }
    }

    #[test]
    fn the_first_picture_contains_every_row_and_its_metadata() {
        let mut tracker = DamageTracker::default();
        let frame = tracker
            .update(TabId::new(1), &snapshot(&["ab", "cd"]))
            .expect("a first snapshot is a full resync");
        assert!(matches!(
            frame.messages().first(),
            Some(ServerMessage::Layout(_))
        ));
        assert!(matches!(
            frame.messages().get(1),
            Some(ServerMessage::Panes(panes)) if panes.len() == 1
        ));
        assert!(matches!(
            frame.messages().get(2),
            Some(ServerMessage::Attention(attention)) if attention.len() == 1
        ));
        assert!(
            matches!(frame.messages().get(3), Some(ServerMessage::Damage { rows, .. }) if rows.len() == 2)
        );
        assert!(matches!(
            frame.messages().get(4),
            Some(ServerMessage::Modes { .. })
        ));
        assert!(matches!(
            frame.messages().get(5),
            Some(ServerMessage::CursorMoved { visible: false, .. })
        ));
    }

    #[test]
    fn identity_is_resent_only_when_it_changes() {
        // Geometry and identity move on different clocks: a row that changed
        // must not drag every pane's name across the wire with it.
        let mut tracker = DamageTracker::default();
        let _ = tracker.update(TabId::new(1), &named_snapshot(&["ab"], "shell"));
        let frame = tracker
            .update(TabId::new(1), &named_snapshot(&["XX"], "shell"))
            .expect("a changed row produces damage");
        assert!(
            !frame
                .messages()
                .iter()
                .any(|message| matches!(message, ServerMessage::Panes(_))),
            "an unchanged name costs no wire frame"
        );

        let renamed = tracker
            .update(TabId::new(1), &named_snapshot(&["XX"], "api"))
            .expect("a rename is a visible change");
        assert_eq!(
            renamed.messages(),
            &[ServerMessage::Panes(named_snapshot(&["XX"], "api").metas)]
        );
    }

    #[test]
    fn attention_is_resent_only_when_it_changes() {
        // Attention is on its own clock too: a changed row must not resend a
        // pane's state, and a changed state must not resend its rows.
        let with_state = |rows: &[&str], state: cloo_proto::AttentionState| {
            let mut snap = snapshot(rows);
            snap.attention[0].state = state;
            snap.attention[0].source = cloo_proto::AttentionSource::Bell;
            snap
        };

        let mut tracker = DamageTracker::default();
        let _ = tracker.update(
            TabId::new(1),
            &with_state(&["ab"], cloo_proto::AttentionState::Unknown),
        );

        // A row changes, attention does not.
        let frame = tracker
            .update(
                TabId::new(1),
                &with_state(&["XX"], cloo_proto::AttentionState::Unknown),
            )
            .expect("a changed row produces damage");
        assert!(
            !frame
                .messages()
                .iter()
                .any(|message| matches!(message, ServerMessage::Attention(_))),
            "an unchanged state costs no wire frame"
        );

        // Attention changes, no row does.
        let frame = tracker
            .update(
                TabId::new(1),
                &with_state(&["XX"], cloo_proto::AttentionState::NeedsInput),
            )
            .expect("a state change is a visible change");
        assert_eq!(
            frame.messages(),
            &[ServerMessage::Attention(vec![cloo_proto::PaneAttention {
                pane: PaneId::new(1),
                state: cloo_proto::AttentionState::NeedsInput,
                source: cloo_proto::AttentionSource::Bell,
                acknowledged: false,
            }])],
            "only the attention message, never the unchanged rows"
        );
    }

    #[test]
    fn one_changed_row_does_not_resend_its_neighbours() {
        let mut tracker = DamageTracker::default();
        let _ = tracker.update(TabId::new(1), &snapshot(&["ab", "cd", "ef"]));
        let frame = tracker
            .update(TabId::new(1), &snapshot(&["ab", "XX", "ef"]))
            .expect("a changed row produces damage");
        assert_eq!(
            frame.messages(),
            &[ServerMessage::Damage {
                pane: PaneId::new(1),
                rows: vec![RowUpdate {
                    row: 1,
                    cells: "XX"
                        .chars()
                        .map(|ch| Cell {
                            ch,
                            ..Cell::default()
                        })
                        .collect(),
                }],
            }]
        );
    }

    #[test]
    fn an_unchanged_snapshot_costs_no_wire_frame() {
        let mut tracker = DamageTracker::default();
        let picture = snapshot(&["ab"]);
        let _ = tracker.update(TabId::new(1), &picture);
        assert_eq!(tracker.update(TabId::new(1), &picture), None);
    }

    #[test]
    fn an_exit_frame_is_detectable_by_a_client_task() {
        assert!(DamageFrame::exit(0).ends_session());
    }

    #[test]
    fn an_effect_frame_keeps_the_request_out_of_grid_damage() {
        let frame = DamageFrame::effect(
            PaneId::new(1),
            cloo_proto::OuterTerminalEffect::SetTitle("agent task".into()),
        );
        assert_eq!(
            frame.messages(),
            &[ServerMessage::Effect {
                pane: PaneId::new(1),
                effect: cloo_proto::OuterTerminalEffect::SetTitle("agent task".into()),
            }]
        );
    }
}
