//! Color palette and theme definitions for Mechanic.
//!
//! The default theme is inspired by the Stark Industries aesthetic: a deep black
//! background with electric cyan/blue primary colors and amber/orange accents.

use serde::{Deserialize, Serialize};

// ── Primitive color type ──────────────────────────────────────────────────────

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Construct from individual channel values.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Construct from a 24-bit hex literal (e.g. `0x52E8FF`).
    pub const fn from_hex(hex: u32) -> Self {
        Self { r: ((hex >> 16) & 0xFF) as u8, g: ((hex >> 8) & 0xFF) as u8, b: (hex & 0xFF) as u8 }
    }
}

// ── Stark palette constants ───────────────────────────────────────────────────

/// Named constants for the Stark-Industries-inspired color palette.
pub mod palette {
    use super::Rgb;

    /// Electric cyan — primary foreground / brand accent (`#52E8FF`).
    pub const ELECTRIC: Rgb = Rgb::from_hex(0x52E8FF);
    /// Celeste — bright highlight variant of electric (`#ADFFFF`).
    pub const CELESTE: Rgb = Rgb::from_hex(0xADFFFF);
    /// Azure blue — mid-tone accent (`#007FFF`).
    pub const AZURE: Rgb = Rgb::from_hex(0x007FFF);
    /// Deep blue — dark accent / dim color (`#0015FF`).
    pub const BLUE: Rgb = Rgb::from_hex(0x0015FF);

    /// Pure black background (`#000000`).
    pub const BLACK: Rgb = Rgb::from_hex(0x000000);
    /// Near-black for subtle depth (`#0A0A0A`).
    pub const NEAR_BLACK: Rgb = Rgb::from_hex(0x0A0A0A);
    /// Dim cyan — used for "bright black" / dark-grey slots (`#1A3A40`).
    pub const DIM_CYAN: Rgb = Rgb::from_hex(0x1A3A40);

    /// Amber — folder / highlight warm accent (`#FFB300`).
    pub const AMBER: Rgb = Rgb::from_hex(0xFFB300);
    /// Gold — brighter warm highlight (`#FFD700`).
    pub const GOLD: Rgb = Rgb::from_hex(0xFFD700);

    /// Alert orange-red — warnings / errors (`#FF4500`).
    pub const ALERT: Rgb = Rgb::from_hex(0xFF4500);
    /// Muted red — ANSI red slot (`#CC2200`).
    pub const RED: Rgb = Rgb::from_hex(0xCC2200);

    /// Soft white — used for ANSI white / bright foreground (`#E0F8FF`).
    pub const SOFT_WHITE: Rgb = Rgb::from_hex(0xE0F8FF);
}

// ── ANSI 16-color mapping ─────────────────────────────────────────────────────

/// Mapping for the 16 standard ANSI terminal colors, mapped to Stark palette
/// equivalents.
///
/// Indices follow the traditional terminal convention:
/// 0–7 normal, 8–15 bright.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnsiColors {
    // Normal (0-7)
    /// ANSI 0 — black
    pub black: Rgb,
    /// ANSI 1 — red
    pub red: Rgb,
    /// ANSI 2 — green (rendered as electric cyan in the Stark palette)
    pub green: Rgb,
    /// ANSI 3 — yellow (rendered as amber)
    pub yellow: Rgb,
    /// ANSI 4 — blue (azure)
    pub blue: Rgb,
    /// ANSI 5 — magenta (deep blue, closest Stark analog)
    pub magenta: Rgb,
    /// ANSI 6 — cyan (electric)
    pub cyan: Rgb,
    /// ANSI 7 — white (soft white)
    pub white: Rgb,

    // Bright (8-15)
    /// ANSI 8 — bright black / dark grey
    pub bright_black: Rgb,
    /// ANSI 9 — bright red (alert orange-red)
    pub bright_red: Rgb,
    /// ANSI 10 — bright green (celeste)
    pub bright_green: Rgb,
    /// ANSI 11 — bright yellow (gold)
    pub bright_yellow: Rgb,
    /// ANSI 12 — bright blue (electric)
    pub bright_blue: Rgb,
    /// ANSI 13 — bright magenta (azure)
    pub bright_magenta: Rgb,
    /// ANSI 14 — bright cyan (celeste)
    pub bright_cyan: Rgb,
    /// ANSI 15 — bright white (pure white)
    pub bright_white: Rgb,
}

impl Default for AnsiColors {
    fn default() -> Self {
        use palette::*;
        Self {
            // Normal
            black: BLACK,
            red: RED,
            green: ELECTRIC,
            yellow: AMBER,
            blue: AZURE,
            magenta: BLUE,
            cyan: ELECTRIC,
            white: SOFT_WHITE,
            // Bright
            bright_black: DIM_CYAN,
            bright_red: ALERT,
            bright_green: CELESTE,
            bright_yellow: GOLD,
            bright_blue: ELECTRIC,
            bright_magenta: AZURE,
            bright_cyan: CELESTE,
            bright_white: Rgb::from_hex(0xFFFFFF),
        }
    }
}

// ── Selection ─────────────────────────────────────────────────────────────────

/// Colors used for text selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectionColors {
    /// Background color of the selection highlight.
    pub background: Rgb,
    /// Foreground (text) color while selected; `None` keeps the original glyph color.
    pub foreground: Option<Rgb>,
}

impl Default for SelectionColors {
    fn default() -> Self {
        Self {
            // Electric cyan background with black text — bright and readable,
            // same hue family as the foreground so it stays on-palette.
            background: palette::ELECTRIC,
            foreground: Some(palette::BLACK),
        }
    }
}

// ── Window opacity ────────────────────────────────────────────────────────────

/// Opacity / fade settings for the terminal window.
///
/// All opacity values are in the range `[0.0, 1.0]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpacityConfig {
    /// Opacity of the macOS title bar chrome.
    pub title_bar_opacity: f32,
    /// Opacity of the content area when the window is focused.
    pub content_active_opacity: f32,
    /// Opacity of the content area when the window is in the background.
    pub content_idle_opacity: f32,
    /// Seconds of inactivity before the fade-out animation begins.
    pub fade_begin_secs: u32,
    /// Seconds after which the fade reaches `content_idle_opacity`.
    pub fade_end_secs: u32,
}

impl Default for OpacityConfig {
    fn default() -> Self {
        Self {
            title_bar_opacity: 0.95,
            content_active_opacity: 0.90,
            content_idle_opacity: 0.75,
            fade_begin_secs: 30,
            fade_end_secs: 60,
        }
    }
}

// ── Top-level Theme ───────────────────────────────────────────────────────────

/// Complete theme configuration.
///
/// Deserializing a partial TOML snippet will fill missing keys from `Default`.
/// Example — override only the font size in a user config:
///
/// ```toml
/// [theme.opacity]
/// content_idle_opacity = 0.70
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    /// Default foreground (text) color.
    pub foreground: Rgb,
    /// Default background color.
    pub background: Rgb,
    /// Cursor color (the block, bar, or underline itself).
    pub cursor: Rgb,
    /// Color of the character displayed *under* a block cursor.  A solid
    /// block cursor in `cursor` would otherwise hide the glyph — this gives
    /// it a contrasting color so the character stays readable.
    pub cursor_text: Rgb,
    /// The 16 standard ANSI colors mapped to Stark palette equivalents.
    pub ansi: AnsiColors,
    /// Text-selection colors.
    pub selection: SelectionColors,
    /// Window/pane opacity settings.
    pub opacity: OpacityConfig,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: palette::ELECTRIC,
            background: palette::BLACK,
            cursor: palette::CELESTE,
            // Amber reads clearly against the bright celeste cursor block
            // and matches the Stark palette's warm-accent hue.
            cursor_text: palette::AMBER,
            ansi: AnsiColors::default(),
            selection: SelectionColors::default(),
            opacity: OpacityConfig::default(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_from_hex_roundtrip() {
        let color = Rgb::from_hex(0x52E8FF);
        assert_eq!(color.r, 0x52);
        assert_eq!(color.g, 0xE8);
        assert_eq!(color.b, 0xFF);
    }

    #[test]
    fn rgb_new_and_from_hex_agree() {
        let a = Rgb::new(0x52, 0xE8, 0xFF);
        let b = Rgb::from_hex(0x52E8FF);
        assert_eq!(a, b);
    }

    #[test]
    fn theme_default_foreground_is_electric() {
        let theme = Theme::default();
        assert_eq!(theme.foreground, palette::ELECTRIC);
    }

    #[test]
    fn theme_default_background_is_black() {
        let theme = Theme::default();
        assert_eq!(theme.background, palette::BLACK);
    }

    #[test]
    fn opacity_defaults_are_correct() {
        let op = OpacityConfig::default();
        assert!((op.title_bar_opacity - 0.95).abs() < f32::EPSILON);
        assert!((op.content_active_opacity - 0.90).abs() < f32::EPSILON);
        assert!((op.content_idle_opacity - 0.75).abs() < f32::EPSILON);
        assert_eq!(op.fade_begin_secs, 30);
        assert_eq!(op.fade_end_secs, 60);
    }

    #[test]
    fn theme_serializes_and_deserializes() {
        let original = Theme::default();
        let serialized = toml::to_string(&original).expect("serialize theme");
        let restored: Theme = toml::from_str(&serialized).expect("deserialize theme");
        assert_eq!(original.foreground, restored.foreground);
        assert_eq!(original.background, restored.background);
        assert_eq!(original.cursor, restored.cursor);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only override the cursor color; everything else should come from Default.
        let partial = r#"
            [cursor]
            r = 255
            g = 0
            b = 0
        "#;
        let theme: Theme = toml::from_str(partial).expect("partial deserialize");
        assert_eq!(theme.cursor, Rgb::new(255, 0, 0));
        // Background should still be the default black.
        assert_eq!(theme.background, palette::BLACK);
    }
}
