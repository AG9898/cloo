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
use cloo_client::renderer::Span;
use cloo_client::theme::{Theme, ThemeToken};
use cloo_core::{ThemeChoice, ThemeName};
use cloo_proto::{Cell, CellAttrs, Color, PaneId, Point, Size, TermCaps};

use harness::{ExpectedFrame, FrameMatrix, Paint, SemanticStyle, assert_frame, check_frame};
use scenes::{Scene, ScenePane};

/// A named theme resolved for a terminal that never negotiated true colour.
fn sixteen_color(name: ThemeName) -> Theme {
    Theme::new(ThemeChoice::Named(name), TermCaps::default())
}

// ---------------------------------------------------------------------------
// Card 01 — the daily one-pane workspace
// ---------------------------------------------------------------------------

/// One tab, one focused pane with a header, and the always-on status row.
fn workspace() -> Scene {
    Scene::new(Size::new(40, 8))
        .session(1)
        .tab("main", true)
        .hint(PrefixHint::new("C-b"))
        .pane(
            ScenePane::new(
                PaneId::new(1),
                0,
                2,
                Size::new(40, 5),
                PaneChrome::new(1, "shell").focused(true),
            )
            .text(&["$ ls", "Cargo.toml  src"]),
        )
}

/// The complete expected frame for [`workspace`], role by role.
///
/// `A`/`.` are the tab row and `P`/`-`/`M`/`p` the status row; both are still
/// authored against the reference palette, so they are expected as
/// [`Paint::Reference`]. The pane header (`a`, `m`, `B`, `s`) already resolves
/// through the client theme, and the pane body (`~`) maps the child's default
/// colours to the selected pane text and surface roles at composition time.
fn workspace_golden() -> ExpectedFrame {
    let reference = |token| Paint::Reference(token);
    let chrome_bg = Paint::Reference(ThemeToken::Surface);
    let pane_bg = Paint::Token(ThemeToken::Surface);

    ExpectedFrame::new()
        .style(
            'A',
            SemanticStyle::new(reference(ThemeToken::Accent), chrome_bg).attrs(CellAttrs::BOLD),
        )
        .style('.', SemanticStyle::new(Paint::Terminal, chrome_bg))
        .style(
            'P',
            SemanticStyle::new(reference(ThemeToken::Primary), chrome_bg).attrs(CellAttrs::BOLD),
        )
        .style(
            'p',
            SemanticStyle::new(reference(ThemeToken::Primary), chrome_bg),
        )
        .style(
            '-',
            SemanticStyle::new(reference(ThemeToken::Muted), chrome_bg),
        )
        .style(
            'M',
            SemanticStyle::new(reference(ThemeToken::Muted), chrome_bg).attrs(CellAttrs::BOLD),
        )
        .style(
            'a',
            SemanticStyle::new(Paint::Token(ThemeToken::Accent), pane_bg),
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
            ">1 main                                 ",
            "AAAAAAA.................................",
        )
        .row(
            "> 1 shell                      ? unknown",
            "aammBBBBBssssssssssssssssssssssmmmmmmmmm",
        )
        .row(
            "$ ls                                    ",
            "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        )
        .row(
            "Cargo.toml  src                         ",
            "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        )
        .row(
            "                                        ",
            "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        )
        .row(
            "                                        ",
            "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        )
        .row(
            "                                        ",
            "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        )
        .row(
            "session:1 >1 main 0! C-b ?              ",
            "PPPPPPPPP-AAAAAAA-MM-ppp-A..............",
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
