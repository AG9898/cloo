//! Client-side resolution of semantic theme tokens.
//!
//! `cloo-core` supplies named RGB palettes as data. This module is the last
//! step before rendering: it keeps RGB when the attached terminal explicitly
//! supports true colour, or deliberately chooses ANSI semantic colours when it
//! does not. The `Terminal` choice inherits the user's default foreground and
//! background instead of painting a competing surface.

use cloo_core::{Rgb, ThemeChoice, ThemeName, ThemeTokens};
use cloo_proto::{Cell, Color, TermCaps};

/// A semantic style-guide token that chrome can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeToken {
    /// Space between panes.
    Frame,
    /// Chrome and pane base surface.
    Surface,
    /// Active tabs and overlays.
    RaisedSurface,
    /// Frame and unfocused-pane borders.
    Border,
    /// Focus, selection, and active controls.
    Accent,
    /// Labels and important text.
    Primary,
    /// Ordinary terminal-friendly chrome text.
    DefaultText,
    /// Secondary text.
    Muted,
    /// Success and ready state.
    Success,
    /// Caution and needs-input state.
    Warning,
    /// Failure state.
    Error,
    /// Informational and working state.
    Info,
}

impl ThemeToken {
    /// Every token in the style guide, in stable documentation order.
    pub const ALL: [Self; 12] = [
        Self::Frame,
        Self::Surface,
        Self::RaisedSurface,
        Self::Border,
        Self::Accent,
        Self::Primary,
        Self::DefaultText,
        Self::Muted,
        Self::Success,
        Self::Warning,
        Self::Error,
        Self::Info,
    ];
}

/// Resolved colours for one attached client.
///
/// It is deliberately small and copyable, so pure chrome helpers can accept it
/// by value. A theme never crosses the wire: two clients may render one session
/// with different local palettes without changing server-owned state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    choice: ThemeChoice,
    frame: Color,
    surface: Color,
    raised_surface: Color,
    border: Color,
    accent: Color,
    primary: Color,
    default_text: Color,
    muted: Color,
    success: Color,
    warning: Color,
    error: Color,
    info: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::storm()
    }
}

impl Theme {
    /// The reference Storm palette in true colour.
    #[must_use]
    pub const fn storm() -> Self {
        Self {
            choice: ThemeChoice::Named(ThemeName::Storm),
            frame: Color::Rgb(0x0f, 0x0f, 0x16),
            surface: Color::Rgb(0x1a, 0x1b, 0x26),
            raised_surface: Color::Rgb(0x24, 0x28, 0x3b),
            border: Color::Rgb(0x2a, 0x2e, 0x42),
            accent: Color::Rgb(0xbb, 0x9a, 0xf7),
            primary: Color::Rgb(0xc0, 0xca, 0xf5),
            default_text: Color::Rgb(0xa9, 0xb1, 0xd6),
            muted: Color::Rgb(0x56, 0x5f, 0x89),
            success: Color::Rgb(0x9e, 0xce, 0x6a),
            warning: Color::Rgb(0xe0, 0xaf, 0x68),
            error: Color::Rgb(0xf7, 0x76, 0x8e),
            info: Color::Rgb(0x7d, 0xcf, 0xff),
        }
    }

    /// Resolves a user choice for one outer terminal.
    ///
    /// Named themes use their literal RGB values only when `truecolor` was
    /// negotiated. Otherwise every role has an explicit ANSI answer, rather
    /// than relying on the renderer's 256-colour quantizer. `Terminal` always
    /// uses the default foreground/background plus ANSI semantic colours, so it
    /// inherits a user's terminal palette at every colour depth.
    #[must_use]
    pub fn new(choice: ThemeChoice, caps: TermCaps) -> Self {
        match choice {
            ThemeChoice::Named(ThemeName::Storm) if caps.truecolor => Self::storm(),
            ThemeChoice::Named(name) if caps.truecolor => Self::from_rgb(name.tokens(), choice),
            ThemeChoice::Named(_) => Self::ansi(choice),
            ThemeChoice::Terminal => Self::terminal(),
        }
    }

    /// Resolves one named theme for an outer terminal.
    #[must_use]
    pub fn named(name: ThemeName, caps: TermCaps) -> Self {
        Self::new(ThemeChoice::Named(name), caps)
    }

    /// The outer-terminal palette inheritance mode.
    #[must_use]
    pub const fn terminal() -> Self {
        Self {
            choice: ThemeChoice::Terminal,
            frame: Color::Default,
            surface: Color::Default,
            raised_surface: Color::Default,
            border: Color::Indexed(8),
            accent: Color::Indexed(13),
            primary: Color::Default,
            default_text: Color::Default,
            muted: Color::Indexed(8),
            success: Color::Indexed(10),
            warning: Color::Indexed(11),
            error: Color::Indexed(9),
            info: Color::Indexed(14),
        }
    }

    /// The choice this client resolved.
    #[must_use]
    pub const fn choice(self) -> ThemeChoice {
        self.choice
    }

    /// The resolved colour for one semantic role.
    #[must_use]
    pub const fn color(self, token: ThemeToken) -> Color {
        match token {
            ThemeToken::Frame => self.frame,
            ThemeToken::Surface => self.surface,
            ThemeToken::RaisedSurface => self.raised_surface,
            ThemeToken::Border => self.border,
            ThemeToken::Accent => self.accent,
            ThemeToken::Primary => self.primary,
            ThemeToken::DefaultText => self.default_text,
            ThemeToken::Muted => self.muted,
            ThemeToken::Success => self.success,
            ThemeToken::Warning => self.warning,
            ThemeToken::Error => self.error,
            ThemeToken::Info => self.info,
        }
    }

    /// Recolours Storm-authored chrome cells to this theme.
    ///
    /// Existing pure chrome helpers deliberately construct the reference theme
    /// first. This small boundary maps only those semantic token values, never
    /// pane content, so an application RGB value identical to Storm purple is
    /// not rewritten. It lets older helpers keep their simple token constants
    /// while all themed entry points stay exact.
    #[must_use]
    pub fn map_storm_cell(self, mut cell: Cell) -> Cell {
        cell.fg = self.map_storm_color(cell.fg);
        cell.bg = self.map_storm_color(cell.bg);
        cell
    }

    /// Recolours a chrome row authored with the reference tokens.
    #[must_use]
    pub fn map_storm_cells(self, cells: Vec<Cell>) -> Vec<Cell> {
        cells
            .into_iter()
            .map(|cell| self.map_storm_cell(cell))
            .collect()
    }

    fn from_rgb(tokens: ThemeTokens, choice: ThemeChoice) -> Self {
        Self {
            choice,
            frame: color(tokens.frame),
            surface: color(tokens.surface),
            raised_surface: color(tokens.raised_surface),
            border: color(tokens.border),
            accent: color(tokens.accent),
            primary: color(tokens.primary),
            default_text: color(tokens.default_text),
            muted: color(tokens.muted),
            success: color(tokens.success),
            warning: color(tokens.warning),
            error: color(tokens.error),
            info: color(tokens.info),
        }
    }

    /// Named-theme fallback on a terminal without true colour.
    ///
    /// Bright ANSI slots make semantic states stand apart from the neutral
    /// frame. Focus still has its `>` marker and attention still has its own
    /// ASCII glyph, so this mapping adds distinction instead of carrying it.
    const fn ansi(choice: ThemeChoice) -> Self {
        Self {
            choice,
            frame: Color::Indexed(0),
            surface: Color::Indexed(0),
            raised_surface: Color::Indexed(8),
            border: Color::Indexed(8),
            accent: Color::Indexed(13),
            primary: Color::Indexed(15),
            default_text: Color::Indexed(7),
            muted: Color::Indexed(8),
            success: Color::Indexed(10),
            warning: Color::Indexed(11),
            error: Color::Indexed(9),
            info: Color::Indexed(14),
        }
    }

    fn map_storm_color(self, color: Color) -> Color {
        let storm = Self::storm();
        for token in ThemeToken::ALL {
            if color == storm.color(token) {
                return self.color(token);
            }
        }
        color
    }
}

const fn color(value: Rgb) -> Color {
    Color::Rgb(value.red, value.green, value.blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn truecolor() -> TermCaps {
        TermCaps {
            truecolor: true,
            ..TermCaps::default()
        }
    }

    #[test]
    fn every_named_theme_resolves_every_style_guide_token_deterministically() {
        for name in ThemeName::ALL {
            let theme = Theme::named(name, truecolor());
            for token in ThemeToken::ALL {
                assert!(
                    matches!(theme.color(token), Color::Rgb(_, _, _)),
                    "{name} {token:?} must retain its named RGB token"
                );
            }
        }
    }

    #[test]
    fn no_truecolor_uses_explicit_sixteen_color_semantics() {
        let theme = Theme::named(ThemeName::Nord, TermCaps::default());
        assert_eq!(theme.color(ThemeToken::Accent), Color::Indexed(13));
        assert_eq!(theme.color(ThemeToken::Warning), Color::Indexed(11));
        assert_eq!(theme.color(ThemeToken::Error), Color::Indexed(9));
        assert_eq!(theme.color(ThemeToken::Info), Color::Indexed(14));
        for token in ThemeToken::ALL {
            assert!(
                matches!(theme.color(token), Color::Indexed(index) if index < 16),
                "{token:?} must not fall through to a 256-colour guess"
            );
        }
    }

    #[test]
    fn terminal_choice_inherits_defaults_but_keeps_semantic_signals() {
        let theme = Theme::terminal();
        assert_eq!(theme.color(ThemeToken::Frame), Color::Default);
        assert_eq!(theme.color(ThemeToken::Surface), Color::Default);
        assert_eq!(theme.color(ThemeToken::Primary), Color::Default);
        assert_ne!(
            theme.color(ThemeToken::Accent),
            theme.color(ThemeToken::Warning)
        );
        assert_ne!(
            theme.color(ThemeToken::Warning),
            theme.color(ThemeToken::Error)
        );
    }
}
