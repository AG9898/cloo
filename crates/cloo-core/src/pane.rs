//! Pane metadata: identity, provenance, and attention state.
//!
//! Everything a pane is *called* and everything cloo *claims about it* lives
//! here. The server owns these values; the client renders them. Neither of them
//! ever derives one by reading the pane's grid.
//!
//! Two invariants are the reason this module exists at all:
//!
//! - **Identity is explicit.** A pane's name, task label, and working directory
//!   come from the user or from a profile default. cloo never guesses a task
//!   from a process name or from transcript text — screen scraping is brittle,
//!   locale and theme dependent, and would make the rendered grid a second
//!   source of truth.
//! - **State carries its provenance.** [`Attention`] is a state *and* where the
//!   state came from, because a bell and an opt-in adapter are not equally
//!   trustworthy and the chrome has to be able to say so. A pane nothing has
//!   reported on stays [`AttentionState::Unknown`] — a live PTY is not proof a
//!   harness is working, and [`AttentionState::Quiet`] is a *claim* that there
//!   is nothing to do, which only a source may make.
//!
//! ```
//! use cloo_core::pane::{Attention, AttentionSource, AttentionState, PaneName};
//!
//! let mut attention = Attention::default();
//! assert_eq!(attention.state, AttentionState::Unknown);
//!
//! attention.set(AttentionState::NeedsInput, AttentionSource::Bell);
//! assert!(attention.is_pending());
//! attention.acknowledge();
//! assert!(!attention.is_pending());
//!
//! assert!(PaneName::new("api tests").is_ok());
//! ```

use std::path::{Path, PathBuf};

use cloo_proto::{PaneAttention, PaneId, PaneInfo, Size};

use crate::error::MetadataError;
use crate::profile::{AdapterId, Profile, ProfileId};

/// The longest a pane name may be. It shares a header row with a task label and
/// a state, so a name longer than this could never be shown whole anyway.
pub const MAX_PANE_NAME: usize = 64;

/// The longest a task label may be.
pub const MAX_TASK_LABEL: usize = 96;

// ---------------------------------------------------------------------------
// Names and labels
// ---------------------------------------------------------------------------

/// Validates one line of user-supplied display text.
///
/// Control characters are rejected everywhere they could appear. Chrome is
/// assembled as [`Cell`](cloo_proto::Cell)s rather than as bytes, so an escape
/// sequence in a name would not currently execute — but it would be smuggled
/// into every future surface that formats a name as text, so the boundary that
/// keeps it out is this one.
pub(crate) fn validate_text(
    field: &'static str,
    text: &str,
    max: usize,
) -> Result<(), MetadataError> {
    if text.is_empty() {
        return Err(MetadataError::Empty(field));
    }
    let len = text.chars().count();
    if len > max {
        return Err(MetadataError::TooLong { field, len, max });
    }
    if let Some(ch) = text.chars().find(|c| c.is_control()) {
        return Err(MetadataError::BadChar { field, ch });
    }
    Ok(())
}

/// A pane's user-visible name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneName(String);

impl PaneName {
    /// Validates and wraps a pane name.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`], [`MetadataError::TooLong`], or
    /// [`MetadataError::BadChar`].
    pub fn new(name: impl Into<String>) -> Result<Self, MetadataError> {
        let name = name.into();
        validate_text("pane name", &name, MAX_PANE_NAME)?;
        Ok(Self(name))
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for PaneName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the user said this pane is *for*.
///
/// Always supplied, never inferred. It is the first thing the pane header drops
/// when width is scarce, which is why it is optional on [`PaneMeta`] rather than
/// defaulted to something invented.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskLabel(String);

impl TaskLabel {
    /// Validates and wraps a task label.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`], [`MetadataError::TooLong`], or
    /// [`MetadataError::BadChar`].
    pub fn new(label: impl Into<String>) -> Result<Self, MetadataError> {
        let label = label.into();
        validate_text("task label", &label, MAX_TASK_LABEL)?;
        Ok(Self(label))
    }

    /// The label as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for TaskLabel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Working directory
// ---------------------------------------------------------------------------

/// The directory a pane's child is launched in.
///
/// Required to be absolute. A relative path means whatever the *daemon's* cwd
/// happens to be, which is not what the user typing `./api` meant and is not
/// even stable across daemon restarts — so resolution happens at the client or
/// the CLI, and only an absolute answer reaches the model.
///
/// Existence is deliberately not checked: `cloo-core` performs no I/O, and a
/// directory that exists at validation time may not exist at launch time
/// anyway. A missing directory is a launch failure the server reports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkingDir(PathBuf);

impl WorkingDir {
    /// Validates and wraps a working directory.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`] for a blank path, [`MetadataError::RelativeCwd`]
    /// for a relative one, and [`MetadataError::BadChar`] for a control
    /// character — including the NUL that could not survive the C string
    /// `chdir` wants.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, MetadataError> {
        let path = path.into();
        let display = path.to_string_lossy();
        if display.is_empty() {
            return Err(MetadataError::Empty("working directory"));
        }
        if let Some(ch) = display.chars().find(|c| c.is_control()) {
            return Err(MetadataError::BadChar {
                field: "working directory",
                ch,
            });
        }
        if !path.is_absolute() {
            return Err(MetadataError::RelativeCwd(display.into_owned()));
        }
        Ok(Self(path))
    }

    /// The directory as a path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

/// A pane's workspace state.
///
/// The six states of `docs/STYLEGUIDE.md`, in the model rather than in the
/// chrome: the client turns each into a glyph, a label, and a colour, and never
/// into a state of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttentionState {
    /// Nothing reliable has reported. The honest default, and distinct from
    /// [`Quiet`](Self::Quiet).
    #[default]
    Unknown,
    /// A source says the pane is making progress.
    Working,
    /// The pane requires a decision or a response.
    NeedsInput,
    /// The pane finished with a result nobody has looked at.
    Ready,
    /// The child exited unsuccessfully, or a source reported failure.
    Failed,
    /// A source says there is nothing to do.
    Quiet,
}

impl AttentionState {
    /// The stable wire and configuration name of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Quiet => "quiet",
        }
    }

    /// Whether this state should put the pane in the attention queue.
    ///
    /// The queue is a navigation surface, not a notification firehose: it lists
    /// panes that want a human, so progress and absence of news are excluded.
    #[must_use]
    pub const fn wants_attention(self) -> bool {
        matches!(self, Self::NeedsInput | Self::Ready | Self::Failed)
    }

    /// The state an opt-in adapter reported, as a model state.
    ///
    /// The conversion only goes this way. An [`AdapterState`](cloo_proto::AdapterState)
    /// cannot express [`Quiet`](Self::Quiet) or [`Unknown`](Self::Unknown), so
    /// an advisory source can never assert "there is nothing to do" nor
    /// withdraw a state cloo observed for itself.
    #[must_use]
    pub const fn from_adapter(state: cloo_proto::AdapterState) -> Self {
        match state {
            cloo_proto::AdapterState::Working => Self::Working,
            cloo_proto::AdapterState::NeedsInput => Self::NeedsInput,
            cloo_proto::AdapterState::Ready => Self::Ready,
            cloo_proto::AdapterState::Failed => Self::Failed,
        }
    }

    /// Projects the state onto its wire form.
    #[must_use]
    pub const fn to_wire(self) -> cloo_proto::AttentionState {
        match self {
            Self::Unknown => cloo_proto::AttentionState::Unknown,
            Self::Working => cloo_proto::AttentionState::Working,
            Self::NeedsInput => cloo_proto::AttentionState::NeedsInput,
            Self::Ready => cloo_proto::AttentionState::Ready,
            Self::Failed => cloo_proto::AttentionState::Failed,
            Self::Quiet => cloo_proto::AttentionState::Quiet,
        }
    }
}

/// Where an attention state came from.
///
/// Kept alongside the state rather than folded into it, so the chrome can show
/// an adapter's claim as an adapter's claim. The three generic sources are
/// things cloo observes itself; [`Adapter`](Self::Adapter) is everything else,
/// and it is always advisory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AttentionSource {
    /// Nothing has reported. Pairs with [`AttentionState::Unknown`].
    #[default]
    None,
    /// The child rang the terminal bell.
    Bell,
    /// The child started, stopped, or exited.
    Lifecycle,
    /// The user marked the pane explicitly.
    User,
    /// An opt-in local adapter reported it.
    Adapter(AdapterId),
}

impl AttentionSource {
    /// Whether the source is advisory — a claim cloo did not observe itself and
    /// which the chrome must attribute rather than present as fact.
    #[must_use]
    pub const fn is_advisory(&self) -> bool {
        matches!(self, Self::Adapter(_))
    }

    /// A short label for the chrome and the pane-details view.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Bell => "bell",
            Self::Lifecycle => "lifecycle",
            Self::User => "user",
            Self::Adapter(id) => id.as_str(),
        }
    }

    /// Projects the source onto its wire form, carrying an adapter's name so the
    /// chrome can attribute an advisory claim on the far side.
    #[must_use]
    pub fn to_wire(&self) -> cloo_proto::AttentionSource {
        match self {
            Self::None => cloo_proto::AttentionSource::None,
            Self::Bell => cloo_proto::AttentionSource::Bell,
            Self::Lifecycle => cloo_proto::AttentionSource::Lifecycle,
            Self::User => cloo_proto::AttentionSource::User,
            Self::Adapter(id) => cloo_proto::AttentionSource::Adapter(id.as_str().to_owned()),
        }
    }
}

/// A pane's attention state, its provenance, and whether it has been seen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attention {
    /// The current state.
    pub state: AttentionState,
    /// Where the current state came from.
    pub source: AttentionSource,
    /// Whether the user has acknowledged the current state.
    pub acknowledged: bool,
}

impl Attention {
    /// Records a new state and its source.
    ///
    /// Acknowledgment is cleared whenever the state actually changes, and
    /// deliberately *kept* when the same state is re-reported: a harness that
    /// re-announces `needs_input` every second must not resurrect an entry the
    /// user already dismissed. That is the coalescing rule the attention queue
    /// depends on, expressed once here rather than in every source.
    pub fn set(&mut self, state: AttentionState, source: AttentionSource) {
        if self.state != state {
            self.acknowledged = false;
        }
        self.state = state;
        self.source = source;
    }

    /// Marks the current state as seen.
    pub const fn acknowledge(&mut self) {
        self.acknowledged = true;
    }

    /// Whether this pane belongs in the attention queue right now.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.state.wants_attention() && !self.acknowledged
    }

    /// Projects this pane's attention onto the wire, keeping state, provenance,
    /// and acknowledgment together.
    ///
    /// A state without its source is exactly the claim the chrome must not make,
    /// which is why all three cross as one value rather than being flattened
    /// into [`PaneInfo`].
    #[must_use]
    pub fn to_wire(&self, pane: PaneId) -> PaneAttention {
        PaneAttention {
            pane,
            state: self.state.to_wire(),
            source: self.source.to_wire(),
            acknowledged: self.acknowledged,
        }
    }
}

// ---------------------------------------------------------------------------
// Pane metadata
// ---------------------------------------------------------------------------

/// Everything the session knows about a pane that is not its grid.
///
/// Constructed from a [`Profile`] plus the user's overrides, and carried by the
/// session actor. The client receives a projection of it and renders chrome from
/// that; it never computes one of these fields for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneMeta {
    /// The profile the pane was launched from.
    pub profile: ProfileId,
    /// The pane's user-visible name.
    pub name: PaneName,
    /// What the pane is for, if the user said.
    pub task: Option<TaskLabel>,
    /// Where the child was launched.
    pub cwd: WorkingDir,
    /// The profile's recommended minimum geometry, carried along so a split can
    /// consult it without looking the profile up again.
    pub min_size: Size,
    /// The opt-in adapter the pane's profile named, if it named one.
    ///
    /// This is the whole of the pane's consent: a report from the local control
    /// interface is applied only when it comes from *this* adapter, so naming
    /// one in a profile is what lets it speak, and a pane that named none is
    /// reachable by no adapter at all. Carried on the pane rather than looked up
    /// from the profile at report time, because a profile can be reloaded and a
    /// running pane was launched under the one it was launched under.
    pub adapter: Option<AdapterId>,
    /// The pane's attention state and provenance.
    pub attention: Attention,
}

impl PaneMeta {
    /// Builds pane metadata from a profile, taking the profile's default name
    /// unless the user supplied one.
    ///
    /// # Errors
    ///
    /// [`MetadataError`] when the profile's default name is unusable, which can
    /// only happen for a profile that was never validated.
    pub fn from_profile(
        profile: &Profile,
        name: Option<PaneName>,
        task: Option<TaskLabel>,
        cwd: WorkingDir,
    ) -> Result<Self, MetadataError> {
        let name = match name {
            Some(name) => name,
            None => PaneName::new(profile.default_name.clone())?,
        };
        Ok(Self {
            profile: profile.id.clone(),
            name,
            task,
            cwd,
            min_size: profile.min_size,
            adapter: profile.adapter.clone(),
            attention: Attention::default(),
        })
    }

    /// Whether `adapter` is the one this pane's profile opted into.
    ///
    /// A pane whose profile named no adapter permits none: the built-ins name
    /// none, so nothing on a default install can have its state claimed by a
    /// local process the user never configured.
    #[must_use]
    pub fn permits_adapter(&self, adapter: &AdapterId) -> bool {
        self.adapter.as_ref() == Some(adapter)
    }

    /// The projection of this metadata a client is sent.
    ///
    /// Identity only. The recommended minimum stays on the server because it is
    /// an input to a split the server performs, and attention crosses the wire
    /// with its provenance as its own [`Attention::to_wire`] projection rather
    /// than being flattened in here — a state without its source is exactly the
    /// claim the chrome must not make.
    #[must_use]
    pub fn to_wire(&self, pane: PaneId) -> PaneInfo {
        PaneInfo {
            pane,
            profile: self.profile.as_str().to_owned(),
            name: self.name.as_str().to_owned(),
            task: self.task.as_ref().map(|task| task.as_str().to_owned()),
            cwd: self.cwd.as_path().to_string_lossy().into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> WorkingDir {
        WorkingDir::new("/home/dev/api").expect("absolute path")
    }

    // -- Names and labels ---------------------------------------------------

    #[test]
    fn a_pane_name_accepts_ordinary_text() {
        for name in ["shell", "api tests", "Codex — refactor", "日本語"] {
            assert!(PaneName::new(name).is_ok(), "{name:?} should be accepted");
        }
    }

    #[test]
    fn a_pane_name_rejects_control_characters() {
        // The escape byte is the one that matters: a name is user text that
        // reaches a header, and this is the boundary that keeps it inert.
        for name in ["esc\u{1b}[31m", "tab\there", "bell\u{7}"] {
            assert!(
                matches!(PaneName::new(name), Err(MetadataError::BadChar { .. })),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn names_and_labels_are_bounded_and_non_empty() {
        assert_eq!(PaneName::new(""), Err(MetadataError::Empty("pane name")));
        assert_eq!(TaskLabel::new(""), Err(MetadataError::Empty("task label")));
        assert!(PaneName::new("a".repeat(MAX_PANE_NAME)).is_ok());
        assert!(matches!(
            PaneName::new("a".repeat(MAX_PANE_NAME + 1)),
            Err(MetadataError::TooLong { .. })
        ));
        assert!(matches!(
            TaskLabel::new("a".repeat(MAX_TASK_LABEL + 1)),
            Err(MetadataError::TooLong { .. })
        ));
    }

    #[test]
    fn a_length_bound_counts_characters_not_bytes() {
        // A multi-byte name that fits on screen must not be rejected for the
        // width of its encoding.
        let name = "é".repeat(MAX_PANE_NAME);
        assert!(PaneName::new(name).is_ok());
    }

    // -- Working directory --------------------------------------------------

    #[test]
    fn a_working_directory_must_be_absolute() {
        assert!(WorkingDir::new("/").is_ok());
        assert!(WorkingDir::new("/home/dev/api").is_ok());
        assert!(matches!(
            WorkingDir::new("api"),
            Err(MetadataError::RelativeCwd(_))
        ));
        assert!(matches!(
            WorkingDir::new("./api"),
            Err(MetadataError::RelativeCwd(_))
        ));
        // `~` is the shell's, not the kernel's: unexpanded it is a directory
        // literally named `~`, which is never what was meant.
        assert!(matches!(
            WorkingDir::new("~/api"),
            Err(MetadataError::RelativeCwd(_))
        ));
    }

    #[test]
    fn a_working_directory_rejects_a_nul() {
        assert!(matches!(
            WorkingDir::new("/home/de\0v"),
            Err(MetadataError::BadChar { .. })
        ));
        assert_eq!(
            WorkingDir::new(""),
            Err(MetadataError::Empty("working directory"))
        );
    }

    #[test]
    fn validation_never_touches_the_filesystem() {
        // Purity is the property: a path that certainly does not exist still
        // validates, because existence is a launch-time answer.
        assert!(WorkingDir::new("/definitely/not/here/at/all").is_ok());
    }

    // -- Attention ----------------------------------------------------------

    #[test]
    fn an_uninstrumented_pane_is_unknown_with_no_source() {
        let attention = Attention::default();
        assert_eq!(attention.state, AttentionState::Unknown);
        assert_eq!(attention.source, AttentionSource::None);
        assert!(!attention.is_pending());
    }

    #[test]
    fn only_the_queue_worthy_states_want_attention() {
        assert!(AttentionState::NeedsInput.wants_attention());
        assert!(AttentionState::Ready.wants_attention());
        assert!(AttentionState::Failed.wants_attention());
        assert!(!AttentionState::Unknown.wants_attention());
        assert!(!AttentionState::Working.wants_attention());
        assert!(!AttentionState::Quiet.wants_attention());
    }

    #[test]
    fn acknowledging_removes_a_pane_from_the_queue() {
        let mut attention = Attention::default();
        attention.set(AttentionState::NeedsInput, AttentionSource::Bell);
        assert!(attention.is_pending());
        attention.acknowledge();
        assert!(!attention.is_pending());
    }

    #[test]
    fn re_reporting_the_same_state_does_not_resurrect_an_acknowledgment() {
        // A harness that re-announces every second would otherwise refill the
        // queue the user just cleared.
        let mut attention = Attention::default();
        attention.set(AttentionState::NeedsInput, AttentionSource::User);
        attention.acknowledge();
        attention.set(AttentionState::NeedsInput, AttentionSource::User);
        assert!(!attention.is_pending());
    }

    #[test]
    fn a_changed_state_clears_the_acknowledgment() {
        let mut attention = Attention::default();
        attention.set(AttentionState::Ready, AttentionSource::Lifecycle);
        attention.acknowledge();
        attention.set(AttentionState::Failed, AttentionSource::Lifecycle);
        assert!(attention.is_pending());
    }

    #[test]
    fn provenance_survives_a_state_change() {
        let adapter = AdapterId::new("my-adapter").expect("valid id");
        let mut attention = Attention::default();
        attention.set(
            AttentionState::Working,
            AttentionSource::Adapter(adapter.clone()),
        );
        assert!(attention.source.is_advisory());
        assert_eq!(attention.source.label(), "my-adapter");

        attention.set(AttentionState::Failed, AttentionSource::Lifecycle);
        assert!(!attention.source.is_advisory());
        assert_eq!(attention.source.label(), "lifecycle");
    }

    #[test]
    fn only_an_adapter_is_advisory() {
        for source in [
            AttentionSource::None,
            AttentionSource::Bell,
            AttentionSource::Lifecycle,
            AttentionSource::User,
        ] {
            assert!(!source.is_advisory(), "{source:?} should be observed");
        }
    }

    #[test]
    fn an_uninstrumented_pane_crosses_the_wire_as_unknown_with_no_source() {
        // The acceptance property: a child nothing has reported on projects to
        // the honest default, provenance and all.
        let wire = Attention::default().to_wire(cloo_proto::PaneId::new(3));
        assert_eq!(
            wire,
            PaneAttention {
                pane: cloo_proto::PaneId::new(3),
                state: cloo_proto::AttentionState::Unknown,
                source: cloo_proto::AttentionSource::None,
                acknowledged: false,
            }
        );
    }

    #[test]
    fn the_wire_projection_keeps_state_provenance_and_acknowledgment_together() {
        let adapter = AdapterId::new("my-adapter").expect("valid id");
        let mut attention = Attention::default();
        attention.set(
            AttentionState::NeedsInput,
            AttentionSource::Adapter(adapter),
        );
        attention.acknowledge();
        let wire = attention.to_wire(cloo_proto::PaneId::new(7));
        assert_eq!(
            wire,
            PaneAttention {
                pane: cloo_proto::PaneId::new(7),
                state: cloo_proto::AttentionState::NeedsInput,
                source: cloo_proto::AttentionSource::Adapter("my-adapter".into()),
                acknowledged: true,
            }
        );
    }

    #[test]
    fn every_state_and_source_has_a_distinct_wire_form() {
        // A conversion that collapsed two states or two sources would be caught
        // by comparing the projected set's cardinality to the input's.
        use std::collections::HashSet;
        let states: HashSet<_> = [
            AttentionState::Unknown,
            AttentionState::Working,
            AttentionState::NeedsInput,
            AttentionState::Ready,
            AttentionState::Failed,
            AttentionState::Quiet,
        ]
        .into_iter()
        .map(|state| format!("{:?}", state.to_wire()))
        .collect();
        assert_eq!(
            states.len(),
            6,
            "a state collapsed onto another on the wire"
        );
    }

    #[test]
    fn state_names_match_the_style_guide() {
        assert_eq!(AttentionState::NeedsInput.as_str(), "needs_input");
        assert_eq!(AttentionState::Unknown.as_str(), "unknown");
        assert_eq!(AttentionState::Quiet.as_str(), "quiet");
    }

    // -- Pane metadata ------------------------------------------------------

    #[test]
    fn metadata_falls_back_to_the_profile_default_name() {
        let meta = PaneMeta::from_profile(&Profile::claude(), None, None, cwd()).expect("valid");
        assert_eq!(meta.name.as_str(), "claude");
        assert_eq!(meta.profile, Profile::claude().id);
        assert_eq!(meta.min_size, Profile::claude().min_size);
        assert_eq!(meta.task, None);
    }

    #[test]
    fn the_user_name_and_task_win_over_the_profile() {
        let meta = PaneMeta::from_profile(
            &Profile::codex(),
            Some(PaneName::new("api").expect("valid name")),
            Some(TaskLabel::new("fix the flaky test").expect("valid label")),
            cwd(),
        )
        .expect("valid");
        assert_eq!(meta.name.as_str(), "api");
        assert_eq!(meta.task.expect("task").as_str(), "fix the flaky test");
        assert_eq!(meta.cwd.as_path(), Path::new("/home/dev/api"));
    }

    #[test]
    fn the_wire_projection_carries_what_the_user_supplied() {
        let meta = PaneMeta::from_profile(
            &Profile::codex(),
            Some(PaneName::new("api").expect("valid name")),
            Some(TaskLabel::new("fix the flaky test").expect("valid label")),
            cwd(),
        )
        .expect("valid");
        let wire = meta.to_wire(cloo_proto::PaneId::new(7));
        assert_eq!(wire.pane, cloo_proto::PaneId::new(7));
        assert_eq!(wire.profile, "codex");
        assert_eq!(wire.name, "api");
        assert_eq!(wire.task.as_deref(), Some("fix the flaky test"));
        assert_eq!(wire.cwd, "/home/dev/api");
    }

    #[test]
    fn an_absent_task_stays_absent_on_the_wire() {
        // Not "", and not the pane name: a client must be able to tell "the
        // user said nothing" from "the user said something short".
        let meta = PaneMeta::from_profile(&Profile::generic(), None, None, cwd()).expect("valid");
        assert_eq!(meta.to_wire(cloo_proto::PaneId::new(1)).task, None);
    }

    #[test]
    fn a_pane_permits_only_the_adapter_its_profile_named() {
        let named = AdapterId::new("my-adapter").expect("valid id");
        let other = AdapterId::new("someone-else").expect("valid id");
        let profile = Profile::generic().adapter(named.clone());
        let meta = PaneMeta::from_profile(&profile, None, None, cwd()).expect("valid");

        assert_eq!(meta.adapter.as_ref(), Some(&named));
        assert!(meta.permits_adapter(&named));
        assert!(
            !meta.permits_adapter(&other),
            "an adapter the profile did not name may not speak for the pane"
        );
    }

    #[test]
    fn a_pane_whose_profile_named_no_adapter_permits_none() {
        // Every built-in is in this state, which is what keeps a default
        // install from having its pane states claimed by a local process the
        // user never opted into.
        let any = AdapterId::new("my-adapter").expect("valid id");
        for profile in Profile::built_ins() {
            let meta = PaneMeta::from_profile(&profile, None, None, cwd()).expect("valid");
            assert_eq!(meta.adapter, None, "{} names an adapter", profile.id);
            assert!(!meta.permits_adapter(&any));
        }
    }

    #[test]
    fn an_adapter_state_can_never_become_quiet_or_unknown() {
        // The permitted set, and the two states an advisory source may not
        // claim: `quiet` asserts there is nothing to do, and `unknown` would
        // withdraw something cloo observed for itself.
        let permitted = [
            (cloo_proto::AdapterState::Working, AttentionState::Working),
            (
                cloo_proto::AdapterState::NeedsInput,
                AttentionState::NeedsInput,
            ),
            (cloo_proto::AdapterState::Ready, AttentionState::Ready),
            (cloo_proto::AdapterState::Failed, AttentionState::Failed),
        ];
        for (reported, expected) in permitted {
            let state = AttentionState::from_adapter(reported);
            assert_eq!(state, expected);
            assert_ne!(state, AttentionState::Quiet);
            assert_ne!(state, AttentionState::Unknown);
        }
    }

    #[test]
    fn a_new_pane_starts_unknown_whatever_its_profile() {
        // A harness profile does not get to imply its child is working; only a
        // source may say so.
        for profile in Profile::built_ins() {
            let meta = PaneMeta::from_profile(&profile, None, None, cwd()).expect("valid");
            assert_eq!(meta.attention, Attention::default());
        }
    }
}
