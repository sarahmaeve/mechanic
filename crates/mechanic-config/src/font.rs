//! Font configuration for Mechanic.
//!
//! Specifies the primary font family, size, and an ordered list of fallback
//! families used when a glyph is not found in the primary face.

use serde::{Deserialize, Serialize};

/// Font settings used by the renderer when shaping terminal text.
///
/// Partial TOML configs are supported via `#[serde(default)]` — any missing
/// key falls back to the values returned by [`FontConfig::default`].
///
/// # Example — user config snippet
/// ```toml
/// [font]
/// family = "JetBrains Mono"
/// size = 13.0
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Primary font family name as recognised by the system font resolver.
    ///
    /// Defaults to `"Berkeley Mono"`.
    pub family: String,

    /// Font size in points.
    ///
    /// Defaults to `16.0`.
    pub size: f32,

    /// Ordered list of fallback font families tried when a glyph is absent
    /// from the primary face.
    ///
    /// Defaults to a curated list of widely-available monospace fonts.
    pub fallback_families: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "Berkeley Mono".to_string(),
            size: 16.0,
            fallback_families: vec![
                "SF Mono".to_string(),
                "Menlo".to_string(),
                "Monaco".to_string(),
                "Courier New".to_string(),
            ],
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_family_is_berkeley_mono() {
        let cfg = FontConfig::default();
        assert_eq!(cfg.family, "Berkeley Mono");
    }

    #[test]
    fn default_size_is_16() {
        let cfg = FontConfig::default();
        assert!((cfg.size - 16.0).abs() < f32::EPSILON);
    }

    #[test]
    fn default_fallbacks_are_non_empty() {
        let cfg = FontConfig::default();
        assert!(!cfg.fallback_families.is_empty());
    }

    #[test]
    fn serializes_and_deserializes() {
        let original = FontConfig::default();
        let serialized = toml::to_string(&original).expect("serialize font config");
        let restored: FontConfig = toml::from_str(&serialized).expect("deserialize font config");
        assert_eq!(original.family, restored.family);
        assert!((original.size - restored.size).abs() < f32::EPSILON);
        assert_eq!(original.fallback_families, restored.fallback_families);
    }

    #[test]
    fn partial_toml_overrides_size_only() {
        let partial = r#"size = 16.0"#;
        let cfg: FontConfig = toml::from_str(partial).expect("partial deserialize");
        assert!((cfg.size - 16.0).abs() < f32::EPSILON);
        // Family should still be the default.
        assert_eq!(cfg.family, "Berkeley Mono");
    }
}
