//! Deterministic visual fixtures for composed client frames.
//!
//! Every fixture here asserts a *complete* fixed-size cell matrix — character,
//! semantic foreground and background role, and rendition — for a frame the
//! production [`compose_frame`](cloo_client::renderer::compose_frame) produced.
//! Nothing in this file opens a pseudoterminal, connects to a daemon, or writes
//! a byte to a descriptor: a scene is client-side state, and a frame is a pure
//! function of it.
//!
//! The golden is written as text (see [`harness::ExpectedFrame`]) and names
//! style-guide roles rather than colours, so one expectation is checked against
//! both a truecolor theme and its 16-colour resolution. Roles that chrome still
//! draws from the reference Storm palette instead of the client theme are
//! written as `Paint::Reference`, which is what keeps the remaining M9 gap
//! visible in the fixture rather than hidden inside a raw RGB triple.

#[path = "visual/harness.rs"]
mod harness;
#[path = "visual/scenes.rs"]
mod scenes;

use cloo_client::chrome::{Attention, PaneChrome, PrefixHint};
use cloo_client::input::PaletteAction;
use cloo_client::overlay::{ConfigPreview, Overlay, SessionEntry, overlay_spans};
use cloo_client::renderer::Span;
use cloo_client::theme::{Theme, ThemeToken};
use cloo_core::{BorderStyle, Keymap, StatusMode, ThemeChoice, ThemeName, VisualConfig};
use cloo_proto::{
    Cell, CellAttrs, Color, Direction, PaneId, Point, SessionSummary, Size, TermCaps,
};
use std::path::PathBuf;

use harness::{ExpectedFrame, FrameMatrix, Paint, SemanticStyle, assert_frame, check_frame};
use scenes::{Scene, ScenePane};

/// A named theme resolved for a terminal that never negotiated true colour.
fn sixteen_color(name: ThemeName) -> Theme {
    Theme::new(ThemeChoice::Named(name), TermCaps::default())
}

// ---------------------------------------------------------------------------
// The session-aware tab row
// ---------------------------------------------------------------------------

/// The tab row's legend: badge, active chip, inactive chip, padding, metadata.
///
/// Every entry is a [`Paint::Token`], because the row resolves through the
/// client theme rather than drawing the reference palette at every colour depth.
fn tab_row_styles(frame: ExpectedFrame) -> ExpectedFrame {
    let surface = Paint::Token(ThemeToken::Surface);
    let muted = SemanticStyle::new(Paint::Token(ThemeToken::Muted), surface);
    frame
        .style(
            'S',
            SemanticStyle::new(surface, Paint::Token(ThemeToken::Accent)).attrs(CellAttrs::BOLD),
        )
        .style(
            'T',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Accent),
                Paint::Token(ThemeToken::RaisedSurface),
            )
            .attrs(CellAttrs::BOLD.union(CellAttrs::UNDERLINE)),
        )
        // An inactive chip and the right-side metadata share one style; they are
        // separate keys so the picture stays readable.
        .style('i', muted)
        .style('w', muted)
        .style(',', SemanticStyle::new(Paint::Terminal, surface))
}

/// A three-tab workspace whose complete top row is the fixture under test.
///
/// The two panes below row zero are headerless and one cell each: they exist so
/// the pane count the row reports comes from the composed layout rather than
/// from a number the fixture asserted about itself.
fn tab_bar(width: u16) -> Scene {
    let pane = |id: u16, x: u16| {
        ScenePane::new(
            PaneId::new(u64::from(id)),
            x,
            1,
            Size::new(1, 1),
            PaneChrome::new(id, "sh"),
        )
        .headerless()
    };
    Scene::new(Size::new(width, 3))
        .named("dev")
        .clients(1)
        .tab("edit", false)
        .tab("build", true)
        .tab("logs", false)
        .pane(pane(1, 0))
        .pane(pane(2, 1))
}

/// The documented width ladder: terminal width, the complete row, its styles.
///
/// Read top to bottom this is the yield order from `docs/STYLEGUIDE.md`: the
/// right-side metadata compacts and then disappears, then inactive tabs yield
/// from the far right and then the far left, then the session badge reduces to
/// its glyph and disappears, and only then does the active title truncate.
const TAB_ROW_LADDER: [(u16, &str, &str); 8] = [
    (
        60,
        " dev  1 edit >2 build  3 logs              2 panes  1 client",
        "SSSSSiiiiiiiiTTTTTTTTiiiiiiii,,,,,,,,,,,,,,wwwwwwwwwwwwwwwww",
    ),
    (
        40,
        " dev  1 edit >2 build  3 logs      2p 1c",
        "SSSSSiiiiiiiiTTTTTTTTiiiiiiii,,,,,,wwwww",
    ),
    (
        32,
        " dev  1 edit >2 build  3 logs   ",
        "SSSSSiiiiiiiiTTTTTTTTiiiiiiii,,,",
    ),
    (24, " dev  1 edit >2 build   ", "SSSSSiiiiiiiiTTTTTTTT,,,"),
    (16, " dev >2 build   ", "SSSSSTTTTTTTT,,,"),
    (12, " s >2 build ", "SSSTTTTTTTT,"),
    (8, ">2 build", "TTTTTTTT"),
    (5, ">2 bu", "TTTTT"),
];

#[test]
fn the_tab_row_matches_its_truecolor_goldens_at_every_documented_width() {
    for (width, text, keys) in TAB_ROW_LADDER {
        let scene = tab_bar(width);
        assert_frame(
            &scene.frame(Theme::storm()).only_row(0),
            &tab_row_styles(ExpectedFrame::new()).row(text, keys),
            Theme::storm(),
        );
    }
}

#[test]
fn the_tab_row_matches_the_same_goldens_in_sixteen_colors() {
    let theme = sixteen_color(ThemeName::Storm);
    assert_ne!(
        theme.color(ThemeToken::RaisedSurface),
        Theme::storm().color(ThemeToken::RaisedSurface),
        "the 16-colour fixture must not resolve to the truecolor palette"
    );
    for (width, text, keys) in TAB_ROW_LADDER {
        let scene = tab_bar(width);
        assert_frame(
            &scene.frame(theme).only_row(0),
            &tab_row_styles(ExpectedFrame::new()).row(text, keys),
            theme,
        );
    }
}

// ---------------------------------------------------------------------------
// Card 01 — the daily one-pane workspace
// ---------------------------------------------------------------------------

/// One tab, one focused pane with a header, and the always-on status row.
fn workspace() -> Scene {
    Scene::new(Size::new(40, 8))
        .named("dev")
        .clients(1)
        .tab("main", true)
        .hint(PrefixHint::new("C-b"))
        .pane(
            ScenePane::new(
                PaneId::new(1),
                1,
                2,
                Size::new(38, 4),
                PaneChrome::new(1, "shell").focused(true),
            )
            .text(&["$ ls", "Cargo.toml  src"]),
        )
}

/// The complete expected frame for [`workspace`], role by role.
///
/// The status row, tab row, pane frame, and pane body all resolve through the
/// client theme; the golden names semantic roles rather than literal colors.
fn workspace_golden() -> ExpectedFrame {
    let chrome_bg = Paint::Token(ThemeToken::Surface);
    let pane_bg = Paint::Token(ThemeToken::Surface);
    let frame_bg = Paint::Token(ThemeToken::Frame);

    tab_row_styles(ExpectedFrame::new())
        .style(
            'd',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Surface),
                Paint::Token(ThemeToken::Accent),
            )
            .attrs(CellAttrs::BOLD),
        )
        .style('.', SemanticStyle::new(Paint::Terminal, chrome_bg))
        .style(
            'u',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Muted),
                Paint::Token(ThemeToken::RaisedSurface),
            )
            .attrs(CellAttrs::BOLD.union(CellAttrs::UNDERLINE)),
        )
        .style(
            'v',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Accent),
                Paint::Token(ThemeToken::RaisedSurface),
            )
            .attrs(CellAttrs::BOLD.union(CellAttrs::UNDERLINE)),
        )
        .style(
            'i',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Info),
                Paint::Token(ThemeToken::RaisedSurface),
            )
            .attrs(CellAttrs::BOLD.union(CellAttrs::UNDERLINE)),
        )
        .style(
            't',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Primary),
                Paint::Token(ThemeToken::RaisedSurface),
            )
            .attrs(CellAttrs::BOLD.union(CellAttrs::UNDERLINE)),
        )
        .style(
            'M',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), chrome_bg).attrs(CellAttrs::BOLD),
        )
        .style(
            '-',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), chrome_bg),
        )
        .style(
            'p',
            SemanticStyle::new(Paint::Token(ThemeToken::Primary), chrome_bg),
        )
        .style(
            'a',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), pane_bg),
        )
        .style(
            'F',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), frame_bg),
        )
        .style(
            'B',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), pane_bg).attrs(CellAttrs::BOLD),
        )
        .style(
            'm',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), pane_bg),
        )
        .style('s', SemanticStyle::new(Paint::Terminal, pane_bg))
        .style(
            '~',
            SemanticStyle::new(
                Paint::Token(ThemeToken::DefaultText),
                Paint::Token(ThemeToken::Surface),
            ),
        )
        .row(
            " dev >1 main            1 pane  1 client",
            "SSSSSTTTTTTT,,,,,,,,,,,,wwwwwwwwwwwwwwww",
        )
        .row(
            "╭> 1 shell                    ? unknown╮",
            "FaammBBBBBssssssssssssssssssssmmmmmmmmmF",
        )
        .row(
            "│$ ls                                  │",
            "F~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~F",
        )
        .row(
            "│Cargo.toml  src                       │",
            "F~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~F",
        )
        .row(
            "│                                      │",
            "F~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~F",
        )
        .row(
            "│                                      │",
            "F~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~F",
        )
        .row(
            "╰──────────────────────────────────────╯",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        )
        .row(
            " s dev  >1 main  0!       1 client  C-b ",
            "ddddddduviuttttu.MM......----------.ppp.",
        )
}

#[test]
fn the_one_pane_workspace_matches_its_truecolor_golden() {
    let scene = workspace();
    assert_frame(
        &scene.frame(Theme::storm()),
        &workspace_golden(),
        Theme::storm(),
    );
}

#[test]
fn the_one_pane_workspace_matches_the_same_golden_in_sixteen_colors() {
    let theme = sixteen_color(ThemeName::Storm);
    // The same expectation, resolved for a terminal without 24-bit colour. It is
    // only a real second assertion because the roles resolve differently: the
    // themed header cells are palette indices here and RGB above.
    assert_ne!(
        theme.color(ThemeToken::Accent),
        Theme::storm().color(ThemeToken::Accent),
        "the 16-colour fixture must not resolve to the truecolor palette"
    );
    let scene = workspace();
    assert_frame(&scene.frame(theme), &workspace_golden(), theme);
}

// ---------------------------------------------------------------------------
// The shared workspace legend
// ---------------------------------------------------------------------------

/// Every role a composed workspace frame draws, as one legend.
///
/// Cards 02, 03, 07, and 08 share it, which is the point: a role that means one
/// thing on the split card has to mean the same thing on the nested one, and a
/// single legend is what makes that checkable rather than conventional. Upper
/// case is chrome that sits on a raised or framed ground, lower case is chrome
/// and content on the base surface, and every key whose style is
/// [`SemanticStyle::dimmed`] belongs to an unfocused pane.
fn workspace_styles(frame: ExpectedFrame) -> ExpectedFrame {
    let surface = Paint::Token(ThemeToken::Surface);
    let raised = Paint::Token(ThemeToken::RaisedSurface);
    let frame_bg = Paint::Token(ThemeToken::Frame);
    let border = Paint::Token(ThemeToken::Border);
    let accent = Paint::Token(ThemeToken::Accent);
    let primary = Paint::Token(ThemeToken::Primary);
    let muted = Paint::Token(ThemeToken::Muted);
    let warning = Paint::Token(ThemeToken::Warning);
    let error = Paint::Token(ThemeToken::Error);
    let bold_underline = CellAttrs::BOLD.union(CellAttrs::UNDERLINE);

    frame
        // -- always-on chrome on a raised or bordered ground ----------------
        .style(
            'S',
            SemanticStyle::new(surface, accent).attrs(CellAttrs::BOLD),
        )
        .style(
            'T',
            SemanticStyle::new(accent, raised).attrs(bold_underline),
        )
        .style(
            'V',
            SemanticStyle::new(primary, raised).attrs(bold_underline),
        )
        .style(
            'N',
            SemanticStyle::new(Paint::Token(ThemeToken::Info), raised).attrs(bold_underline),
        )
        .style('U', SemanticStyle::new(muted, raised).attrs(bold_underline))
        .style('K', SemanticStyle::new(muted, raised))
        .style('Q', SemanticStyle::new(accent, border))
        .style(
            'R',
            SemanticStyle::new(primary, border).attrs(CellAttrs::BOLD),
        )
        .style('D', SemanticStyle::new(border, raised))
        .style('G', SemanticStyle::new(raised, surface))
        // -- chrome and content on the base surface -------------------------
        .style(',', SemanticStyle::new(Paint::Terminal, surface))
        .style('m', SemanticStyle::new(muted, surface))
        .style('p', SemanticStyle::new(primary, surface))
        .style('a', SemanticStyle::new(accent, surface))
        .style(
            'B',
            SemanticStyle::new(accent, surface).attrs(CellAttrs::BOLD),
        )
        .style('F', SemanticStyle::new(accent, frame_bg))
        .style(
            'L',
            SemanticStyle::new(accent, frame_bg).attrs(CellAttrs::BOLD),
        )
        .style(
            'g',
            SemanticStyle::new(Paint::Token(ThemeToken::Success), surface).attrs(CellAttrs::BOLD),
        )
        .style('w', SemanticStyle::new(warning, surface))
        .style(
            'W',
            SemanticStyle::new(warning, surface).attrs(CellAttrs::BOLD),
        )
        .style('X', SemanticStyle::new(error, surface))
        .style(
            'Y',
            SemanticStyle::new(error, surface).attrs(CellAttrs::BOLD),
        )
        .style(
            'c',
            SemanticStyle::new(primary, surface).attrs(CellAttrs::BOLD),
        )
        .style(
            '~',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), surface),
        )
        // -- the same roles after the unfocused-pane treatment --------------
        .style('f', SemanticStyle::new(border, frame_bg).dimmed())
        .style('b', SemanticStyle::new(border, surface).dimmed())
        .style('k', SemanticStyle::new(muted, surface).dimmed())
        .style('P', SemanticStyle::new(primary, surface).dimmed())
        .style('x', SemanticStyle::new(Paint::Terminal, surface).dimmed())
        .style('H', SemanticStyle::new(warning, surface).dimmed())
        .style('Z', SemanticStyle::new(error, surface).dimmed())
        .style(
            'y',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), surface).dimmed(),
        )
        // -- the gutter no span paints --------------------------------------
        .style('_', SemanticStyle::new(Paint::Terminal, Paint::Terminal))
}

// ---------------------------------------------------------------------------
// Card 02 — the vertical split
// ---------------------------------------------------------------------------

/// Two equal panes with a one-cell gutter and card 07's rich status row.
///
/// The right pane is unfocused *and* waiting, which is the pair card 02 exists
/// to prove: dimming reduces contrast toward the frame without turning an amber
/// `needs input` into the same grey as a quiet neighbour.
fn vertical_split() -> Scene {
    Scene::new(Size::new(40, 9))
        .named("dev")
        .clients(1)
        .tab("main", true)
        .hint(PrefixHint::new("C-b"))
        .status(StatusMode::Powerline)
        .repository("main", 2)
        .pane(
            ScenePane::new(
                PaneId::new(1),
                1,
                2,
                Size::new(17, 5),
                PaneChrome::new(1, "shell").focused(true),
            )
            .text(&["$ cargo test"]),
        )
        .pane(
            ScenePane::new(
                PaneId::new(2),
                21,
                2,
                Size::new(18, 5),
                PaneChrome::new(2, "claude").attention(Attention::NeedsInput),
            )
            .text(&["apply the patch?"]),
        )
        .attention(2, "claude", Attention::NeedsInput)
}

// ---------------------------------------------------------------------------
// Card 03 — the nested agent workspace
// ---------------------------------------------------------------------------

/// A left column beside a stacked right column, with two live notices over it.
///
/// The notices are the reason this card is composed rather than assembled from
/// helpers: `toast_rows` places them between the two always-on chrome rows and
/// skips the focused pane's cursor row, so the golden is where the client
/// actually put them.
fn nested_workspace() -> Scene {
    Scene::new(Size::new(40, 12))
        .named("dev")
        .clients(2)
        .tab("edit", false)
        .tab("agents", true)
        .hint(PrefixHint::new("C-b"))
        .cursor_row(2)
        .pane(
            ScenePane::new(
                PaneId::new(1),
                1,
                2,
                Size::new(17, 8),
                PaneChrome::new(1, "shell").focused(true),
            )
            .text(&["$ cargo test", "running 12 tests"]),
        )
        .pane(
            ScenePane::new(
                PaneId::new(2),
                21,
                2,
                Size::new(18, 3),
                PaneChrome::new(2, "claude").attention(Attention::NeedsInput),
            )
            .text(&["apply the patch?"]),
        )
        .pane(
            ScenePane::new(
                PaneId::new(3),
                21,
                7,
                Size::new(18, 3),
                PaneChrome::new(3, "build").attention(Attention::Failed),
            )
            .text(&["error[E0308]"]),
        )
        .attention(2, "claude", Attention::NeedsInput)
        .attention(3, "build", Attention::Failed)
        .toast(2, "claude", Attention::NeedsInput)
        .toast(3, "build", Attention::Failed)
}

// ---------------------------------------------------------------------------
// Card 07 — the two status compositions
// ---------------------------------------------------------------------------

/// A workspace with every optional status value available, so the two
/// compositions differ only in how they spend one row.
///
/// Both variants read the same caches: the daemon's projected session name,
/// client count, and effective minimum size, this client's own attention queue
/// and prefix, and its bounded local clock and Git answers. Changing the mode
/// therefore changes chrome and nothing else, which is what a pair of goldens
/// over one scene can show and a pair of separately built rows cannot.
fn status_variant(mode: StatusMode, width: u16) -> Scene {
    Scene::new(Size::new(width, 5))
        .named("dev")
        .clients(2)
        .tab("edit", false)
        .tab("build", true)
        .hint(PrefixHint::new("C-b"))
        .status(mode)
        .repository("main", 2)
        .clock("14:38")
        .effective_size(Size::new(132, 38))
        .pane({
            let body = width.saturating_sub(2).max(1);
            let pane = ScenePane::new(
                PaneId::new(1),
                1,
                2,
                Size::new(body, 1),
                PaneChrome::new(1, "shell").focused(true),
            );
            // A floor-width scene has no room for a transcript, and inventing
            // one would only be asserting about the fixture.
            if body >= 12 {
                pane.text(&["$ cargo test"])
            } else {
                pane
            }
        })
        .attention(2, "claude", Attention::NeedsInput)
}

/// The status row of [`status_variant`], which is the whole card.
fn status_row(mode: StatusMode, width: u16, theme: Theme) -> FrameMatrix {
    status_variant(mode, width).frame(theme).only_row(4)
}

// ---------------------------------------------------------------------------
// Card 08 — the active pane resize
// ---------------------------------------------------------------------------

/// Card 02's workspace mid-resize: the divider column is lit for exactly the
/// rows the two framed allocations share, and the label reports the ratio those
/// allocations reconstruct.
fn resizing_split() -> Scene {
    let divider = (1..=7).map(|row| Point::new(19, row)).collect();
    vertical_split().resizing(divider, Direction::Horizontal, 0.47)
}

// ---------------------------------------------------------------------------
// Cards 02, 03, 07, and 08 — the reviewed goldens
// ---------------------------------------------------------------------------

/// Card 02's complete frame: gutter, focus, dimming, and the powerline row.
fn vertical_split_golden() -> ExpectedFrame {
    let body = "│                 │ │                  │";
    let body_keys = "F~~~~~~~~~~~~~~~~~F_fyyyyyyyyyyyyyyyyyyf";
    workspace_styles(ExpectedFrame::new())
        .row(
            " dev >1 main           2 panes  1 client",
            "SSSSSTTTTTTT,,,,,,,,,,,mmmmmmmmmmmmmmmmm",
        )
        .row(
            "╭> 1 shell       ?╮ ╭  2 claude       !╮",
            "FaammBBBBB,,,,,,,mF_fbbkkPPPPPPxxxxxxxHf",
        )
        .row(
            "│$ cargo test     │ │apply the patch?  │",
            "F~~~~~~~~~~~~~~~~~F_fyyyyyyyyyyyyyyyyyyf",
        )
        .row(body, body_keys)
        .row(body, body_keys)
        .row(body, body_keys)
        .row(body, body_keys)
        .row(
            "╰─────────────────╯ ╰──────────────────╯",
            "FFFFFFFFFFFFFFFFFFF_ffffffffffffffffffff",
        )
        .row(
            " NORMAL \u{e0b0} s dev \u{e0b0} >1 main \u{e0b0} git main +2 ",
            "SSSSSSSSQRRRRRRRDUTNUVVVVUG,gggmppppmww,",
        )
}

#[test]
fn the_vertical_split_card_matches_its_truecolor_golden() {
    assert_frame(
        &vertical_split().frame(Theme::storm()),
        &vertical_split_golden(),
        Theme::storm(),
    );
}

#[test]
fn the_vertical_split_card_matches_the_same_golden_in_sixteen_colors() {
    let theme = sixteen_color(ThemeName::Storm);
    assert_frame(
        &vertical_split().frame(theme),
        &vertical_split_golden(),
        theme,
    );
}

/// Dimming has to reduce contrast without erasing meaning, and that is a claim
/// about *pairs* of cells rather than about any one of them.
#[test]
fn the_unfocused_pane_recedes_without_losing_its_waiting_state() {
    let theme = Theme::storm();
    let frame = vertical_split().frame(theme);
    let waiting = frame.cell(38, 1);
    let quiet = frame.cell(26, 1);
    assert_eq!(waiting.ch, '!', "the glyph survives the treatment");
    assert_ne!(
        waiting.fg, quiet.fg,
        "a dimmed amber `needs input` must not become the dimmed grey of a quiet neighbour"
    );

    // The focused pane keeps its accent frame and the neighbour does not, at
    // both colour depths, which is the signal a user without colour still reads
    // from the `>` marker beside it.
    for theme in [theme, sixteen_color(ThemeName::Storm)] {
        let frame = vertical_split().frame(theme);
        assert_eq!(frame.cell(0, 1).fg, theme.color(ThemeToken::Accent));
        assert_ne!(frame.cell(20, 1).fg, theme.color(ThemeToken::Accent));
        assert_eq!(frame.cell(1, 1).ch, '>');
        assert_eq!(frame.cell(21, 1).ch, ' ');
    }
}

/// The no-dim accessibility configuration turns the whole treatment off and
/// leaves focus to the accent frame and the marker.
#[test]
fn the_no_dim_configuration_leaves_focus_to_the_accent_and_the_marker() {
    let theme = Theme::storm();
    let frame = vertical_split().no_dim().frame(theme);
    assert_eq!(
        frame.cell(38, 1).fg,
        theme.color(ThemeToken::Warning),
        "an undimmed waiting pane is the warning colour itself"
    );
    assert_eq!(frame.cell(21, 2).bg, theme.color(ThemeToken::Surface));
    assert_eq!(
        frame.cell(0, 1).fg,
        theme.color(ThemeToken::Accent),
        "focus is still the accent frame"
    );
    assert_ne!(frame.cell(20, 1).fg, theme.color(ThemeToken::Accent));
}

/// Card 03's complete frame, notices included.
fn nested_workspace_golden() -> ExpectedFrame {
    let body = "│                 │ │                  │";
    let body_keys = "F~~~~~~~~~~~~~~~~~F_fyyyyyyyyyyyyyyyyyyf";
    workspace_styles(ExpectedFrame::new())
        .row(
            " dev  1 edit >2 agents             3p 2c",
            "SSSSSmmmmmmmmTTTTTTTTT,,,,,,,,,,,,,mmmmm",
        )
        // The first notice floats in the upper-right safe area, which on a frame
        // this small is the right pane's own top frame row. That is the
        // documented placement, not a collision: a notice may pass in front of a
        // harness, and only the focused pane's cursor row is protected.
        .row(
            "╭> 1 shell       ?╮claude ! needs input╮",
            "FaammBBBBB,,,,,,,mFpppppp,wwwwwwwwwwwwwf",
        )
        .row(
            "│$ cargo test     │ │apply the patch?  │",
            "F~~~~~~~~~~~~~~~~~F_fyyyyyyyyyyyyyyyyyyf",
        )
        .row(
            "│running 12 tests │ │    build x failed│",
            "F~~~~~~~~~~~~~~~~~F_fyyyyppppp,XXXXXXXXf",
        )
        .row(body, body_keys)
        .row(
            "│                 │ ╰──────────────────╯",
            "F~~~~~~~~~~~~~~~~~F_ffffffffffffffffffff",
        )
        .row(
            "│                 │ ╭  3 build x failed╮",
            "F~~~~~~~~~~~~~~~~~F_fbbkkPPPPPxZZZZZZZZf",
        )
        .row(
            "│                 │ │error[E0308]      │",
            "F~~~~~~~~~~~~~~~~~F_fyyyyyyyyyyyyyyyyyyf",
        )
        .row(body, body_keys)
        .row(body, body_keys)
        .row(
            "╰─────────────────╯ ╰──────────────────╯",
            "FFFFFFFFFFFFFFFFFFF_ffffffffffffffffffff",
        )
        .row(
            " s dev   1 edit  >2 agents  1! 1x   C-b ",
            "SSSSSSSmmmmmmmmmUTNUVVVVVVU,Ww,YX,,,ppp,",
        )
}

#[test]
fn the_nested_workspace_card_matches_its_truecolor_golden() {
    assert_frame(
        &nested_workspace().frame(Theme::storm()),
        &nested_workspace_golden(),
        Theme::storm(),
    );
}

#[test]
fn the_nested_workspace_card_matches_the_same_golden_in_sixteen_colors() {
    let theme = sixteen_color(ThemeName::Storm);
    assert_frame(
        &nested_workspace().frame(theme),
        &nested_workspace_golden(),
        theme,
    );
}

/// The notice stack is bounded by the frame as well as by its own capacity, and
/// it never takes a row the chrome owns.
#[test]
fn the_notice_stack_stays_inside_the_frame_and_off_the_cursor_row() {
    let theme = Theme::storm();
    let frame = nested_workspace().frame(theme);
    // Row 2 is the declared cursor row, so the second notice landed on row 3.
    assert!(
        !frame.text_row(2).contains("build"),
        "{:?}",
        frame.text_row(2)
    );
    assert!(frame.text_row(3).contains("build x failed"));
    // Neither always-on chrome row was taken.
    assert!(frame.text_row(0).contains("agents"));
    assert!(frame.text_row(11).contains("C-b"));
}

/// The two status compositions, at the reference width and at the narrow one.
///
/// Read as a table this is the whole card: the same scene, the same caches, one
/// configuration key, and two rows that differ in composition rather than in
/// what they claim.
const STATUS_LADDER: [(StatusMode, u16, &str, &str); 4] = [
    (
        StatusMode::Minimal,
        72,
        " s dev   1 edit  >2 build  1!        git main +2  2 clients  C-b  14:38 ",
        "SSSSSSSmmmmmmmmmUTNUVVVVVU,Ww,,,,,,,,gggmppppmww,mmmmmmmmmmm,ppp,ccccccc",
    ),
    (
        StatusMode::Powerline,
        72,
        " NORMAL \u{e0b0} s dev \u{e0b0} >2 build \u{e0b0} git main +2         2 clients · min 132x38 ",
        "SSSSSSSSQRRRRRRRDUTNUVVVVVUG,gggmppppmww,,,,,,,,KKKKKKKKKKKKKKKKKKKKKKKK",
    ),
    (
        StatusMode::Minimal,
        40,
        " s dev   1 edit  >2 build  1!       C-b ",
        "SSSSSSSmmmmmmmmmUTNUVVVVVU,Ww,,,,,,,ppp,",
    ),
    (
        StatusMode::Powerline,
        40,
        " NORMAL \u{e0b0} s dev \u{e0b0} >2 build \u{e0b0} git +2     ",
        "SSSSSSSSQRRRRRRRDUTNUVVVVVUG,gggwww,,,,,",
    ),
];

#[test]
fn both_status_variants_match_their_truecolor_and_sixteen_color_goldens() {
    for theme in [Theme::storm(), sixteen_color(ThemeName::Storm)] {
        for (mode, width, text, keys) in STATUS_LADDER {
            assert_frame(
                &status_row(mode, width, theme),
                &workspace_styles(ExpectedFrame::new()).row(text, keys),
                theme,
            );
        }
    }
}

/// Below the narrow forms above, both compositions reach an ASCII floor that
/// still names session, tab, attention or repository, and the prefix.
#[test]
fn both_status_variants_reach_a_documented_ascii_floor() {
    let theme = Theme::storm();
    let text = |mode, width| {
        let row = status_row(mode, width, theme);
        (0..row.size().cols)
            .map(|col| row.cell(col, 0).ch)
            .collect::<String>()
    };
    assert_eq!(text(StatusMode::Minimal, 4), "s>!b");
    assert_eq!(text(StatusMode::Powerline, 4), "Ns>g");
    for width in [4, 12, 20, 40, 72] {
        assert!(
            text(StatusMode::Minimal, width).is_ascii(),
            "the default composition never needs a font: {:?}",
            text(StatusMode::Minimal, width)
        );
    }
    // Powerline's only non-ASCII cell is the separator the preference opted
    // into, and it is the first thing to go: the floor is four ASCII markers.
    assert!(text(StatusMode::Powerline, 20).contains('\u{e0b0}'));
    assert!(
        text(StatusMode::Powerline, 4).is_ascii(),
        "the floor keeps the fields and drops the glyph"
    );
}

/// Card 08's complete frame: the same split, mid-resize.
fn resizing_split_golden() -> ExpectedFrame {
    let body = "│                 │││                  │";
    let body_keys = "F~~~~~~~~~~~~~~~~~FLfyyyyyyyyyyyyyyyyyyf";
    workspace_styles(ExpectedFrame::new())
        .row(
            " dev >1 main           2 panes  1 client",
            "SSSSSTTTTTTT,,,,,,,,,,,mmmmmmmmmmmmmmmmm",
        )
        .row(
            "╭> 1 shell       ?╮│╭  2 claude       !╮",
            "FaammBBBBB,,,,,,,mFLfbbkkPPPPPPxxxxxxxHf",
        )
        .row(
            "│$ cargo test     │││apply the patch?  │",
            "F~~~~~~~~~~~~~~~~~FLfyyyyyyyyyyyyyyyyyyf",
        )
        .row(body, body_keys)
        .row(body, body_keys)
        .row(body, body_keys)
        .row(body, body_keys)
        .row(
            "╰─────────────────╯│╰──────────────────╯",
            "FFFFFFFFFFFFFFFFFFFLffffffffffffffffffff",
        )
        .row(
            " NORMAL \u{e0b0} s dev \u{e0b0} >1 resize · ratio 0.47",
            "SSSSSSSSQRRRRRRRDUTNUmmmmmmmmmmmmmmmBBBB",
        )
}

#[test]
fn the_active_resize_card_matches_its_truecolor_golden() {
    assert_frame(
        &resizing_split().frame(Theme::storm()),
        &resizing_split_golden(),
        Theme::storm(),
    );
}

#[test]
fn the_active_resize_card_matches_the_same_golden_in_sixteen_colors() {
    let theme = sixteen_color(ThemeName::Storm);
    assert_frame(
        &resizing_split().frame(theme),
        &resizing_split_golden(),
        theme,
    );
}

/// The affordance is additive: clearing it restores card 02 exactly, so nothing
/// about a resize can leave a mark once the gesture ends.
#[test]
fn clearing_the_resize_affordance_restores_the_untouched_split() {
    for theme in [Theme::storm(), sixteen_color(ThemeName::Storm)] {
        assert_eq!(
            vertical_split().frame(theme),
            vertical_split().frame(theme),
            "the scene itself is deterministic"
        );
        assert_ne!(resizing_split().frame(theme), vertical_split().frame(theme));
    }
    assert_frame(
        &vertical_split().frame(Theme::storm()),
        &vertical_split_golden(),
        Theme::storm(),
    );
}

/// A terminal too narrow for the label keeps its *ratio* rather than its
/// prefix, and the lit divider is unaffected by that yield: the number is the
/// thing the affordance exists to report.
#[test]
fn the_resize_label_truncates_from_its_head_on_a_narrow_frame() {
    let theme = Theme::storm();
    let narrow = Scene::new(Size::new(12, 4))
        .tab("main", true)
        .pane(
            ScenePane::new(
                PaneId::new(1),
                0,
                1,
                Size::new(12, 1),
                PaneChrome::new(1, "sh").focused(true),
            )
            .headerless(),
        )
        .resizing(vec![Point::new(5, 1)], Direction::Horizontal, 0.62)
        .frame(theme);
    assert_eq!(narrow.text_row(3), "· ratio 0.62");
    assert_eq!(narrow.cell(5, 1).ch, '│');
    assert_eq!(narrow.cell(5, 1).fg, theme.color(ThemeToken::Accent));
    assert!(narrow.cell(5, 1).attrs.contains(CellAttrs::BOLD));
}

// ---------------------------------------------------------------------------
// The shared overlay legend
// ---------------------------------------------------------------------------

/// The roles every overlay surface draws over the raised box.
///
/// The surfaces share one rendering model, so they share one legend: a selected
/// row's marker is the same accent on every card, and a hint row is the same
/// muted text. A card that needs more — card 06's swatches and its live preview
/// panes — extends this rather than replacing it.
fn overlay_styles(frame: ExpectedFrame) -> ExpectedFrame {
    let raised = Paint::Token(ThemeToken::RaisedSurface);
    frame
        .style(
            'r',
            SemanticStyle::new(Paint::Token(ThemeToken::Border), raised),
        )
        .style(
            'A',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), raised).attrs(CellAttrs::BOLD),
        )
        .style(
            'e',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), raised),
        )
        .style(
            'm',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), raised),
        )
        .style(
            'd',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), raised),
        )
        .style(
            'v',
            SemanticStyle::new(Paint::Token(ThemeToken::Primary), raised),
        )
        .style(
            'V',
            SemanticStyle::new(Paint::Token(ThemeToken::Primary), raised).attrs(CellAttrs::BOLD),
        )
        .style(
            's',
            SemanticStyle::new(Paint::Token(ThemeToken::Success), raised),
        )
}

// ---------------------------------------------------------------------------
// Card 04 — the searchable prefix palette
// ---------------------------------------------------------------------------

/// The palette after typing `spl`, which is the card's own illustration.
fn command_palette() -> Overlay {
    let mut overlay = Overlay::palette(&Keymap::defaults());
    for ch in "spl".chars() {
        overlay.apply_palette(PaletteAction::Insert(ch));
    }
    overlay
}

fn overlay_frame(overlay: &Overlay, size: Size, theme: Theme) -> FrameMatrix {
    FrameMatrix::capture(size, &overlay_spans(Point::new(0, 0), overlay, size, theme))
}

/// The palette's complete surface: title, query line, results, hints.
///
/// The chord column is accented *and* bold on every row, which is the card's
/// one non-negotiable: the thing a user opened this surface to find may not rest
/// on colour. The rest of a row spends its width in the shared overlay order.
fn command_palette_golden() -> ExpectedFrame {
    overlay_styles(ExpectedFrame::new())
        .row(
            "  commands - prefix C-b 1/2             ",
            "rrAAAAAAAAAAAAAAAAAAAAAmmmmddddddddddddd",
        )
        .row(
            "  / spl_                                ",
            "rrmmvvvvdddddddddddddddddddddddddddddddd",
        )
        .row(
            "> % split right split-vertical          ",
            "AAAmeeeeeeeeeeemmmmmmmmmmmmmmmdddddddddd",
        )
        .row(
            "  \" split down split-horizontal         ",
            "rrAmvvvvvvvvvvmmmmmmmmmmmmmmmmmddddddddd",
        )
        .row(
            "  esc close enter run up/down move      ",
            "rrmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmdddddd",
        )
}

#[test]
fn command_palette_card_matches_truecolor_and_sixteen_color_goldens() {
    let overlay = command_palette();
    let size = Size::new(40, 5);
    for theme in [Theme::storm(), sixteen_color(ThemeName::Storm)] {
        assert_frame(
            &overlay_frame(&overlay, size, theme),
            &command_palette_golden(),
            theme,
        );
    }
}

/// The palette's own departures from the shared overlay vocabulary, each of
/// which the card asks for: a live query, a `no matches` position claim rather
/// than a blank box, and a hint row that says how navigation moved.
#[test]
fn command_palette_card_reports_its_query_state_rather_than_going_blank() {
    let theme = Theme::storm();
    let size = Size::new(40, 5);
    let text = |overlay: &Overlay, row| overlay_frame(overlay, size, theme).text_row(row);

    let empty = Overlay::palette(&Keymap::defaults());
    assert!(text(&empty, 0).starts_with("  commands - prefix C-b 1/"));
    assert_eq!(text(&empty, 1).trim_end(), "  / _");

    let mut nothing = command_palette();
    for ch in "zzz".chars() {
        nothing.apply_palette(PaletteAction::Insert(ch));
    }
    assert_eq!(
        text(&nothing, 0).trim_end(),
        "  commands - prefix C-b no matches"
    );

    // Navigation is the arrows here, because `j` is query text; the hint row is
    // where that departure is stated.
    assert!(text(&empty, 4).contains("up/down move"));
    assert!(!text(&empty, 4).contains("j/k move"));
}

/// The narrow ladder: the query line yields before the title and the hint row,
/// so a palette too short for one still says what it is and how to leave it.
#[test]
fn command_palette_card_keeps_its_title_and_dismissal_on_a_narrow_surface() {
    let theme = Theme::storm();
    let overlay = command_palette();

    let narrow = overlay_frame(&overlay, Size::new(18, 4), theme);
    assert_eq!(narrow.text_row(0), "  commands - prefi");
    assert_eq!(narrow.text_row(1), "  / spl_          ");
    assert_eq!(narrow.text_row(3), "  esc close       ");

    let short = overlay_frame(&overlay, Size::new(24, 3), theme);
    assert_eq!(short.text_row(0).trim_end(), "  commands - prefix C-b");
    assert!(
        short.text_row(2).contains("esc close"),
        "dismissal is the last hint standing: {:?}",
        short.text_row(2)
    );
}

// ---------------------------------------------------------------------------
// Card 05 — the real session switcher
// ---------------------------------------------------------------------------

fn session_switcher() -> Overlay {
    let entry = |socket: &str, name: &str, tabs, panes, clients| {
        SessionEntry::new(
            PathBuf::from(socket),
            SessionSummary {
                name: name.to_owned(),
                tabs,
                panes,
                clients,
                uptime_secs: 12,
            },
        )
    };
    Overlay::sessions(vec![
        entry("/run/cloo/main.sock", "main", 2, 3, 1).attached(true),
        entry("/run/cloo/review.sock", "review", 1, 1, 0),
    ])
}

fn session_switcher_golden() -> ExpectedFrame {
    let raised = Paint::Token(ThemeToken::RaisedSurface);
    let mut selected = "AAAAAAmSSSSSSSS".to_owned();
    selected.push_str(&"m".repeat(24));
    selected.push('d');
    ExpectedFrame::new()
        .style(
            'A',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), raised).attrs(CellAttrs::BOLD),
        )
        .style(
            'P',
            SemanticStyle::new(Paint::Token(ThemeToken::Primary), raised).attrs(CellAttrs::BOLD),
        )
        .style(
            'S',
            SemanticStyle::new(Paint::Token(ThemeToken::Success), raised),
        )
        .style(
            'm',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), raised),
        )
        .style(
            'r',
            SemanticStyle::new(Paint::Token(ThemeToken::Border), raised),
        )
        .style(
            'd',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), raised),
        )
        .row(
            "  sessions 1/2                          ",
            "rrAAAAAAAAmmmmdddddddddddddddddddddddddd",
        )
        .row("> main attached 2 tabs 3 panes 1 client ", &selected)
        .row(
            "  review 1 tab 1 pane 0 clients         ",
            "rrPPPPPPmmmmmmmmmmmmmmmmmmmmmmmddddddddd",
        )
        .row(
            "  esc close enter switch j/k move       ",
            "rrmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmddddddd",
        )
}

#[test]
fn session_switcher_card_matches_truecolor_and_sixteen_color_goldens() {
    let overlay = session_switcher();
    let size = Size::new(40, 4);
    for theme in [Theme::storm(), sixteen_color(ThemeName::Storm)] {
        let frame = FrameMatrix::capture(
            size,
            &overlay_spans(Point::new(0, 0), &overlay, size, theme),
        );
        assert_frame(&frame, &session_switcher_golden(), theme);
    }
}

#[test]
fn session_switcher_card_has_legible_empty_and_narrow_frames() {
    let theme = Theme::storm();
    let empty = Overlay::sessions(Vec::new());
    let empty = FrameMatrix::capture(
        Size::new(30, 3),
        &overlay_spans(Point::new(0, 0), &empty, Size::new(30, 3), theme),
    );
    assert_eq!(empty.text_row(0).trim_end(), "  sessions");
    assert!(empty.text_row(1).trim().is_empty());
    assert!(empty.text_row(2).contains("esc close"));

    let narrow = FrameMatrix::capture(
        Size::new(12, 3),
        &overlay_spans(
            Point::new(0, 0),
            &session_switcher(),
            Size::new(12, 3),
            theme,
        ),
    );
    assert_eq!(narrow.text_row(0), "  sessions  ");
    assert_eq!(narrow.text_row(1), "> main      ");
    assert_eq!(narrow.text_row(2), "  esc close ");
}

// ---------------------------------------------------------------------------
// Card 06 — the runtime configuration and theme preview
// ---------------------------------------------------------------------------

/// The card's reference geometry: the full surface, list, swatches, and preview.
const CONFIG_SIZE: Size = Size::new(40, 17);

fn config_surface(visual: VisualConfig, caps: TermCaps) -> Overlay {
    Overlay::config(ConfigPreview::new(visual, "C-b", caps))
}

fn config_frame(overlay: &Overlay, size: Size, theme: Theme) -> FrameMatrix {
    FrameMatrix::capture(size, &overlay_spans(Point::new(0, 0), overlay, size, theme))
}

/// Every row of the card, as text. Composition, not colour: the same picture is
/// expected of a truecolor client and its 16-color resolution.
fn config_card_rows() -> Vec<String> {
    let mut rows: Vec<String> = [
        "  configuration 1/11",
        "> theme  storm",
        "  focus  dim unfocused",
        "  status minimal",
        "  motion on",
        "  reduce off",
        "  keys   C-b",
        "  themes",
        "  * storm   # # # # #",
        "    night   # # # # #",
        "    gruvbox # # # # #",
        "    nord    # # # # #",
        "  preview",
    ]
    .iter()
    .map(|row| (*row).to_owned())
    .collect();
    rows.push("\u{256d}> 1 focused     -\u{256e} \u{256d}  2 unfocused    -\u{256e}".to_owned());
    rows.push("\u{2502}$ cloo           \u{2502} \u{2502}$ cloo            \u{2502}".to_owned());
    rows.push(format!(
        "\u{2570}{}\u{256f} \u{2570}{}\u{256f}",
        "\u{2500}".repeat(17),
        "\u{2500}".repeat(18)
    ));
    rows.push("  esc close read only j/k move".to_owned());
    rows
}

/// Card 06's legend: the shared overlay roles, twenty swatch chips, and the
/// live preview pair's own workspace roles.
///
/// The chips are the reason this card has a golden of its own. Each is a
/// [`Paint::Swatch`] naming the theme it previews and the semantic role it
/// stands for, so the truecolor expectation says "night's warning" where the
/// 16-colour resolution of the *same* expectation says "the one bright-yellow
/// every theme collapses to". The preview block below them names the ordinary
/// focused and dimmed pane roles, because it is drawn by the production frame
/// helpers rather than by a second mock renderer.
fn config_card_styles(frame: ExpectedFrame) -> ExpectedFrame {
    let raised = Paint::Token(ThemeToken::RaisedSurface);
    let surface = Paint::Token(ThemeToken::Surface);
    let chips = [
        ThemeToken::Accent,
        ThemeToken::Info,
        ThemeToken::Success,
        ThemeToken::Warning,
        ThemeToken::Error,
    ];
    // One key per (theme, role): four rows of five, in the documented order.
    let keys = [
        ['1', '2', '3', '4', '5'],
        ['6', '7', '8', '9', '0'],
        ['!', '@', '#', '$', '%'],
        ['^', '&', '*', '(', ')'],
    ];
    let mut frame = overlay_styles(frame);
    for (name, row) in ThemeName::ALL.into_iter().zip(keys) {
        for (token, key) in chips.into_iter().zip(row) {
            frame = frame.style(key, SemanticStyle::new(Paint::Swatch(name, token), raised));
        }
    }
    frame
        // -- the live preview pair, drawn by the real frame helpers ---------
        .style(
            'F',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Accent),
                Paint::Token(ThemeToken::Frame),
            ),
        )
        .style(
            'b',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), surface),
        )
        .style(
            'B',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), surface).attrs(CellAttrs::BOLD),
        )
        .style(
            'u',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), surface),
        )
        .style(',', SemanticStyle::new(Paint::Terminal, surface))
        .style(
            '~',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), surface),
        )
        .style(
            'f',
            SemanticStyle::new(
                Paint::Token(ThemeToken::Border),
                Paint::Token(ThemeToken::Frame),
            )
            .dimmed(),
        )
        .style(
            'g',
            SemanticStyle::new(Paint::Token(ThemeToken::Border), surface).dimmed(),
        )
        .style(
            'h',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), surface).dimmed(),
        )
        .style(
            'i',
            SemanticStyle::new(Paint::Token(ThemeToken::Primary), surface).dimmed(),
        )
        .style('j', SemanticStyle::new(Paint::Terminal, surface).dimmed())
        .style(
            'l',
            SemanticStyle::new(Paint::Token(ThemeToken::DefaultText), surface).dimmed(),
        )
}

fn config_text(frame: &FrameMatrix) -> Vec<String> {
    (0..frame.size().rows)
        .map(|row| frame.text_row(row).trim_end().to_owned())
        .collect()
}

/// Card 06's complete surface, role by role.
///
/// The four swatch rows are the point: each chip names the theme it previews and
/// the semantic role it stands for, so this one expectation says "night's
/// warning at its own value" against a truecolor client and "the single bright
/// yellow every theme collapses to" against a 16-colour one.
fn config_preview_golden() -> ExpectedFrame {
    config_card_styles(ExpectedFrame::new())
        .row(
            "  configuration 1/11                    ",
            "rrAAAAAAAAAAAAAmmmmmdddddddddddddddddddd",
        )
        .row(
            "> theme  storm                          ",
            "AAmmmmmmmeeeeedddddddddddddddddddddddddd",
        )
        .row(
            "  focus  dim unfocused                  ",
            "rrmmmmmmmvvvvvvvvvvvvvdddddddddddddddddd",
        )
        .row(
            "  status minimal                        ",
            "rrmmmmmmmvvvvvvvdddddddddddddddddddddddd",
        )
        .row(
            "  motion on                             ",
            "rrmmmmmmmvvddddddddddddddddddddddddddddd",
        )
        .row(
            "  reduce off                            ",
            "rrmmmmmmmvvvdddddddddddddddddddddddddddd",
        )
        .row(
            "  keys   C-b                            ",
            "rrmmmmmmmvvvdddddddddddddddddddddddddddd",
        )
        .row(
            "  themes                                ",
            "rrmmmmmmdddddddddddddddddddddddddddddddd",
        )
        .row(
            "  * storm   # # # # #                   ",
            "rrsmVVVVVVVm1m2m3m4m5ddddddddddddddddddd",
        )
        .row(
            "    night   # # # # #                   ",
            "rrmmVVVVVVVm6m7m8m9m0ddddddddddddddddddd",
        )
        .row(
            "    gruvbox # # # # #                   ",
            "rrmmVVVVVVVm!m@m#m$m%ddddddddddddddddddd",
        )
        .row(
            "    nord    # # # # #                   ",
            "rrmmVVVVVVVm^m&m*m(m)ddddddddddddddddddd",
        )
        .row(
            "  preview                               ",
            "rrmmmmmmmddddddddddddddddddddddddddddddd",
        )
        .row(
            "╭> 1 focused     -╮ ╭  2 unfocused    -╮",
            "FbbuuBBBBBBB,,,,,uFdfgghhiiiiiiiiijjjjhf",
        )
        .row(
            "│$ cloo           │ │$ cloo            │",
            "F~~~~~~~~~~~~~~~~~Fdfllllllllllllllllllf",
        )
        .row(
            "╰─────────────────╯ ╰──────────────────╯",
            "FFFFFFFFFFFFFFFFFFFdffffffffffffffffffff",
        )
        .row(
            "  esc close read only j/k move          ",
            "rrmmmmmmmmmmmmmmmmmmmmmmmmmmmmdddddddddd",
        )
}

#[test]
fn config_preview_card_matches_its_reviewed_cell_golden_at_both_colour_depths() {
    let truecolor = TermCaps {
        truecolor: true,
        ..TermCaps::default()
    };
    for (theme, caps) in [
        (Theme::storm(), truecolor),
        (sixteen_color(ThemeName::Storm), TermCaps::default()),
    ] {
        let overlay = config_surface(VisualConfig::defaults(), caps);
        assert_frame(
            &config_frame(&overlay, CONFIG_SIZE, theme),
            &config_preview_golden(),
            theme,
        );
    }
}

/// The reference truecolor capture: the whole surface, its swatch sets at their
/// own named values, an accent-bordered focused preview pane, and a dimmed
/// neighbour that is neither accent nor the plain border colour.
#[test]
fn config_preview_card_matches_its_truecolor_composition() {
    let theme = Theme::storm();
    let caps = TermCaps {
        truecolor: true,
        ..TermCaps::default()
    };
    let overlay = config_surface(VisualConfig::defaults(), caps);
    let frame = config_frame(&overlay, CONFIG_SIZE, theme);
    assert_eq!(config_text(&frame), config_card_rows());

    // Swatch chips carry the colours of the theme they name, not the client's.
    for (offset, name) in ThemeName::ALL.into_iter().enumerate() {
        let swatch = Theme::named(name, caps);
        let row = 8 + u16::try_from(offset).expect("four themes");
        for (chip, token) in [
            ThemeToken::Accent,
            ThemeToken::Info,
            ThemeToken::Success,
            ThemeToken::Warning,
            ThemeToken::Error,
        ]
        .into_iter()
        .enumerate()
        {
            let col = 12 + u16::try_from(chip * 2).expect("five chips");
            let cell = frame.cell(col, row);
            assert_eq!(cell.ch, '#', "{name} chip {chip}");
            assert_eq!(cell.fg, swatch.color(token), "{name} {token:?}");
            assert!(
                matches!(cell.fg, Color::Rgb(_, _, _)),
                "a truecolor swatch keeps its named RGB"
            );
        }
    }

    // Focus is carried by the border, and the neighbour recedes toward the
    // frame rather than merely losing its label.
    let focused_corner = frame.cell(0, 13);
    let dimmed_corner = frame.cell(20, 13);
    assert_eq!(focused_corner.fg, theme.color(ThemeToken::Accent));
    assert_ne!(dimmed_corner.fg, theme.color(ThemeToken::Accent));
    assert_ne!(
        dimmed_corner.fg,
        theme.color(ThemeToken::Border),
        "the unfocused preview pane is dimmed, not merely unaccented"
    );

    // The active theme is marked in the lead column, where no width takes it.
    assert_eq!(frame.cell(2, 8).ch, '*');
    assert_eq!(frame.cell(2, 8).fg, theme.color(ThemeToken::Success));
}

/// The same composition on a terminal that never negotiated true colour, where
/// every chrome colour is an explicit 16-colour answer and the four swatch sets
/// deliberately collapse onto the shared semantic one.
#[test]
fn config_preview_card_resolves_to_sixteen_colors_without_guessing() {
    let theme = sixteen_color(ThemeName::Storm);
    let overlay = config_surface(VisualConfig::defaults(), TermCaps::default());
    let frame = config_frame(&overlay, CONFIG_SIZE, theme);
    assert_eq!(config_text(&frame), config_card_rows());

    for row in 0..CONFIG_SIZE.rows {
        for col in 0..CONFIG_SIZE.cols {
            let cell = frame.cell(col, row);
            for color in [cell.fg, cell.bg] {
                // `Color::Default` is the outer terminal's own, which chrome
                // padding legitimately keeps; anything beyond index 15 would be
                // a 256-colour guess this client never negotiated.
                assert!(
                    matches!(color, Color::Default)
                        || matches!(color, Color::Indexed(index) if index < 16),
                    "row {row} col {col} must not fall through to a 256-colour guess: {color:?}"
                );
            }
        }
    }

    let chips = |row: u16| {
        (0..5)
            .map(|chip| frame.cell(12 + chip * 2, row).fg)
            .collect::<Vec<_>>()
    };
    for row in 9..12 {
        assert_eq!(
            chips(row),
            chips(8),
            "16 colours give every theme the same semantic swatches"
        );
    }
    assert_eq!(
        frame.cell(2, 8).ch,
        '*',
        "the marker still names the active theme"
    );
}

/// The palette-inheriting choice keeps the user's own foreground and background
/// throughout the surface, takes the terminal's own DIM rendition rather than
/// blending toward a frame colour it cannot know, and still previews the named
/// themes at their real values.
#[test]
fn config_preview_card_inherits_the_outer_terminal_palette() {
    let caps = TermCaps {
        truecolor: true,
        ..TermCaps::default()
    };
    let visual = VisualConfig {
        theme: ThemeChoice::Terminal,
        ..VisualConfig::defaults()
    };
    let frame = config_frame(
        &config_surface(visual, caps),
        CONFIG_SIZE,
        Theme::terminal(),
    );

    let mut expected = config_card_rows();
    expected[1] = "> theme  terminal".to_owned();
    expected[8] = "    storm   # # # # #".to_owned();
    assert_eq!(config_text(&frame), expected);

    assert_eq!(frame.cell(39, 0).bg, Color::Default);
    assert_eq!(frame.cell(1, 14).fg, Color::Default);
    assert!(
        frame.cell(20, 13).attrs.contains(CellAttrs::DIM),
        "with no frame colour to blend toward, dimming is the terminal's own"
    );
    for (offset, name) in ThemeName::ALL.into_iter().enumerate() {
        let row = 8 + u16::try_from(offset).expect("four themes");
        assert_eq!(
            frame.cell(12, row).fg,
            Theme::named(name, caps).color(ThemeToken::Accent),
            "{name} is still previewed at its own value"
        );
    }
}

/// The narrow ladder: the preview yields to the settings the surface exists to
/// report, and the dismissal hint is the last thing standing.
#[test]
fn config_preview_card_degrades_to_its_settings_on_a_narrow_terminal() {
    let theme = Theme::storm();
    let overlay = config_surface(VisualConfig::defaults(), TermCaps::default());

    let narrow = config_frame(&overlay, Size::new(22, 9), theme);
    assert_eq!(config_text(&narrow)[0], "  configuration 1/11");
    assert_eq!(config_text(&narrow)[1], "> theme  storm");
    assert!(
        config_text(&narrow)[5].starts_with('\u{256d}'),
        "22 columns still hold two framed preview panes"
    );
    assert_eq!(config_text(&narrow)[8], "  esc close read only");

    let cramped = config_frame(&overlay, Size::new(18, 9), theme);
    let rows = config_text(&cramped);
    assert!(
        rows.iter().all(|row| !row.contains('\u{256d}')),
        "a box too narrow for two legible panes spends its rows on settings: {rows:?}"
    );
    assert_eq!(rows[0], "  configuration");
    assert_eq!(rows[6], "  keys   C-b");
    assert_eq!(rows[7], "  themes");
    assert_eq!(rows[8], "  esc close");
}

#[test]
fn composing_and_capturing_a_frame_leaves_the_scene_grids_unchanged() {
    let scene = workspace();
    let before = scene.grids();

    // Every composition path a fixture uses: both capability tiers and a theme
    // that is not the one the chrome helpers author against.
    assert_frame(
        &scene.frame(Theme::storm()),
        &workspace_golden(),
        Theme::storm(),
    );
    let nord = sixteen_color(ThemeName::Nord);
    assert_frame(&scene.frame(nord), &workspace_golden(), nord);

    assert_eq!(
        scene.grids(),
        before,
        "capturing a frame must not write back into the client's grid cache"
    );
}

// ---------------------------------------------------------------------------
// The diff itself
// ---------------------------------------------------------------------------

/// A two-row frame built from spans directly, so the diff fixtures do not
/// depend on any chrome helper's current layout.
fn labelled_frame() -> FrameMatrix {
    let styled = |text: &str, fg: Color, attrs: CellAttrs| -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                ch,
                fg,
                bg: Theme::storm().color(ThemeToken::Surface),
                attrs,
            })
            .collect()
    };
    let spans = [
        Span::new(
            Point::new(0, 0),
            styled(
                "shell",
                Theme::storm().color(ThemeToken::Muted),
                CellAttrs::NONE,
            ),
        ),
        Span::new(
            Point::new(0, 1),
            styled(
                "quiet",
                Theme::storm().color(ThemeToken::Muted),
                CellAttrs::NONE,
            ),
        ),
    ];
    FrameMatrix::capture(Size::new(5, 2), &spans)
}

fn labelled_golden(second_row: &str, first_key: char) -> ExpectedFrame {
    let surface = Paint::Token(ThemeToken::Surface);
    ExpectedFrame::new()
        .style(
            'm',
            SemanticStyle::new(Paint::Token(ThemeToken::Muted), surface),
        )
        .style(
            'A',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), surface).attrs(CellAttrs::BOLD),
        )
        .row("shell", &format!("{first_key}mmmm"))
        .row(second_row, "mmmmm")
}

#[test]
fn a_character_difference_names_its_row_column_and_both_characters() {
    let report = check_frame(
        &labelled_frame(),
        &labelled_golden("quIet", 'm'),
        Theme::storm(),
    )
    .expect_err("a golden with a different character must not match");

    assert!(report.contains("row 1, col 2"), "{report}");
    assert!(
        report.contains("char   expected 'I'   actual 'i'"),
        "{report}"
    );
    assert!(report.contains("expected row 1 |quIet|"), "{report}");
    assert!(report.contains("actual   row 1 |quiet|"), "{report}");

    // The caret has to point at the cell the report named, or a wide frame's
    // diff sends a reader to the wrong column.
    let lines: Vec<&str> = report.lines().collect();
    let drawn = lines
        .iter()
        .position(|line| line.contains("actual   row 1 |"))
        .expect("the report draws the actual row");
    let opening = lines[drawn].find('|').expect("the row is delimited");
    assert_eq!(
        lines[drawn + 1].find('^'),
        Some(opening + 1 + 2),
        "{report}"
    );
}

#[test]
fn a_style_only_difference_names_both_semantic_roles() {
    let report = check_frame(
        &labelled_frame(),
        &labelled_golden("quiet", 'A'),
        Theme::storm(),
    )
    .expect_err("a golden with a different style must not match");

    assert!(report.contains("row 0, col 0"), "{report}");
    assert!(
        report.contains("char   expected 's'   actual 's'"),
        "{report}"
    );
    assert!(report.contains("fg     expected Accent"), "{report}");
    assert!(report.contains("actual Muted"), "{report}");
    assert!(
        report.contains("attrs  expected bold   actual none"),
        "{report}"
    );
}

#[test]
fn a_frame_of_the_wrong_geometry_is_refused_before_any_cell_is_compared() {
    let report = check_frame(
        &FrameMatrix::capture(Size::new(4, 2), &[]),
        &labelled_golden("quiet", 'm'),
        Theme::storm(),
    )
    .expect_err("a golden of a different size must not match");

    assert!(report.contains("expected 5x2"), "{report}");
    assert!(report.contains("drew 4x2"), "{report}");
}

// ---------------------------------------------------------------------------
// Border style prototype
// ---------------------------------------------------------------------------

/// Every cell of a scene composed under one border style.
fn bordered(scene: &Scene, style: BorderStyle, theme: Theme) -> FrameMatrix {
    scene.clone().borders(style).frame(theme)
}

/// A border style changes characters in frame cells and nothing else.
///
/// This is the whole claim the preference rests on: the three styles are one
/// composition, so a frame's geometry, colours, renditions, and therefore its
/// mouse hit-testing are identical across all of them. Only a corner or edge
/// cell's `ch` may differ, and only into that style's own glyph set.
#[test]
fn a_border_style_changes_only_frame_characters() {
    let theme = Theme::storm();
    let scene = workspace();
    let square = bordered(&scene, BorderStyle::Square, theme);
    let size = square.size();

    for style in [BorderStyle::Rounded, BorderStyle::Ascii] {
        let other = bordered(&scene, style, theme);
        assert_eq!(other.size(), size, "{style} resized the frame");

        let square_glyphs: Vec<char> = BorderStyle::Square
            .corners()
            .into_iter()
            .chain([
                BorderStyle::Square.horizontal(),
                BorderStyle::Square.vertical(),
            ])
            .collect();

        for row in 0..size.rows {
            for col in 0..size.cols {
                let a = square.cell(col, row);
                let b = other.cell(col, row);
                assert_eq!(a.fg, b.fg, "{style} changed a colour at {col},{row}");
                assert_eq!(a.bg, b.bg, "{style} changed a colour at {col},{row}");
                assert_eq!(
                    a.attrs, b.attrs,
                    "{style} changed a rendition at {col},{row}"
                );
                if a.ch != b.ch {
                    assert!(
                        square_glyphs.contains(&a.ch),
                        "{style} changed a non-frame cell at {col},{row}: {:?} -> {:?}",
                        a.ch,
                        b.ch
                    );
                }
            }
        }
    }
}

/// Prints card 01 under all three styles, for review against the handoff.
///
/// Run with `cargo test -p cloo-client --test visual border_style_prototype
/// -- --nocapture`. This asserts nothing a reviewer could not see; the
/// assertion that matters is the one above.
#[test]
fn border_style_prototype() {
    let theme = Theme::storm();
    let scene = workspace();
    for style in BorderStyle::ALL {
        let frame = bordered(&scene, style, theme);
        println!("\n=== card 01 · borders = \"{style}\" ===");
        for row in 0..frame.size().rows {
            println!("{}", frame.text_row(row));
        }
    }
}
