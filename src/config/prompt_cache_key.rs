//! Opt-in stable Responses prompt cache key for native agent launches
//! (design 034, `promptCacheKey` in `config.json` / `/v2/settings`).
//!
//! Every native Responses request carries a `prompt_cache_key` (by default
//! the thread's session id) and the backend derives prompt-cache affinity
//! from the session-id header, so each fresh agent thread lands on a newly
//! keyed cache shard and its first request usually misses. When enabled,
//! launches pin the thread to one stable key per host + workspace
//! (`launch/prompt_cache_key.rs`); the native side must advertise support.

use serde::{Deserialize, Serialize};

/// Whether native agent launches get a stable Responses prompt cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PromptCacheKeySettings {
    /// Pin native agent threads to `cutex-<host>-<hash of workspace>`.
    /// Default false: per-thread keys (current behavior).
    pub enabled: bool,
}

impl Default for PromptCacheKeySettings {
    fn default() -> Self {
        Self { enabled: false }
    }
}

impl PromptCacheKeySettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Apply a partial `/v2/settings` `promptCacheKey` object.
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("promptCacheKey: must be an object")?;
        let mut next = *self;
        for (key, value) in object {
            match key.as_str() {
                "enabled" => {
                    next.enabled = value
                        .as_bool()
                        .ok_or("promptCacheKey.enabled: must be a boolean")?;
                }
                other => return Err(format!("promptCacheKey: unsupported field {other}")),
            }
        }
        Ok(next)
    }
}

/// The current settings from `config.json` (default when unreadable).
pub fn current() -> PromptCacheKeySettings {
    crate::config::store::load_codez_config().prompt_cache_key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_and_omitted_from_config() {
        let settings = PromptCacheKeySettings::default();
        assert!(!settings.enabled && settings.is_default());
        let value = serde_json::to_value(settings).unwrap();
        assert_eq!(value, serde_json::json!({"enabled": false}));
        let parsed: PromptCacheKeySettings =
            serde_json::from_str("{}").expect("hand-edited block without the field keeps the default");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn patch_accepts_enabled_and_rejects_unknown_fields() {
        let settings = PromptCacheKeySettings::default();
        assert_eq!(
            settings
                .patched(&serde_json::json!({"enabled": true}))
                .unwrap(),
            PromptCacheKeySettings { enabled: true }
        );
        assert!(settings.patched(&serde_json::json!(true)).is_err());
        assert!(settings
            .patched(&serde_json::json!({"enabled": "yes"}))
            .is_err());
        assert!(settings.patched(&serde_json::json!({"x": 1})).is_err());
    }
}