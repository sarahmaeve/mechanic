//! TOML configuration with defaults for missing fields and ignored unknown keys.

pub mod control;
pub mod font;
pub mod notifications;
pub mod session;
pub mod terminal;
pub mod theme;

pub use control::ControlConfig;
pub use font::FontConfig;
pub use notifications::NotificationsConfig;
pub use session::SessionConfig;
pub use terminal::{CloseOnExitPolicy, TerminalConfig};
pub use theme::{AnsiColors, OpacityConfig, Rgb, SelectionColors, Theme};

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Shell program to launch inside the terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Shell path or executable name; defaults to `$SHELL`, then `/bin/zsh`.
    pub program: String,
    /// Install command-boundary and working-directory hooks in supported shells.
    pub integration: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        Self { program, integration: true }
    }
}

/// Top-level Mechanic configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Color palette and window opacity settings.
    pub theme: Theme,
    /// Font family, size, and fallback list.
    pub font: FontConfig,
    /// Shell program to launch.
    pub shell: ShellConfig,
    /// Scrollback and shell-exit settings.
    pub terminal: TerminalConfig,
    /// Optional command-completion alerts for unfocused windows.
    pub notifications: NotificationsConfig,
    /// Restore the last workspace's layout and pane directories.
    pub session: SessionConfig,
    /// Enable the local control endpoint.
    pub control: ControlConfig,
}

impl Config {
    /// Load TOML, falling back to all defaults on missing files, read errors, or parse errors.
    /// Missing files log at debug level; other failures warn.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let raw = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
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
        assert!(cfg.shell.integration);
        assert!(cfg.session.restore);
        assert!(cfg.control.enabled);
    }

    #[test]
    fn shell_config_falls_back_to_zsh_when_env_absent() {
        let sc = ShellConfig::default();
        assert!(!sc.program.is_empty());
    }

    #[test]
    fn session_and_control_can_be_disabled_independently() {
        let cfg: Config =
            toml::from_str("[session]\nrestore = false\n[control]\nenabled = false\n").unwrap();
        assert!(!cfg.session.restore);
        assert!(!cfg.control.enabled);
        let partial: Config = toml::from_str("[session]\n[control]\n").unwrap();
        assert!(partial.session.restore);
        assert!(partial.control.enabled);
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
        assert_eq!(original.shell.integration, restored.shell.integration);
    }

    #[test]
    fn config_load_falls_back_on_missing_file() {
        let cfg = Config::load("/nonexistent/path/mechanic.toml");
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
integration = false
"#
        )
        .expect("write tempfile");

        let cfg = Config::load(tmp.path());
        assert!((cfg.font.size - 18.0).abs() < f32::EPSILON);
        assert_eq!(cfg.font.family, "Berkeley Mono"); // still the default
        assert_eq!(cfg.shell.program, "/bin/bash");
        assert!(!cfg.shell.integration);
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
        assert_ne!(theme.foreground, theme.background);
    }
}
