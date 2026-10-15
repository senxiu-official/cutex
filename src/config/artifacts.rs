//! Artifact store settings (`config.json` `artifacts`, `/v2/settings`
//! `artifacts`; cutex-desktop `docs/research/artifacts-api-v1.md`): blob and
//! store limits and project auto capture.

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// `(field, min, max)` of every numeric setting (inclusive).
pub const RANGES: &[(&str, u64, u64)] = &[
    ("maxBlobBytes", 64 * 1024, 16 * GIB),
    ("hashExternalMaxBytes", 0, 1024 * GIB),
    ("quotaBytes", 100 * MIB, 100 * 1024 * GIB),
];
/// At most this many auto capture patterns, each at most
/// [`MAX_PATTERN_CHARS`] characters.
pub const MAX_PATTERNS: usize = 64;
pub const MAX_PATTERN_CHARS: usize = 256;

pub const DEFAULT_PATTERNS: &[&str] = &["reports/**", "figures/**", "results/**", "output/**", "outputs/**", "handoff/**"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutoCaptureSettings {
    pub enabled: bool,
    /// Globs relative to a project's workspace root (`**` = any depth).
    pub patterns: Vec<String>,
}

impl Default for AutoCaptureSettings {
    fn default() -> Self {
        Self { enabled: true, patterns: DEFAULT_PATTERNS.iter().map(|pattern| pattern.to_string()).collect() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactSettings {
    /// Larger files are registered by path (`storage: external`), not copied.
    pub max_blob_bytes: u64,
    /// External files up to this size still get a sha256.
    pub hash_external_max_bytes: u64,
    /// Distinct blob bytes the store may hold.
    pub quota_bytes: u64,
    pub auto_capture: AutoCaptureSettings,
}

impl Default for ArtifactSettings {
    fn default() -> Self {
        Self { max_blob_bytes: 64 * MIB, hash_external_max_bytes: GIB, quota_bytes: 10 * GIB, auto_capture: Default::default() }
    }
}

fn in_range(name: &str, value: u64) -> bool {
    RANGES.iter().find(|(field, _, _)| *field == name).is_some_and(|(_, min, max)| (*min..=*max).contains(&value))
}

/// A pattern is relative to the workspace root: no absolute path, drive,
/// `..` segment or control character.
pub fn pattern_error(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_PATTERN_CHARS {
        return Some(format!("1-{MAX_PATTERN_CHARS} characters"));
    }
    if trimmed.chars().any(char::is_control) {
        return Some("no control characters".into());
    }
    let normalized = trimmed.replace('\\', "/");
    if normalized.starts_with('/') || normalized.chars().nth(1) == Some(':') {
        return Some("must be relative to the workspace root".into());
    }
    if normalized.split('/').any(|segment| segment == "..") {
        return Some("must not contain ..".into());
    }
    None
}

impl ArtifactSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Invalid hand-edited values fall back to the defaults field by field.
    pub fn effective(&self) -> Self {
        let defaults = Self::default();
        let patterns: Vec<String> = self
            .auto_capture
            .patterns
            .iter()
            .filter(|pattern| pattern_error(pattern).is_none())
            .take(MAX_PATTERNS)
            .map(|pattern| pattern.trim().replace('\\', "/"))
            .collect();
        Self {
            max_blob_bytes: if in_range("maxBlobBytes", self.max_blob_bytes) { self.max_blob_bytes } else { defaults.max_blob_bytes },
            hash_external_max_bytes: if in_range("hashExternalMaxBytes", self.hash_external_max_bytes) {
                self.hash_external_max_bytes
            } else {
                defaults.hash_external_max_bytes
            },
            quota_bytes: if in_range("quotaBytes", self.quota_bytes) { self.quota_bytes } else { defaults.quota_bytes },
            auto_capture: AutoCaptureSettings { enabled: self.auto_capture.enabled, patterns },
        }
    }

    /// Apply a partial `/v2/settings` `artifacts` object.
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("artifacts: must be an object")?;
        let mut next = self.clone();
        for (key, field) in object {
            if key == "autoCapture" {
                let capture = field.as_object().ok_or("artifacts.autoCapture: must be an object")?;
                for (key, field) in capture {
                    match key.as_str() {
                        "enabled" => {
                            next.auto_capture.enabled =
                                field.as_bool().ok_or("artifacts.autoCapture.enabled: must be a boolean")?;
                        }
                        "patterns" => {
                            let items = field.as_array().ok_or("artifacts.autoCapture.patterns: must be an array")?;
                            if items.len() > MAX_PATTERNS {
                                return Err(format!("artifacts.autoCapture.patterns: at most {MAX_PATTERNS} patterns"));
                            }
                            let mut patterns = Vec::new();
                            for item in items {
                                let pattern = item.as_str().ok_or("artifacts.autoCapture.patterns: strings only")?;
                                if let Some(error) = pattern_error(pattern) {
                                    return Err(format!("artifacts.autoCapture.patterns: {pattern:?}: {error}"));
                                }
                                let pattern = pattern.trim().replace('\\', "/");
                                if !patterns.contains(&pattern) {
                                    patterns.push(pattern);
                                }
                            }
                            next.auto_capture.patterns = patterns;
                        }
                        other => return Err(format!("artifacts.autoCapture: unsupported field {other}")),
                    }
                }
                continue;
            }
            let Some((_, min, max)) = RANGES.iter().find(|(name, _, _)| name == key) else {
                return Err(format!("artifacts: unsupported field {key}"));
            };
            let number =
                field.as_u64().filter(|number| (*min..=*max).contains(number)).ok_or(format!("artifacts.{key}: must be {min}-{max}"))?;
            match key.as_str() {
                "maxBlobBytes" => next.max_blob_bytes = number,
                "hashExternalMaxBytes" => next.hash_external_max_bytes = number,
                _ => next.quota_bytes = number,
            }
        }
        Ok(next)
    }

    /// `/v2/settings` view: `{artifacts, artifactsDefault, artifactsRange}`.
    pub fn view(&self) -> serde_json::Value {
        let mut ranges: serde_json::Map<String, serde_json::Value> =
            RANGES.iter().map(|(name, min, max)| (name.to_string(), serde_json::json!([min, max]))).collect();
        ranges.insert("autoCapture.patterns".into(), serde_json::json!([0, MAX_PATTERNS]));
        serde_json::json!({"artifacts": self, "artifactsDefault": Self::default(), "artifactsRange": ranges})
    }
}

/// The current effective settings from `config.json`.
pub fn current() -> ArtifactSettings {
    crate::config::store::load_codez_config().artifacts.effective()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_patches_and_fallbacks() {
        let settings = ArtifactSettings::default();
        assert_eq!(settings.max_blob_bytes, 64 * MIB);
        assert_eq!(settings.hash_external_max_bytes, GIB);
        assert_eq!(settings.quota_bytes, 10 * GIB);
        assert!(settings.auto_capture.enabled);
        assert_eq!(settings.auto_capture.patterns.len(), 6);
        let patched = settings
            .patched(&serde_json::json!({"quotaBytes": 209715200u64, "autoCapture": {"enabled": false, "patterns": ["out\\**", "out/**"]}}))
            .unwrap();
        assert_eq!(patched.quota_bytes, 209_715_200);
        assert!(!patched.auto_capture.enabled);
        assert_eq!(patched.auto_capture.patterns, vec!["out/**"]);
        assert_eq!(patched.max_blob_bytes, settings.max_blob_bytes);
        assert!(settings.patched(&serde_json::json!({"quotaBytes": 1})).is_err());
        assert!(settings.patched(&serde_json::json!({"bogus": 1})).is_err());
        assert!(settings.patched(&serde_json::json!({"autoCapture": {"patterns": ["../x"]}})).is_err());
        assert!(settings.patched(&serde_json::json!({"autoCapture": {"patterns": ["/abs/**"]}})).is_err());
        assert!(settings.patched(&serde_json::json!({"autoCapture": {"patterns": ["C:/x"]}})).is_err());
        assert!(settings.patched(&serde_json::json!({"autoCapture": {"other": 1}})).is_err());
        let broken = ArtifactSettings { max_blob_bytes: 1, ..settings.clone() };
        assert_eq!(broken.effective().max_blob_bytes, 64 * MIB);
        let view = settings.view();
        assert_eq!(view["artifacts"]["autoCapture"]["patterns"][0], "reports/**");
        assert_eq!(view["artifactsDefault"]["maxBlobBytes"], 64 * MIB);
        assert!(view["artifactsRange"]["quotaBytes"].is_array());
    }
}
