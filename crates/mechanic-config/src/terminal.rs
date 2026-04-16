//! Terminal-behavior configuration.
//!
//! Settings that control how the terminal emulator behaves — scrollback
//! size, child-exit handling, and similar knobs that sit between the
//! grid/renderer and the user's shell.  Purely policy: nothing in here
//! affects colors, fonts, or rendering.

use serde::{Deserialize, Serialize};

// ── Close-on-exit policy ──────────────────────────────────────────────────────

/// What to do with the window when the child shell process exits.
///
/// The default ([`CloseOnExitPolicy::Success`]) matches iTerm2: clean
/// exits (code 0 — user typed `exit`, Ctrl+D on an empty prompt,
/// `logout`) close the window automatically, while non-zero exits
/// freeze the window so the user can read whatever went wrong before
/// dismissing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloseOnExitPolicy {
    /// Close the window regardless of exit status.  Matches xterm and
    /// macOS Terminal.app's default behaviour.
    Always,
    /// Close the window only when the shell exited successfully (code
    /// `0`).  Non-zero exits freeze the window for inspection.  Default.
    #[default]
    Success,
    /// Never close automatically — always freeze.  Useful for long-running
    /// sessions where you want to keep the final output visible until
    /// you dismiss it yourself.
    Never,
}

// ── TerminalConfig ────────────────────────────────────────────────────────────

/// Policy knobs for the terminal emulator itself (not the renderer).
///
/// Partial TOML is supported — any missing key falls back to the value
/// returned by [`TerminalConfig::default`].
///
/// # Example
/// ```toml
/// [terminal]
/// close_on_exit    = "always"
/// scrollback_lines = 50000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// When to automatically close the window on child-shell exit.
    pub close_on_exit: CloseOnExitPolicy,

    /// Number of scrollback lines retained above the visible viewport.
    ///
    /// Larger values use more memory but let you scroll further back.
    /// Defaults to `10_000`, matching alacritty's default.  Set to `0`
    /// to disable scrollback entirely.
    pub scrollback_lines: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { close_on_exit: CloseOnExitPolicy::Success, scrollback_lines: 10_000 }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        // The lowercase rename ensures user-friendly TOML values
        // (`close_on_exit = "always"` rather than `"Always"`).
        let cfg: TerminalConfig =
            toml::from_str(r#"close_on_exit = "always""#).expect("parse partial");
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Always);

        let cfg: TerminalConfig =
            toml::from_str(r#"close_on_exit = "never""#).expect("parse partial");
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Never);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Override only scrollback_lines; close_on_exit should stay default.
        let cfg: TerminalConfig =
            toml::from_str(r#"scrollback_lines = 50000"#).expect("parse partial");
        assert_eq!(cfg.scrollback_lines, 50_000);
        assert_eq!(cfg.close_on_exit, CloseOnExitPolicy::Success);
    }

    #[test]
    fn zero_scrollback_is_valid() {
        // Users who want minimal memory use may set scrollback_lines = 0.
        // The alacritty Term accepts 0 (disables history).
        let cfg: TerminalConfig =
            toml::from_str(r#"scrollback_lines = 0"#).expect("parse zero");
        assert_eq!(cfg.scrollback_lines, 0);
    }
}
