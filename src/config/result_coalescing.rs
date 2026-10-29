//! Delegation-result quiet windows, exposed as agentBus.resultCoalescing.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultClass {
    ClaudeSubscription,
    ClaudeOther,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Window {
    pub enabled: bool,
    pub quiet_secs: u64,
    pub max_wait_secs: u64,
    pub cold_quiet_secs: u64,
    pub cold_max_wait_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ResultCoalescing {
    pub claude_subscription: Window,
    pub claude_other: Window,
    pub codex: Window,
}

impl Default for ResultCoalescing {
    fn default() -> Self {
        let short = Window {
            enabled: true,
            quiet_secs: 30,
            max_wait_secs: 120,
            cold_quiet_secs: 180,
            cold_max_wait_secs: 600,
        };
        Self {
            claude_subscription: Window {
                quiet_secs: 120,
                max_wait_secs: 600,
                ..short
            },
            claude_other: Window {
                cold_quiet_secs: 30,
                cold_max_wait_secs: 120,
                ..short
            },
            codex: short,
        }
    }
}

impl ResultCoalescing {
    /// Disk config is forward compatible: a newer/invalid field cannot undo
    /// a valid owner opt-out. PATCH remains strict via `patched`.
    pub fn from_raw(value: &Value) -> Self {
        let mut next = Self::default();
        let Some(classes) = value.as_object() else {
            eprintln!("result_coalescing: expected an object; using defaults");
            return next;
        };
        for (name, window) in [
            ("claudeSubscription", &mut next.claude_subscription),
            ("claudeOther", &mut next.claude_other),
            ("codex", &mut next.codex),
        ] {
            let Some(raw) = classes.get(name) else {
                continue;
            };
            let Some(fields) = raw.as_object() else {
                eprintln!("result_coalescing.{name}: expected an object");
                continue;
            };
            if let Some(raw) = fields.get("enabled") {
                if let Some(enabled) = raw.as_bool() {
                    window.enabled = enabled;
                } else {
                    eprintln!("result_coalescing.{name}.enabled: expected a boolean");
                }
            }
            let defaults = *window;
            for (field, target) in [
                ("quietSecs", &mut window.quiet_secs),
                ("maxWaitSecs", &mut window.max_wait_secs),
                ("coldQuietSecs", &mut window.cold_quiet_secs),
                ("coldMaxWaitSecs", &mut window.cold_max_wait_secs),
            ] {
                if let Some(raw) = fields.get(field) {
                    if let Some(secs) = raw.as_u64().filter(|secs| *secs <= 3600) {
                        *target = secs;
                    } else {
                        eprintln!(
                            "result_coalescing.{name}.{field}: expected 0..3600 integer seconds"
                        );
                    }
                }
            }
            if window.quiet_secs > window.max_wait_secs {
                eprintln!("result_coalescing.{name}: invalid quiet/cap pair; using default pair");
                window.quiet_secs = defaults.quiet_secs;
                window.max_wait_secs = defaults.max_wait_secs;
            }
            if window.cold_quiet_secs > window.cold_max_wait_secs {
                eprintln!(
                    "result_coalescing.{name}: invalid cold quiet/cap pair; using default pair"
                );
                window.cold_quiet_secs = defaults.cold_quiet_secs;
                window.cold_max_wait_secs = defaults.cold_max_wait_secs;
            }
        }
        next
    }
    pub fn window(self, class: ResultClass) -> Window {
        match class {
            ResultClass::ClaudeSubscription => self.claude_subscription,
            ResultClass::ClaudeOther => self.claude_other,
            ResultClass::Codex => self.codex,
        }
    }

    pub fn patched(self, patch: &Value) -> Result<Self, String> {
        let mut value = serde_json::to_value(self).expect("settings serialize");
        let object = patch
            .as_object()
            .ok_or("resultCoalescing must be an object")?;
        for (class, fields) in object {
            let target = value
                .get_mut(class)
                .ok_or_else(|| format!("resultCoalescing: unsupported class {class}"))?;
            for (key, setting) in fields
                .as_object()
                .ok_or("resultCoalescing class must be an object")?
            {
                let field = target
                    .get_mut(key)
                    .ok_or_else(|| format!("resultCoalescing: unsupported field {key}"))?;
                *field = setting.clone();
            }
        }
        let next: Self =
            serde_json::from_value(value).map_err(|e| format!("resultCoalescing: {e}"))?;
        for window in [next.claude_subscription, next.claude_other, next.codex] {
            if window.quiet_secs > window.max_wait_secs
                || window.cold_quiet_secs > window.cold_max_wait_secs
                || window.max_wait_secs > 3600
                || window.cold_max_wait_secs > 3600
            {
                return Err("resultCoalescing: quietSecs <= maxWaitSecs <= 3600 and coldQuietSecs <= coldMaxWaitSecs <= 3600 required".into());
            }
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn disk_load_keeps_valid_opt_out_and_ignores_future_fields() {
        let raw = json!({"futureClass":{},"claudeSubscription":{"enabled":false,"futureField":3,"quietSecs":"bad","coldMaxWaitSecs":9999},"claudeOther":{"enabled":false,"quietSecs":12},"codex":{"quietSecs":121}});
        let config: crate::config::agent_bus::AgentBusSettings =
            serde_json::from_value(json!({"result_coalescing":raw})).unwrap();
        assert!(!config.result_coalescing.claude_subscription.enabled);
        assert_eq!(config.result_coalescing.claude_subscription.quiet_secs, 120);
        assert!(!config.result_coalescing.claude_other.enabled);
        assert_eq!(config.result_coalescing.claude_other.quiet_secs, 12);
        assert_eq!(config.result_coalescing.codex.quiet_secs, 30);
        assert!(ResultCoalescing::default().patched(&raw).is_err());
    }
    #[test]
    fn partial_settings_preserve_other_classes_and_validate() {
        let defaults = ResultCoalescing::default();
        let changed = defaults
            .patched(&json!({"claudeSubscription":{"enabled":false},"codex":{"quietSecs":15}}))
            .unwrap();
        assert!(!changed.claude_subscription.enabled);
        assert_eq!(changed.codex.quiet_secs, 15);
        assert_eq!(changed.claude_other, defaults.claude_other);
        for bad in [
            json!({"unknown":{}}),
            json!({"codex":{"enabled":0}}),
            json!({"codex":{"quietSecs":121}}),
            json!({"codex":{"maxWaitSecs":3601}}),
            json!({"codex":{"quietSecs":-1}}),
            json!({"codex":{"x":1}}),
        ] {
            assert!(defaults.patched(&bad).is_err(), "{bad}");
        }
    }
}
