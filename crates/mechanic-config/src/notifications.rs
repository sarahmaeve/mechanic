//! Optional notifications for commands that finish while their window is unfocused.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

const DEFAULT_MIN_COMMAND_SECONDS: f64 = 10.0;
/// Reject nonsensical thresholds without risking a duration conversion panic.
const MAX_MIN_COMMAND_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Opt in to native command-completion alerts and macOS authorization.
    pub enabled: bool,
    /// Minimum elapsed command time, in seconds. Zero permits every completion.
    /// Invalid values (non-finite, negative, or greater than seven days) use 10.
    #[serde(deserialize_with = "deserialize_min_command_seconds")]
    pub min_command_seconds: f64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { enabled: false, min_command_seconds: DEFAULT_MIN_COMMAND_SECONDS }
    }
}

impl NotificationsConfig {
    /// Also validates programmatically constructed configuration values.
    pub fn min_command_duration(&self) -> Duration {
        Duration::from_secs_f64(valid_threshold(self.min_command_seconds))
    }
}

fn valid_threshold(seconds: f64) -> f64 {
    if seconds.is_finite() && (0.0..=MAX_MIN_COMMAND_SECONDS).contains(&seconds) {
        seconds
    } else {
        DEFAULT_MIN_COMMAND_SECONDS
    }
}

fn deserialize_min_command_seconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<f64, D::Error> {
    f64::deserialize(deserializer).map(valid_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_default_to_disabled_with_ten_second_threshold() {
        let config = NotificationsConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.min_command_duration(), Duration::from_secs(10));
    }

    #[test]
    fn opt_in_and_partial_configuration_keep_defaults() {
        let config: NotificationsConfig = toml::from_str("enabled = true").unwrap();
        assert!(config.enabled);
        assert_eq!(config.min_command_duration(), Duration::from_secs(10));
        let config: NotificationsConfig =
            toml::from_str("enabled = true\nmin_command_seconds = 2.5").unwrap();
        assert_eq!(config.min_command_duration(), Duration::from_millis(2500));
    }

    #[test]
    fn thresholds_allow_zero_integer_and_maximum() {
        for seconds in [0, 1, 604800] {
            let config: NotificationsConfig =
                toml::from_str(&format!("min_command_seconds = {seconds}")).unwrap();
            assert_eq!(config.min_command_duration(), Duration::from_secs(seconds));
        }
    }

    #[test]
    fn invalid_thresholds_use_default_without_disabling_opt_in() {
        for value in ["nan", "inf", "-inf", "-1.0", "604801", "1e100"] {
            let config: NotificationsConfig =
                toml::from_str(&format!("enabled = true\nmin_command_seconds = {value}")).unwrap();
            assert!(config.enabled, "{value}");
            assert_eq!(config.min_command_duration(), Duration::from_secs(10), "{value}");
        }
    }

    #[test]
    fn programmatic_nonfinite_values_are_also_safe() {
        let config = NotificationsConfig { enabled: true, min_command_seconds: f64::NAN };
        assert_eq!(config.min_command_duration(), Duration::from_secs(10));
    }

    #[test]
    fn top_level_notifications_table_roundtrips() {
        let config: crate::Config =
            toml::from_str("[notifications]\nenabled = true\nmin_command_seconds = 3.5").unwrap();
        assert!(config.notifications.enabled);
        assert_eq!(config.notifications.min_command_duration(), Duration::from_millis(3500));
        let serialized = toml::to_string(&config).unwrap();
        let restored: crate::Config = toml::from_str(&serialized).unwrap();
        assert!(restored.notifications.enabled);
        assert_eq!(restored.notifications.min_command_duration(), Duration::from_millis(3500));
    }
}
