//! Terminal-behavior configuration.

use serde::{Deserialize, Serialize};

/// What to do with the window when the child shell process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloseOnExitPolicy {
    /// Close regardless of exit status.
    Always,
    /// Close on success; retain failed sessions for inspection. Default.
    #[default]
    Success,
    /// Retain final output until the user dismisses the window.
    Never,
}

/// Scrollback and shell-exit behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// When to automatically close the window on child-shell exit.
    pub close_on_exit: CloseOnExitPolicy,

    /// Number of scrollback lines retained above the visible viewport.
    pub scrollback_lines: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { close_on_exit: CloseOnExitPolicy::Success, scrollback_lines: 10_000 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_success() {
        assert_eq!(TerminalConfig::default().close_on_exit, CloseOnExitPolicy::Success);
    }

    #[test]
    fn default_scrollback_is_10k() {
        assert_eq!(TerminalConfig::default().scrollback_lines, 10_000);
    }

    #[test]
    fn serializes_and_deserializes() {
        let original = TerminalConfig::default();
        let toml_str = toml::to_string(&original).expect("serialize terminal config");
        let round: TerminalConfig = toml::from_str(&toml_str).expect("deserialize terminal config");
        assert_eq!(round.close_on_exit, original.close_on_exit);
        assert_eq!(round.scrollback_lines, original.scrollback_lines);
    }

    #[test]
    fn policy_deserializes_lowercase() {
        let cfg: TerminalConfig =
            toml::from_str(r#"close_on_exit = "always""#).expect("parse partial");
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Always);

        let cfg: TerminalConfig =
            toml::from_str(r#"close_on_exit = "never""#).expect("parse partial");
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Never);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg: TerminalConfig =
            toml::from_str(r#"scrollback_lines = 50000"#).expect("parse partial");
        assert_eq!(cfg.scrollback_lines, 50_000);
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Success);
    }

    #[test]
    fn zero_scrollback_is_valid() {
        let cfg: TerminalConfig = toml::from_str(r#"scrollback_lines = 0"#).expect("parse zero");
        assert_eq!(cfg.scrollback_lines, 0);
    }
}
