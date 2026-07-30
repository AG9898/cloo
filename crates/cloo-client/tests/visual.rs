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

use cloo_client::chrome::{PaneChrome, PrefixHint};
use cloo_client::overlay::{Overlay, SessionEntry, overlay_spans};
use cloo_client::renderer::Span;
use cloo_client::theme::{Theme, ThemeToken};
use cloo_core::{ThemeChoice, ThemeName};
use cloo_proto::{Cell, CellAttrs, Color, PaneId, Point, SessionSummary, Size, TermCaps};
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
            "┌> 1 shell                    ? unknown┐",
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
            "└──────────────────────────────────────┘",
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
