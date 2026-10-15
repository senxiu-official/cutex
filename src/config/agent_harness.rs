//! Agent harness settings (`config.json` `agent_harness`, `/v2/settings`
//! `agentHarness`; capability `agent-harness`): whether cutex puts its
//! `# Cutex` environment section in front of every agent it launches
//! (cutex-desktop `docs/research/agent-harness.md`). Applied when a runtime
//! is next launched. r217 and older ignore the block and drop it on their
//! next config save (the default, enabled, then applies again).

use serde::{Deserialize, Serialize};

/// Opt-in browser instruction shared by the two runtime launch harnesses.
pub const BROWSER_INSTRUCTION: &str = "\n- When you need to view a web page yourself, use browser_* tools; do not use shell to open the system browser. They are for your own inspection, not for showing the owner a result.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentHarnessSettings {
    /// Render the `# Cutex` section at launch (default true).
    pub enabled: bool,
}

impl Default for AgentHarnessSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl AgentHarnessSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Apply a partial `/v2/settings` `agentHarness` object.
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("agentHarness: must be an object")?;
        let mut next = *self;
        for (key, value) in object {
            match key.as_str() {
                "enabled" => {
                    next.enabled = value.as_bool().ok_or("agentHarness.enabled: must be a boolean")?;
                }
                other => return Err(format!("agentHarness: unsupported field {other}")),
            }
        }
        Ok(next)
    }
}

/// The current settings from `config.json` (default when unreadable).
pub fn current() -> AgentHarnessSettings {
    crate::config::store::load_codez_config().agent_harness
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_patch_and_serialized_shape() {
        let settings = AgentHarnessSettings::default();
        assert!(settings.enabled && settings.is_default());
        let off = settings.patched(&serde_json::json!({"enabled": false})).unwrap();
        assert!(!off.enabled && !off.is_default());
        assert_eq!(serde_json::to_value(off).unwrap(), serde_json::json!({"enabled": false}));
        for bad in [serde_json::json!({"enabled": "no"}), serde_json::json!({"x": 1}), serde_json::json!(true)] {
            assert!(settings.patched(&bad).is_err(), "{bad}");
        }
        // A hand-edited block without the field keeps the default.
        let parsed: AgentHarnessSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.enabled);
    }
}
