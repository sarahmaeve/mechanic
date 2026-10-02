//! Font configuration for Mechanic.

use serde::{Deserialize, Serialize};

/// Font settings used by the renderer when shaping terminal text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Primary system font family.
    pub family: String,

    /// Font size in points.
    pub size: f32,

    /// Preferred fallback families, followed by platform font fallback.
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
                "Hiragino Sans".to_string(),
                "Noto Sans Mono CJK JP".to_string(),
                "Geeza Pro".to_string(),
                "Noto Sans Arabic".to_string(),
            ],
        }
    }
}

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
        assert_eq!(cfg.family, "Berkeley Mono");
    }
}
