//! Short, interruptible transitions for chrome.
//!
//! The style guide gives cloo one motion vocabulary: focus, split, close, and
//! overlay transitions target 120ms, stay inside the render frame budget, are
//! interruptible, and obey a reduce-motion setting (DECISIONS.md RESOLVED-09).
//! This module is that vocabulary as a model plus the cell arithmetic it paints
//! with; [`crate::renderer::Renderer::render_transition`] is the only place it
//! becomes bytes.
//!
//! Three properties are worth reading twice, because each is a rule the rest of
//! the client would otherwise have to remember:
//!
//! - **Time is passed in, never read.** [`Motion::start`] and [`Motion::tick`]
//!   take an [`Instant`], so a whole transition is testable frame by frame
//!   without sleeping and without a clock the tests have to fake.
//! - **A transition advances only on a tick, and only once per frame budget.**
//!   [`Motion::tick`] answers `None` when the step it would draw is the step it
//!   already drew, so a caller that sampled once per PTY read still costs at
//!   most [`MOTION_STEPS`] `+ 1` frames for a whole transition. Motion can
//!   therefore never become the per-read repaint the frame cap exists to
//!   prevent.
//! - **An interruption settles; it never reverts.** [`Motion::interrupt`] ends
//!   the transition at its *end* state, which is exactly the frame the client
//!   was about to draw for the input, resize, or state change that interrupted
//!   it. Input and a resize are never delayed by a frame of motion, and the
//!   screen is never left half-way through one.
//!
//! Motion is a contrast ramp, not an appearance: a transition starts recessed
//! toward the frame background and settles at the chrome's own colours, and a
//! settled [`Phase`] returns every cell *unchanged*. That is what makes an
//! interrupted transition byte-identical to a client that animates nothing at
//! all — including one running under reduce-motion.
//!
//! ```
//! use std::time::Instant;
//!
//! use cloo_client::motion::{Motion, MotionKind, MotionSettings};
//!
//! let now = Instant::now();
//! let mut motion = Motion::new(MotionSettings::default());
//! assert!(!motion.start(MotionKind::Focus, now).is_settled());
//!
//! // A keystroke arrives mid-transition: it settles rather than waiting.
//! let settled = motion.interrupt().expect("a transition was in flight");
//! assert!(settled.is_settled());
//! assert!(!motion.is_active());
//! ```

use std::time::{Duration, Instant};

use cloo_proto::{Cell, CellAttrs, Color};

use crate::renderer::Span;

/// How long a transition takes, from the style guide.
pub const MOTION_DURATION: Duration = Duration::from_millis(120);

/// The render frame budget a transition is quantized into (~60fps).
///
/// The same interval the client's render loop ticks on. Motion is described in
/// whole frames rather than in milliseconds so it cannot ask for a repaint the
/// frame cap would not allow.
pub const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// How many frames a whole transition is drawn in.
///
/// `120ms / 16ms` is seven and a half; a transition takes the whole frames and
/// settles on the eighth tick, so it is never *longer* than the style guide's
/// budget.
pub const MOTION_STEPS: u8 = 7;

/// How far a transition's first frame is pulled toward the frame background.
///
/// Deliberately a partial blend rather than a fade from nothing: a transition
/// that is interrupted at any step must leave readable text, which is the same
/// rule the dimming treatment follows. The ramp runs from here to zero.
const MOTION_BLEND: u16 = 60;

// ---------------------------------------------------------------------------
// Kinds and settings
// ---------------------------------------------------------------------------

/// The four layout changes the style guide gives motion to.
///
/// A closed vocabulary on purpose: motion is a way of making a *layout* change
/// legible, so there is no kind for a pane's output, an attention report, or
/// anything else that arrives on a data clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionKind {
    /// Focus moved to another pane.
    Focus,
    /// A pane was split.
    Split,
    /// A pane was closed.
    Close,
    /// An overlay opened or dismissed.
    Overlay,
}

impl MotionKind {
    /// Every kind, in stable documentation order.
    pub const ALL: [Self; 4] = [Self::Focus, Self::Split, Self::Close, Self::Overlay];
}

/// The client-local accessibility choice for motion.
///
/// Client-local like the theme and the effect policy: two terminals attached to
/// one session may legitimately disagree, and neither answer is session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MotionSettings {
    /// Whether transitions are drawn at all. With reduce-motion on, every
    /// transition settles on the frame it started and no extra frame is ever
    /// requested.
    pub reduce_motion: bool,
}

impl MotionSettings {
    /// Transitions are drawn: the default.
    #[must_use]
    pub const fn animated() -> Self {
        Self {
            reduce_motion: false,
        }
    }

    /// The reduce-motion accessibility setting.
    #[must_use]
    pub const fn reduced() -> Self {
        Self {
            reduce_motion: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

/// How far along a transition is, quantized to whole frames.
///
/// Carries its [`MotionKind`] so a caller drawing one frame knows what it is
/// drawing, and a step rather than a duration so two clients on the same tick
/// paint the same cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    kind: MotionKind,
    step: u8,
}

impl Phase {
    /// The end of a transition: the chrome's own colours, unchanged.
    #[must_use]
    pub const fn settled(kind: MotionKind) -> Self {
        Self {
            kind,
            step: MOTION_STEPS,
        }
    }

    /// The transition this phase belongs to.
    #[must_use]
    pub const fn kind(self) -> MotionKind {
        self.kind
    }

    /// Which whole frame of the transition this is, from zero.
    #[must_use]
    pub const fn step(self) -> u8 {
        self.step
    }

    /// Whether the transition has reached its end state.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        self.step >= MOTION_STEPS
    }

    /// How far along the transition is, as a percentage.
    #[must_use]
    pub const fn percent(self) -> u16 {
        if self.step >= MOTION_STEPS {
            return 100;
        }
        (self.step as u16) * 100 / (MOTION_STEPS as u16)
    }

    /// How far this phase pulls a cell toward the frame background, as a
    /// percentage. Zero once settled, which is what makes a settled frame
    /// identical to one drawn with no motion at all.
    #[must_use]
    const fn blend(self) -> u16 {
        MOTION_BLEND * (100 - self.percent()) / 100
    }
}

// ---------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------

/// One in-flight transition, or none.
///
/// At most one: a second [`start`](Motion::start) replaces whatever was running
/// rather than queueing behind it, because the newer layout change is the one
/// the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    settings: MotionSettings,
    active: Option<Active>,
}

/// The bookkeeping for a transition that is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Active {
    kind: MotionKind,
    started: Instant,
    /// The step the caller was last handed. A tick that would produce this step
    /// again produces nothing instead — that is the frame cap, expressed once.
    drawn: u8,
}

impl Motion {
    /// Builds a motion state with the given accessibility settings.
    #[must_use]
    pub const fn new(settings: MotionSettings) -> Self {
        Self {
            settings,
            active: None,
        }
    }

    /// The settings this state was built with.
    #[must_use]
    pub const fn settings(self) -> MotionSettings {
        self.settings
    }

    /// Whether a transition is in flight, and so whether the render loop has a
    /// reason to draw on the next tick.
    #[must_use]
    pub const fn is_active(self) -> bool {
        self.active.is_some()
    }

    /// The transition in flight, if any.
    #[must_use]
    pub const fn kind(self) -> Option<MotionKind> {
        match self.active {
            Some(active) => Some(active.kind),
            None => None,
        }
    }

    /// Begins a transition and returns the phase to draw now.
    ///
    /// Under reduce-motion the returned phase is already settled and nothing is
    /// left in flight, so a client with the setting on draws exactly one frame
    /// for a layout change — the same frame it would draw with no motion at all.
    pub fn start(&mut self, kind: MotionKind, now: Instant) -> Phase {
        if self.settings.reduce_motion {
            self.active = None;
            return Phase::settled(kind);
        }
        self.active = Some(Active {
            kind,
            started: now,
            drawn: 0,
        });
        Phase { kind, step: 0 }
    }

    /// Advances the transition on the render tick.
    ///
    /// Answers `None` when there is nothing in flight *or* when the step this
    /// tick lands on is the one already drawn, so sampling faster than the frame
    /// budget costs nothing. The last phase of a transition is the settled one,
    /// and it clears the state: a caller that stops ticking once
    /// [`is_active`](Self::is_active) is false has still drawn the end state.
    pub fn tick(&mut self, now: Instant) -> Option<Phase> {
        let active = self.active.as_mut()?;
        let step = step_at(active.started, now);
        if step == active.drawn {
            return None;
        }
        active.drawn = step;
        let kind = active.kind;
        if step >= MOTION_STEPS {
            self.active = None;
            return Some(Phase::settled(kind));
        }
        Some(Phase { kind, step })
    }

    /// Ends any transition at its end state.
    ///
    /// Called for input, a resize, and a state change — the three things that
    /// always win. The returned phase is settled, so the caller draws the frame
    /// it was going to draw anyway and no half-finished ramp is left on screen.
    /// `None` means there was nothing in flight and the caller has nothing extra
    /// to draw.
    pub fn interrupt(&mut self) -> Option<Phase> {
        let active = self.active.take()?;
        Some(Phase::settled(active.kind))
    }
}

impl Default for Motion {
    fn default() -> Self {
        Self::new(MotionSettings::default())
    }
}

/// Which whole frame of a transition `now` lands in.
///
/// Saturating, so a clock that went backwards reads as the start rather than
/// panicking, and clamped to [`MOTION_STEPS`], so a tick that arrives late —
/// after a burst, or after the process was stopped — settles instead of
/// overshooting into a step nothing knows how to draw.
fn step_at(started: Instant, now: Instant) -> u8 {
    let elapsed = now.saturating_duration_since(started).as_millis();
    let budget = FRAME_BUDGET.as_millis().max(1);
    let step = elapsed / budget;
    u8::try_from(step).unwrap_or(MOTION_STEPS).min(MOTION_STEPS)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Applies a phase to one cell.
///
/// The character is never touched: motion changes contrast and nothing else, so
/// a transition can be interrupted at any step without a word being missing. A
/// 24-bit colour is blended toward `frame` exactly; a palette index or the
/// terminal's own default cannot be, so it takes the terminal's `DIM` rendition
/// for the duration instead — the same fallback dimming makes, and for the same
/// reason: guessing at the user's palette produces a worse answer than the
/// terminal's own faint one.
#[must_use]
pub fn phase_cell(cell: Cell, phase: Phase, frame: Color) -> Cell {
    let blend = phase.blend();
    if blend == 0 {
        return cell;
    }
    let mut moved = cell;
    match toward(cell.fg, frame, blend) {
        Some(fg) => moved.fg = fg,
        None => moved.attrs = moved.attrs.union(CellAttrs::DIM),
    }
    if let Some(bg) = toward(cell.bg, frame, blend) {
        moved.bg = bg;
    }
    moved
}

/// Applies a phase to a run of cells.
#[must_use]
pub fn phase_cells(cells: &[Cell], phase: Phase, frame: Color) -> Vec<Cell> {
    cells
        .iter()
        .copied()
        .map(|cell| phase_cell(cell, phase, frame))
        .collect()
}

/// Applies a phase to a positioned chrome span, keeping its origin.
///
/// Motion moves no chrome: a transition ramps a span's contrast where it
/// already is, because a header that slid across the screen would have to be
/// hit-tested at a position the client did not draw it at.
#[must_use]
pub fn phase_span(span: &Span, phase: Phase, frame: Color) -> Span {
    Span::new(span.at, phase_cells(&span.cells, phase, frame))
}

/// Blends one colour `percent` of the way toward `frame`.
///
/// `None` for anything that is not 24-bit at both ends, which is the caller's
/// signal to use the attribute fallback.
fn toward(color: Color, frame: Color, percent: u16) -> Option<Color> {
    let Color::Rgb(r, g, b) = color else {
        return None;
    };
    let Color::Rgb(fr, fg, fb) = frame else {
        return None;
    };
    let mix = |value: u8, target: u8| -> u8 {
        let mixed = (u16::from(value) * (100 - percent) + u16::from(target) * percent) / 100;
        u8::try_from(mixed).unwrap_or(value)
    };
    Some(Color::Rgb(mix(r, fr), mix(g, fg), mix(b, fb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use cloo_proto::Point;

    use crate::theme::{Theme, ThemeToken};

    fn frame() -> Color {
        Theme::storm().color(ThemeToken::Frame)
    }

    fn accent_cell() -> Cell {
        Cell {
            ch: 'x',
            fg: Theme::storm().color(ThemeToken::Accent),
            bg: Theme::storm().color(ThemeToken::Surface),
            attrs: CellAttrs::BOLD,
        }
    }

    /// A transition's whole span of ticks, one per frame budget, starting at
    /// `now`.
    fn frames(now: Instant) -> impl Iterator<Item = Instant> {
        (0..=u32::from(MOTION_STEPS)).map(move |n| now + FRAME_BUDGET * n)
    }

    // -- Budget -----------------------------------------------------------

    #[test]
    fn a_transition_fits_inside_the_style_guides_hundred_and_twenty_milliseconds() {
        let drawn = FRAME_BUDGET * u32::from(MOTION_STEPS);
        assert!(drawn <= MOTION_DURATION, "{drawn:?} exceeds the budget");
        assert!(
            drawn + FRAME_BUDGET > MOTION_DURATION,
            "a whole extra frame fits; the step count is too low"
        );
    }

    #[test]
    fn the_frame_budget_is_the_render_loops() {
        // The client renders at roughly 60fps; motion must be quantized into
        // that same tick or it would ask for frames the cap refuses.
        assert_eq!(FRAME_BUDGET, Duration::from_millis(16));
    }

    // -- Model ------------------------------------------------------------

    #[test]
    fn a_transition_starts_unsettled_and_advances_one_step_per_frame() {
        let now = Instant::now();
        let mut motion = Motion::default();
        let start = motion.start(MotionKind::Split, now);
        assert_eq!(start.step(), 0);
        assert!(!start.is_settled());

        for (expected, at) in frames(now).enumerate().skip(1) {
            let phase = motion.tick(at).expect("a new frame each budget");
            assert_eq!(usize::from(phase.step()), expected);
            assert_eq!(phase.kind(), MotionKind::Split);
        }
        assert!(!motion.is_active(), "the last frame settles the transition");
    }

    #[test]
    fn the_last_frame_of_a_transition_is_the_settled_one() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.start(MotionKind::Overlay, now);
        let last = frames(now)
            .skip(1)
            .filter_map(|at| motion.tick(at))
            .last()
            .expect("the transition drew frames");
        assert!(last.is_settled());
        assert_eq!(last.percent(), 100);
    }

    #[test]
    fn sampling_faster_than_the_frame_budget_produces_no_extra_frames() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.start(MotionKind::Focus, now);
        // A thousand samples inside one budget — a burst of PTY reads, say.
        let within: Vec<_> = (0..1000)
            .filter_map(|n| motion.tick(now + Duration::from_micros(n * 15)))
            .collect();
        assert!(
            within.is_empty(),
            "a step already drawn must not be drawn again"
        );
    }

    #[test]
    fn a_whole_transition_costs_a_bounded_number_of_frames_however_often_it_is_sampled() {
        let now = Instant::now();
        let mut motion = Motion::default();
        let mut drawn = 1; // The frame `start` itself returns.
        motion.start(MotionKind::Close, now);
        // One sample every 200 microseconds for the whole transition: eighty
        // times the frame budget's rate, which is what a large `cat` looks like.
        for n in 0..1000 {
            if motion.tick(now + Duration::from_micros(n * 200)).is_some() {
                drawn += 1;
            }
        }
        assert!(
            drawn <= usize::from(MOTION_STEPS) + 1,
            "{drawn} frames for one transition"
        );
    }

    #[test]
    fn a_late_tick_settles_rather_than_overshooting() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.start(MotionKind::Focus, now);
        let phase = motion
            .tick(now + Duration::from_secs(30))
            .expect("a late tick still draws the end state");
        assert!(phase.is_settled());
        assert_eq!(phase.step(), MOTION_STEPS);
        assert!(!motion.is_active());
        assert_eq!(motion.tick(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn a_clock_that_went_backwards_reads_as_the_start() {
        let now = Instant::now() + Duration::from_secs(10);
        let mut motion = Motion::default();
        motion.start(MotionKind::Focus, now);
        assert_eq!(motion.tick(now - Duration::from_secs(5)), None);
        assert!(motion.is_active());
    }

    #[test]
    fn a_second_transition_replaces_the_one_in_flight() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.start(MotionKind::Focus, now);
        let _ = motion.tick(now + FRAME_BUDGET * 3);
        let restart = motion.start(MotionKind::Split, now + FRAME_BUDGET * 3);
        assert_eq!(restart.step(), 0, "the newer change starts from the top");
        assert_eq!(motion.kind(), Some(MotionKind::Split));
    }

    #[test]
    fn nothing_in_flight_ticks_to_nothing() {
        let mut motion = Motion::default();
        assert_eq!(motion.tick(Instant::now()), None);
        assert_eq!(motion.interrupt(), None);
        assert_eq!(motion.kind(), None);
    }

    // -- Interruption -----------------------------------------------------

    #[test]
    fn input_a_resize_and_a_state_change_interrupt_at_the_end_state() {
        let now = Instant::now();
        // One fixture per interrupting event, because each one is a different
        // branch of the render loop and all three must answer the same way.
        for kind in MotionKind::ALL {
            let mut motion = Motion::default();
            motion.start(kind, now);
            let _ = motion.tick(now + FRAME_BUDGET * 2);
            let settled = motion.interrupt().expect("a transition was in flight");
            assert!(settled.is_settled(), "an interruption never leaves a ramp");
            assert_eq!(settled.kind(), kind);
            assert!(!motion.is_active());
            assert_eq!(
                motion.tick(now + FRAME_BUDGET * 3),
                None,
                "an interrupted transition asks for no further frames"
            );
        }
    }

    #[test]
    fn an_interrupted_transition_paints_exactly_what_no_motion_would() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.start(MotionKind::Focus, now);
        let mid = motion.tick(now + FRAME_BUDGET).expect("a mid frame");
        assert_ne!(
            phase_cell(accent_cell(), mid, frame()),
            accent_cell(),
            "a mid-transition frame is visibly different"
        );
        let settled = motion.interrupt().expect("in flight");
        assert_eq!(
            phase_cell(accent_cell(), settled, frame()),
            accent_cell(),
            "the settled frame is the chrome's own cells, unchanged"
        );
    }

    // -- Reduce motion ----------------------------------------------------

    #[test]
    fn reduce_motion_settles_immediately_and_draws_nothing_extra() {
        let now = Instant::now();
        let mut motion = Motion::new(MotionSettings::reduced());
        for kind in MotionKind::ALL {
            let phase = motion.start(kind, now);
            assert!(phase.is_settled(), "{kind:?} must not animate");
            assert!(!motion.is_active());
            assert_eq!(motion.tick(now + FRAME_BUDGET), None);
            assert_eq!(
                phase_cell(accent_cell(), phase, frame()),
                accent_cell(),
                "reduce-motion paints the end state and nothing else"
            );
        }
    }

    #[test]
    fn the_default_settings_animate() {
        assert_eq!(MotionSettings::default(), MotionSettings::animated());
        assert!(!MotionSettings::default().reduce_motion);
        assert!(MotionSettings::reduced().reduce_motion);
    }

    // -- Painting ---------------------------------------------------------

    #[test]
    fn every_step_keeps_its_character_and_stays_readable() {
        let now = Instant::now();
        let mut motion = Motion::default();
        let mut phases = vec![motion.start(MotionKind::Split, now)];
        phases.extend(frames(now).skip(1).filter_map(|at| motion.tick(at)));
        assert_eq!(phases.len(), usize::from(MOTION_STEPS) + 1);

        for phase in phases {
            let cell = phase_cell(accent_cell(), phase, frame());
            assert_eq!(cell.ch, 'x', "motion never changes a character");
            assert_ne!(cell.fg, frame(), "a step never blends all the way out");
            assert!(
                cell.attrs.contains(CellAttrs::BOLD),
                "a cell keeps the attributes chrome gave it"
            );
        }
    }

    #[test]
    fn the_ramp_runs_toward_the_chromes_own_colour() {
        let now = Instant::now();
        let mut motion = Motion::default();
        let start = motion.start(MotionKind::Focus, now);
        let mid = motion.tick(now + FRAME_BUDGET * 4).expect("a mid frame");
        let settled = Phase::settled(MotionKind::Focus);

        let distance = |phase: Phase| -> u16 {
            let Color::Rgb(r, ..) = phase_cell(accent_cell(), phase, frame()).fg else {
                unreachable!("the accent is 24-bit")
            };
            let Color::Rgb(target, ..) = accent_cell().fg else {
                unreachable!("the accent is 24-bit")
            };
            u16::from(target.abs_diff(r))
        };
        assert!(distance(start) > distance(mid), "the ramp closes");
        assert_eq!(distance(settled), 0, "and lands exactly on the chrome");
    }

    #[test]
    fn a_palette_colour_falls_back_to_the_dim_attribute() {
        let cell = Cell {
            ch: 'a',
            fg: Color::Indexed(4),
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        };
        let moving = phase_cell(
            cell,
            Phase {
                kind: MotionKind::Focus,
                step: 0,
            },
            frame(),
        );
        assert_eq!(moving.fg, Color::Indexed(4), "no colour is invented");
        assert!(moving.attrs.contains(CellAttrs::DIM));

        let settled = phase_cell(cell, Phase::settled(MotionKind::Focus), frame());
        assert_eq!(settled, cell, "the end state drops the fallback too");
    }

    #[test]
    fn a_terminal_palette_theme_has_no_frame_to_blend_toward() {
        let cell = accent_cell();
        let moving = phase_cell(
            cell,
            Phase {
                kind: MotionKind::Overlay,
                step: 0,
            },
            Color::Default,
        );
        assert!(
            moving.attrs.contains(CellAttrs::DIM),
            "an unknown background takes the attribute path"
        );
        assert_eq!(moving.fg, cell.fg);
    }

    #[test]
    fn a_span_keeps_its_origin_and_its_length() {
        let span = Span::new(Point::new(3, 9), vec![accent_cell(); 4]);
        let moved = phase_span(
            &span,
            Phase {
                kind: MotionKind::Split,
                step: 2,
            },
            frame(),
        );
        assert_eq!(moved.at, span.at, "motion never moves chrome");
        assert_eq!(moved.cells.len(), span.cells.len());
        assert_ne!(moved.cells, span.cells);
    }

    #[test]
    fn phase_cells_matches_the_single_cell_answer() {
        let phase = Phase {
            kind: MotionKind::Close,
            step: 3,
        };
        let cells = [accent_cell(); 3];
        assert_eq!(
            phase_cells(&cells, phase, frame()),
            cells
                .iter()
                .map(|c| phase_cell(*c, phase, frame()))
                .collect::<Vec<_>>()
        );
    }
}
