//! Color palette and theme definitions for Mechanic.

use serde::{Deserialize, Serialize};

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Construct from a 24-bit hex literal (e.g. `0x52E8FF`).
    pub const fn from_hex(hex: u32) -> Self {
        Self { r: ((hex >> 16) & 0xFF) as u8, g: ((hex >> 8) & 0xFF) as u8, b: (hex & 0xFF) as u8 }
    }
}

/// Default theme palette.
pub mod palette {
    use super::Rgb;

    pub const ELECTRIC: Rgb = Rgb::from_hex(0x52E8FF);
    pub const CELESTE: Rgb = Rgb::from_hex(0xADFFFF);
    pub const AZURE: Rgb = Rgb::from_hex(0x007FFF);
    pub const BLUE: Rgb = Rgb::from_hex(0x0015FF);

    pub const BLACK: Rgb = Rgb::from_hex(0x000000);
    pub const NEAR_BLACK: Rgb = Rgb::from_hex(0x0A0A0A);
    pub const DIM_CYAN: Rgb = Rgb::from_hex(0x1A3A40);

    pub const AMBER: Rgb = Rgb::from_hex(0xFFB300);
    pub const GOLD: Rgb = Rgb::from_hex(0xFFD700);

    pub const ALERT: Rgb = Rgb::from_hex(0xFF4500);
    pub const RED: Rgb = Rgb::from_hex(0xCC2200);

    pub const SOFT_WHITE: Rgb = Rgb::from_hex(0xE0F8FF);
}

/// Sixteen ANSI colors, with theme-specific defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnsiColors {
    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,

    pub bright_black: Rgb,
    pub bright_red: Rgb,
    pub bright_green: Rgb,
    pub bright_yellow: Rgb,
    pub bright_blue: Rgb,
    pub bright_magenta: Rgb,
    pub bright_cyan: Rgb,
    pub bright_white: Rgb,
}

impl Default for AnsiColors {
    fn default() -> Self {
        use palette::*;
        Self {
            black: BLACK,
            red: RED,
            green: ELECTRIC,
            yellow: AMBER,
            blue: AZURE,
            magenta: BLUE,
            cyan: ELECTRIC,
            white: SOFT_WHITE,
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
        Self { background: palette::ELECTRIC, foreground: Some(palette::BLACK) }
    }
}

/// Opacity settings for the terminal window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpacityConfig {
    /// Opacity of the macOS title bar chrome.
    pub title_bar_opacity: f32,
    /// Opacity of the content area when the window is focused.
    pub content_active_opacity: f32,
    /// Opacity of the content area when the window is in the background.
    pub content_idle_opacity: f32,
    /// Unfocused glyph coverage: 0 hides text, 1 preserves full contrast.
    /// Focused text always uses full coverage.
    pub text_idle_opacity: f32,
    /// Duration of the focus-gain bloom in milliseconds.
    pub bloom_duration_ms: u32,
    /// Focus dwell in milliseconds before bloom begins, to avoid animating brief focus changes.
    pub bloom_dwell_ms: u32,
    /// Peak logo-opacity multiplier during bloom; 1 leaves brightness unchanged.
    pub bloom_peak_multiplier: f32,
}

impl Default for OpacityConfig {
    fn default() -> Self {
        Self {
            title_bar_opacity: 0.95,
            content_active_opacity: 0.85,
            content_idle_opacity: 0.65,
            text_idle_opacity: 0.55,
            bloom_duration_ms: 250,
            bloom_dwell_ms: 120,
            bloom_peak_multiplier: 2.25,
        }
    }
}

/// Complete theme configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    /// Default foreground (text) color.
    pub foreground: Rgb,
    /// Default background color.
    pub background: Rgb,
    /// Cursor color (the block, bar, or underline itself).
    pub cursor: Rgb,
    /// Text color inside a focused block cursor.
    pub cursor_text: Rgb,
    pub ansi: AnsiColors,
    /// Text-selection colors.
    pub selection: SelectionColors,
    /// Window opacity and focus animation.
    pub opacity: OpacityConfig,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: palette::ELECTRIC,
            background: palette::BLACK,
            cursor: palette::CELESTE,
            cursor_text: palette::BLACK,
            ansi: AnsiColors::default(),
            selection: SelectionColors::default(),
            opacity: OpacityConfig::default(),
        }
    }
}

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
        assert!((op.content_active_opacity - 0.85).abs() < f32::EPSILON);
        assert!((op.content_idle_opacity - 0.65).abs() < f32::EPSILON);
        assert!((op.text_idle_opacity - 0.55).abs() < f32::EPSILON);
        assert_eq!(op.bloom_duration_ms, 250);
        assert_eq!(op.bloom_dwell_ms, 120);
        assert!((op.bloom_peak_multiplier - 2.25).abs() < f32::EPSILON);
    }

    #[test]
    fn bloom_dwell_fits_within_focus_redraw_burst() {
        const FOCUS_REDRAW_BURST_MS: u32 = 5 * 33;
        let op = OpacityConfig::default();
        assert!(
            op.bloom_dwell_ms <= FOCUS_REDRAW_BURST_MS,
            "bloom_dwell_ms ({}) must be ≤ focus-redraw-burst duration ({} ms) \
             — otherwise the bloom-commit check never fires.  See OpacityConfig \
             docs for the invariant.",
            op.bloom_dwell_ms,
            FOCUS_REDRAW_BURST_MS
        );
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
        let partial = r#"
            [cursor]
            r = 255
            g = 0
            b = 0
        "#;
        let theme: Theme = toml::from_str(partial).expect("partial deserialize");
        assert_eq!(theme.cursor, Rgb::new(255, 0, 0));
        assert_eq!(theme.background, palette::BLACK);
    }
}
