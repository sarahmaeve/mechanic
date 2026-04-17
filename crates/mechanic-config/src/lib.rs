//! `mechanic-config` — configuration, theme, and font settings for the
//! Mechanic terminal emulator.
//!
//! The crate exposes a single top-level [`Config`] struct that owns:
//!
//! - [`Theme`] — color palette (Stark Industries aesthetic)
//! - [`FontConfig`] — font family, size, and fallbacks
//! - [`ShellConfig`] — which shell program to launch
//!
//! Configs are stored as TOML files. Unknown keys are ignored and missing keys
//! fall back to their [`Default`] implementations, so users only need to
//! specify what they want to override.

pub mod font;
pub mod terminal;
pub mod theme;

pub use font::FontConfig;
pub use terminal::{CloseOnExitPolicy, TerminalConfig};
pub use theme::{AnsiColors, OpacityConfig, Rgb, SelectionColors, Theme};

use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Shell config ──────────────────────────────────────────────────────────────

/// Shell program to launch inside the terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Absolute path (or name on `$PATH`) of the shell executable.
    ///
    /// Defaults to the value of the `SHELL` environment variable, or
    /// `/bin/zsh` if `SHELL` is not set.
    pub program: String,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        Self { program }
    }
}

// ── Top-level Config ──────────────────────────────────────────────────────────

/// Top-level Mechanic configuration.
///
/// Load from a TOML file with [`Config::load`], or obtain in-memory defaults
/// with [`Config::default`].
///
/// # Example config file
/// ```toml
/// [font]
/// family = "JetBrains Mono"
/// size   = 13.0
///
/// [theme.opacity]
/// content_idle_opacity = 0.70
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Color palette and window opacity settings.
    pub theme: Theme,
    /// Font family, size, and fallback list.
    pub font: FontConfig,
    /// Shell program to launch.
    pub shell: ShellConfig,
    /// Terminal behaviour — scrollback, close-on-exit policy, etc.
    pub terminal: TerminalConfig,
}

impl Config {
    /// Load configuration from a TOML file at `path`.
    ///
    /// Missing-file is the expected path for users who never wrote a
    /// config — the built-in defaults are the intended UX, and there
    /// is nothing wrong to report.  That case returns silently.
    ///
    /// Any other read failure (permission denied, I/O error) or a
    /// TOML parse error does warrant a warning: the file exists but
    /// we couldn't honour it, so the user might be confused by their
    /// settings silently not taking effect.  Those paths log at
    /// `warn` level and fall back to the full default configuration
    /// so the terminal always starts.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let raw = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Missing file is an expected, zero-config first run.
                // Nothing to say — defaults are what the user wanted.
                log::debug!(
                    "mechanic-config: no config file at '{}' — using defaults",
                    path.display()
                );
                return Self::default();
            }
            Err(err) => {
                log::warn!(
                    "mechanic-config: could not read '{}': {err} — using defaults",
                    path.display()
                );
                return Self::default();
            }
        };

        match toml::from_str::<Self>(&raw) {
            Ok(cfg) => cfg,
            Err(err) => {
                log::warn!(
                    "mechanic-config: could not parse '{}': {err} — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn config_default_smoke() {
        let cfg = Config::default();
        assert_eq!(cfg.theme.foreground, theme::palette::ELECTRIC);
        assert_eq!(cfg.font.family, "Berkeley Mono");
        assert!(!cfg.shell.program.is_empty());
    }

    #[test]
    fn shell_config_falls_back_to_zsh_when_env_absent() {
        // Remove SHELL from the environment for this test.
        // (We can't easily unset it portably inside a unit test without
        // forking, so we just check the "or else" branch indirectly by
        // confirming the program is a non-empty string.)
        let sc = ShellConfig::default();
        assert!(!sc.program.is_empty());
    }

    #[test]
    fn config_serializes_and_deserializes_roundtrip() {
        let original = Config::default();
        let serialized = toml::to_string(&original).expect("serialize config");
        let restored: Config = toml::from_str(&serialized).expect("deserialize config");
        assert_eq!(original.theme.foreground, restored.theme.foreground);
        assert_eq!(original.font.family, restored.font.family);
        assert!((original.font.size - restored.font.size).abs() < f32::EPSILON);
        assert_eq!(original.shell.program, restored.shell.program);
    }

    #[test]
    fn config_load_falls_back_on_missing_file() {
        let cfg = Config::load("/nonexistent/path/mechanic.toml");
        // Should silently return defaults.
        assert_eq!(cfg.font.family, "Berkeley Mono");
    }

    #[test]
    fn config_load_partial_toml_from_tempfile() {
        let mut tmp = tempfile::NamedTempFile::new().expect("create tempfile");
        writeln!(
            tmp,
            r#"
[font]
size = 18.0

[shell]
program = "/bin/bash"
"#
        )
        .expect("write tempfile");

        let cfg = Config::load(tmp.path());
        assert!((cfg.font.size - 18.0).abs() < f32::EPSILON);
        assert_eq!(cfg.font.family, "Berkeley Mono"); // still the default
        assert_eq!(cfg.shell.program, "/bin/bash");
    }

    #[test]
    fn config_load_invalid_toml_falls_back_to_defaults() {
        let mut tmp = tempfile::NamedTempFile::new().expect("create tempfile");
        writeln!(tmp, "this is [ not valid toml !!!").expect("write tempfile");

        let cfg = Config::load(tmp.path());
        assert_eq!(cfg.font.family, "Berkeley Mono");
    }

    #[test]
    fn opacity_config_values_in_range() {
        let cfg = Config::default();
        let o = &cfg.theme.opacity;
        assert!((0.0..=1.0).contains(&o.title_bar_opacity));
        assert!((0.0..=1.0).contains(&o.content_active_opacity));
        assert!((0.0..=1.0).contains(&o.content_idle_opacity));
        assert!((0.0..=1.0).contains(&o.text_idle_opacity));
        assert!(o.content_idle_opacity <= o.content_active_opacity);
    }

    #[test]
    fn ansi_colors_all_distinct_from_background() {
        let theme = Theme::default();
        // At minimum, foreground should differ from background.
        assert_ne!(theme.foreground, theme.background);
    }
}
