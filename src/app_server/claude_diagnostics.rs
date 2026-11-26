//! Allowlisted Claude presentation facts. Producer strings, command output and
//! provider/settings prose never cross this Management boundary.
use serde::Serialize;
use serde_json::Value;

pub const NATIVE_METHOD: &str = "claude/runtime/diagnosticsChanged";
pub const METHOD: &str = "cutex/session/claudeDiagnostics/changed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeDiagnostics {
    warnings: Vec<&'static str>,
    active_hooks: Vec<Hook>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Hook {
    event: &'static str,
    started_at_ms: u64,
}

impl ClaudeDiagnostics {
    pub fn from_wire(value: &Value) -> Option<Self> {
        let warnings = value.get("warnings")?.as_array()?;
        let hooks = value.get("activeHooks")?.as_array()?;
        let warnings = ["configurationNotApplied", "managedHooksNotLoaded", "mcpConnectionFailed", "mcpAuthenticationRequired"]
            .into_iter()
            .filter(|code| warnings.iter().take(8).any(|warning| warning.as_str() == Some(*code)))
            .collect();
        let active_hooks = hooks.iter().take(32).filter_map(|hook| {
            let event = match hook.get("event")?.as_str()? {
                "PreToolUse" => "PreToolUse",
                "PostToolUse" => "PostToolUse",
                "PostToolUseFailure" => "PostToolUseFailure",
                "PermissionRequest" => "PermissionRequest",
                "UserPromptSubmit" => "UserPromptSubmit",
                "SessionStart" => "SessionStart",
                "SessionEnd" => "SessionEnd",
                "Stop" => "Stop",
                "SubagentStart" => "SubagentStart",
                "SubagentStop" => "SubagentStop",
                "PreCompact" => "PreCompact",
                "PostCompact" => "PostCompact",
                "Notification" => "Notification",
                "Setup" => "Setup",
                "TaskCompleted" => "TaskCompleted",
                "TeammateIdle" => "TeammateIdle",
                "ConfigChange" => "ConfigChange",
                "InstructionsLoaded" => "InstructionsLoaded",
                _ => "other",
            };
            let started_at_ms = hook.get("startedAtMs")?.as_u64()?;
            (0 < started_at_ms && started_at_ms < 9_000_000_000_000).then_some(Hook { event, started_at_ms })
        }).collect();
        Some(Self { warnings, active_hooks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostics_project_only_public_fields() {
        let raw = json!({"warnings":["configurationNotApplied","secret-provider-text","configurationNotApplied"],
            "activeHooks":[{"event":"secret-name","startedAtMs":123,"stdout":"secret"},
                {"event":"SessionStart","startedAtMs":123},{"event":"Stop","startedAtMs":-1}],
            "config":"private"});
        let value = serde_json::to_value(ClaudeDiagnostics::from_wire(&raw).unwrap()).unwrap();
        assert_eq!(value, json!({"warnings":["configurationNotApplied"],
            "activeHooks":[{"event":"other","startedAtMs":123},{"event":"SessionStart","startedAtMs":123}]}));
        assert!(ClaudeDiagnostics::from_wire(&json!({"warnings":[]})).is_none());
    }
}
