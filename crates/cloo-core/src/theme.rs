//! Named theme definitions and their terminal-palette selection.
//!
//! A theme is data, not renderer policy. This crate owns the four named
//! palettes and the choice to inherit the outer terminal's palette; the client
//! owns turning those tokens into SGR colours for its particular capabilities.
//! Keeping that boundary here means configuration can name a theme without
//! making server-owned state depend on an attached terminal.

use core::fmt;

/// A 24-bit palette value, kept independent from the wire's [`Color`] type.
///
/// The wire colour is for pane contents. Chrome tokens are client-local, and
/// the client decides whether an outer terminal receives RGB, ANSI, or its
/// own default palette entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Rgb {
    /// Builds one RGB token.
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// Every semantic colour the style guide assigns to chrome.
///
/// These names deliberately describe a role rather than an appearance: a
/// client can preserve `warning` as a warning on a 16-colour terminal without
/// trying to approximate an RGB value in a palette it does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeTokens {
    /// Space between panes.
    pub frame: Rgb,
    /// Chrome and pane base surface.
    pub surface: Rgb,
    /// Active tabs and overlays.
    pub raised_surface: Rgb,
    /// Frame and unfocused-pane borders.
    pub border: Rgb,
    /// Focus, selection, and active controls.
    pub accent: Rgb,
    /// Labels and important text.
    pub primary: Rgb,
    /// Ordinary terminal-friendly chrome text.
    pub default_text: Rgb,
    /// Secondary text.
    pub muted: Rgb,
    /// Success and ready state.
    pub success: Rgb,
    /// Caution and needs-input state.
    pub warning: Rgb,
    /// Failure state.
    pub error: Rgb,
    /// Informational and working state.
    pub info: Rgb,
}

/// One built-in named theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    /// The reference palette from the approved visual handoff.
    #[default]
    Storm,
    /// A cool, high-contrast dark palette.
    Night,
    /// A warm, low-contrast dark palette.
    Gruvbox,
    /// The blue-grey Nord palette.
    Nord,
}

impl ThemeName {
    /// Every named theme in stable launcher/configuration order.
    pub const ALL: [Self; 4] = [Self::Storm, Self::Night, Self::Gruvbox, Self::Nord];

    /// The stable configuration spelling for this theme.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Storm => "storm",
            Self::Night => "night",
            Self::Gruvbox => "gruvbox",
            Self::Nord => "nord",
        }
    }

    /// Parses one configuration spelling.
    #[must_use]
    pub const fn parse(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"storm" => Some(Self::Storm),
            b"night" => Some(Self::Night),
            b"gruvbox" => Some(Self::Gruvbox),
            b"nord" => Some(Self::Nord),
            _ => None,
        }
    }

    /// This theme's complete style-guide token table.
    #[must_use]
    pub const fn tokens(self) -> ThemeTokens {
        match self {
            Self::Storm => ThemeTokens {
                frame: Rgb::new(0x0f, 0x0f, 0x16),
                surface: Rgb::new(0x1a, 0x1b, 0x26),
                raised_surface: Rgb::new(0x24, 0x28, 0x3b),
                border: Rgb::new(0x2a, 0x2e, 0x42),
                accent: Rgb::new(0xbb, 0x9a, 0xf7),
                primary: Rgb::new(0xc0, 0xca, 0xf5),
                default_text: Rgb::new(0xa9, 0xb1, 0xd6),
                muted: Rgb::new(0x56, 0x5f, 0x89),
                success: Rgb::new(0x9e, 0xce, 0x6a),
                warning: Rgb::new(0xe0, 0xaf, 0x68),
                error: Rgb::new(0xf7, 0x76, 0x8e),
                info: Rgb::new(0x7d, 0xcf, 0xff),
            },
            Self::Night => ThemeTokens {
                frame: Rgb::new(0x01, 0x16, 0x27),
                surface: Rgb::new(0x0b, 0x1c, 0x2c),
                raised_surface: Rgb::new(0x12, 0x25, 0x3b),
                border: Rgb::new(0x1d, 0x3b, 0x53),
                accent: Rgb::new(0x82, 0xaa, 0xff),
                primary: Rgb::new(0xd6, 0xde, 0xeb),
                default_text: Rgb::new(0xc5, 0xd1, 0xeb),
                muted: Rgb::new(0x63, 0x77, 0x77),
                success: Rgb::new(0xc3, 0xe8, 0x8d),
                warning: Rgb::new(0xff, 0xcb, 0x6b),
                error: Rgb::new(0xef, 0x53, 0x50),
                info: Rgb::new(0x7f, 0xdb, 0xca),
            },
            Self::Gruvbox => ThemeTokens {
                frame: Rgb::new(0x28, 0x28, 0x28),
                surface: Rgb::new(0x3c, 0x38, 0x36),
                raised_surface: Rgb::new(0x50, 0x49, 0x45),
                border: Rgb::new(0x66, 0x5c, 0x54),
                accent: Rgb::new(0xd3, 0x86, 0x9b),
                primary: Rgb::new(0xeb, 0xdb, 0xb2),
                default_text: Rgb::new(0xd5, 0xc4, 0xa1),
                muted: Rgb::new(0x92, 0x83, 0x74),
                success: Rgb::new(0xb8, 0xbb, 0x26),
                warning: Rgb::new(0xfa, 0xbd, 0x2f),
                error: Rgb::new(0xfb, 0x49, 0x34),
                info: Rgb::new(0x83, 0xa5, 0x98),
            },
            Self::Nord => ThemeTokens {
                frame: Rgb::new(0x2e, 0x34, 0x40),
                surface: Rgb::new(0x3b, 0x42, 0x52),
                raised_surface: Rgb::new(0x43, 0x4c, 0x5e),
                border: Rgb::new(0x4c, 0x56, 0x6a),
                accent: Rgb::new(0x88, 0xc0, 0xd0),
                primary: Rgb::new(0xec, 0xef, 0xf4),
                default_text: Rgb::new(0xd8, 0xde, 0xe9),
                muted: Rgb::new(0x61, 0x6e, 0x88),
                success: Rgb::new(0xa3, 0xbe, 0x8c),
                warning: Rgb::new(0xeb, 0xcb, 0x8b),
                error: Rgb::new(0xbf, 0x61, 0x6a),
                info: Rgb::new(0x81, 0xa1, 0xc1),
            },
        }
    }
}

impl fmt::Display for ThemeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a client chooses its chrome palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    /// One of cloo's complete named palettes.
    Named(ThemeName),
    /// Reuse the outer terminal's default foreground/background and ANSI
    /// semantic palette rather than imposing a chrome palette of our own.
    Terminal,
}

impl Default for ThemeChoice {
    fn default() -> Self {
        Self::Named(ThemeName::Storm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_theme_has_a_stable_spelling_and_complete_token_table() {
        for theme in ThemeName::ALL {
            assert_eq!(ThemeName::parse(theme.as_str()), Some(theme));
            let tokens = theme.tokens();
            let all = [
                tokens.frame,
                tokens.surface,
                tokens.raised_surface,
                tokens.border,
                tokens.accent,
                tokens.primary,
                tokens.default_text,
                tokens.muted,
                tokens.success,
                tokens.warning,
                tokens.error,
                tokens.info,
            ];
            assert_eq!(all.len(), 12, "{theme} maps every style-guide token");
        }
        assert_eq!(ThemeName::parse("solarized"), None);
    }

    #[test]
    fn storm_is_the_style_guides_reference_palette() {
        assert_eq!(
            ThemeName::Storm.tokens(),
            ThemeTokens {
                frame: Rgb::new(0x0f, 0x0f, 0x16),
                surface: Rgb::new(0x1a, 0x1b, 0x26),
                raised_surface: Rgb::new(0x24, 0x28, 0x3b),
                border: Rgb::new(0x2a, 0x2e, 0x42),
                accent: Rgb::new(0xbb, 0x9a, 0xf7),
                primary: Rgb::new(0xc0, 0xca, 0xf5),
                default_text: Rgb::new(0xa9, 0xb1, 0xd6),
                muted: Rgb::new(0x56, 0x5f, 0x89),
                success: Rgb::new(0x9e, 0xce, 0x6a),
                warning: Rgb::new(0xe0, 0xaf, 0x68),
                error: Rgb::new(0xf7, 0x76, 0x8e),
                info: Rgb::new(0x7d, 0xcf, 0xff),
            }
        );
    }
}
