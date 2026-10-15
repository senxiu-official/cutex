//! Launch permission defaults for newly created native threads.
//!
//! Two kinds of new thread exist: formal **agents** (`cutex human new`,
//! `POST /v2/human/agents`) and **quick chats** (owned sessions: `cutex new`,
//! TUI "New session", `POST /v2/human/sessions`). Each kind has its own
//! configurable defaults in `config.json` (`new_thread_defaults`); every
//! field is optional and an absent field falls back to the built-in default
//! for that kind. The resolved [`LaunchPermissions`] are persisted on the
//! session record at creation, so later config edits never change existing
//! threads and relaunches keep what the thread was created with.
//!
//! Model / reasoning defaults are per profile (`new_session_defaults`) and
//! are not duplicated here.

use anyhow::ensure;
use serde::{Deserialize, Serialize};

pub const SANDBOX_MODES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
/// Approval policies cutex can launch with. `untrusted` is rejected by the
/// native config loader and `on-failure` is a deprecated alias of `on-request`.
pub const APPROVAL_POLICIES: &[&str] = &["never", "on-request"];
pub const MAX_WRITABLE_ROOTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadKind {
    Agent,
    QuickChat,
}

/// One kind's configured defaults; `None` means "built-in default".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// Stored (config.json / session records): unknown fields are ignored so an
// older cutex still reads a newer file (persistence audit, backend
// auto-update RFC §6.2); dropped on this binary's next save, like CodezConfig.
pub struct ThreadLaunchDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable_roots: Option<Vec<String>>,
}

impl ThreadLaunchDefaults {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `config.json` `new_thread_defaults`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// Stored (config.json / session records): unknown fields are ignored so an
// older cutex still reads a newer file (persistence audit, backend
// auto-update RFC §6.2); dropped on this binary's next save, like CodezConfig.
pub struct NewThreadDefaults {
    #[serde(default, skip_serializing_if = "ThreadLaunchDefaults::is_empty")]
    pub agent: ThreadLaunchDefaults,
    #[serde(default, skip_serializing_if = "ThreadLaunchDefaults::is_empty")]
    pub quick_chat: ThreadLaunchDefaults,
}

impl NewThreadDefaults {
    pub fn is_empty(&self) -> bool {
        self.agent.is_empty() && self.quick_chat.is_empty()
    }

    pub fn configured(&self, kind: ThreadKind) -> &ThreadLaunchDefaults {
        match kind {
            ThreadKind::Agent => &self.agent,
            ThreadKind::QuickChat => &self.quick_chat,
        }
    }

    pub fn configured_mut(&mut self, kind: ThreadKind) -> &mut ThreadLaunchDefaults {
        match kind {
            ThreadKind::Agent => &mut self.agent,
            ThreadKind::QuickChat => &mut self.quick_chat,
        }
    }

    /// Effective defaults for a new thread of `kind`.
    pub fn resolve(&self, kind: ThreadKind) -> LaunchPermissions {
        let mut resolved = LaunchPermissions::builtin(kind);
        let configured = self.configured(kind);
        if let Some(sandbox) = &configured.sandbox {
            resolved.sandbox = sandbox.clone();
        }
        if let Some(approval) = &configured.approval {
            resolved.approval = approval.clone();
        }
        if let Some(network) = configured.network_access {
            resolved.network_access = network;
        }
        if let Some(roots) = &configured.writable_roots {
            resolved.writable_roots = roots.clone();
        }
        resolved
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (kind, name) in [(ThreadKind::Agent, "agent"), (ThreadKind::QuickChat, "quickChat")] {
            self.resolve(kind)
                .validate_shape()
                .map_err(|error| anyhow::anyhow!("newThreadDefaults.{name}: {error}"))?;
        }
        Ok(())
    }
}

/// Resolved, persisted per-thread launch permissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Stored (config.json / session records): unknown fields are ignored so an
// older cutex still reads a newer file (persistence audit, backend
// auto-update RFC §6.2); dropped on this binary's next save, like CodezConfig.
pub struct LaunchPermissions {
    pub sandbox: String,
    pub approval: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub network_access: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

impl LaunchPermissions {
    /// Agents use the composer's "auto" mode (workspace-write with network,
    /// on-request; owner decision 2026-10-15: before, danger-full-access +
    /// never). Homes that existed before keep full access through the
    /// one-time migration `fresh-agent-permissions`, which stores the old
    /// values explicitly. Quick chats are confined to their own directory
    /// (workspace-write, no network) and ask before escalating (on-request).
    pub fn builtin(kind: ThreadKind) -> Self {
        match kind {
            ThreadKind::Agent => Self {
                sandbox: "workspace-write".into(),
                approval: "on-request".into(),
                network_access: true,
                writable_roots: Vec::new(),
            },
            ThreadKind::QuickChat => Self {
                sandbox: "workspace-write".into(),
                approval: "on-request".into(),
                network_access: false,
                writable_roots: Vec::new(),
            },
        }
    }

    /// Network access and extra writable roots only matter for workspace-write.
    pub fn has_workspace_extras(&self) -> bool {
        self.sandbox == "workspace-write" && (self.network_access || !self.writable_roots.is_empty())
    }

    /// Checks that need no filesystem access.
    pub fn validate_shape(&self) -> anyhow::Result<()> {
        ensure!(
            SANDBOX_MODES.contains(&self.sandbox.as_str()),
            "sandbox must be one of {}",
            SANDBOX_MODES.join(", ")
        );
        ensure!(
            APPROVAL_POLICIES.contains(&self.approval.as_str()),
            "approvalPolicy must be one of {}",
            APPROVAL_POLICIES.join(", ")
        );
        validate_writable_roots(&self.writable_roots)
    }

    /// Shape checks plus: every writable root exists as a directory. Roots
    /// are stored without the Windows verbatim prefix.
    pub fn validate_for_launch(&mut self) -> anyhow::Result<()> {
        self.validate_shape()?;
        for root in &mut self.writable_roots {
            let canonical = std::path::Path::new(root.as_str())
                .canonicalize()
                .map_err(|_| anyhow::anyhow!("writable root does not exist: {root}"))?;
            ensure!(canonical.is_dir(), "writable root must be a directory: {root}");
            let text = canonical
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("writable root must be UTF-8: {root}"))?;
            *root = crate::agent_management::strip_verbatim_prefix(text).to_string();
        }
        let mut seen = std::collections::BTreeSet::new();
        self.writable_roots.retain(|root| seen.insert(root.clone()));
        Ok(())
    }

    /// Persist onto a session record (the source of truth for relaunches).
    pub fn apply_to_record(&self, record: &mut crate::session::model::CutexSessionRecord) {
        record.sandbox_mode = Some(self.sandbox.clone());
        record.permission_defaults = Some(self.sandbox.clone());
        record.approval_policy = Some(self.approval.clone());
        record.sandbox_network_access = self.network_access.then_some(true);
        record.sandbox_writable_roots = self.writable_roots.clone();
    }
}

pub fn validate_writable_roots(roots: &[String]) -> anyhow::Result<()> {
    ensure!(
        roots.len() <= MAX_WRITABLE_ROOTS,
        "writableRoots allows at most {MAX_WRITABLE_ROOTS} entries"
    );
    for root in roots {
        ensure!(
            !root.is_empty()
                && root.chars().count() <= 1024
                && !root.chars().any(char::is_control)
                && std::path::Path::new(root).is_absolute(),
            "writableRoots entries must be absolute paths"
        );
    }
    Ok(())
}

/// Per-request launch overrides accepted by the Human create routes
/// (`launch` object). Every field is optional; absent fields keep the
/// kind's configured default (model / effort: the profile default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable_roots: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl LaunchOverrides {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn has_permission_overrides(&self) -> bool {
        self.sandbox.is_some()
            || self.approval_policy.is_some()
            || self.network_access.is_some()
            || self.writable_roots.is_some()
    }

    /// Shape checks that need no filesystem or profile access.
    pub fn validate(&self) -> Result<(), String> {
        let visible = |value: &str| {
            !value.trim().is_empty()
                && value.trim() == value
                && value.chars().count() <= 200
                && !value.chars().any(char::is_control)
        };
        if let Some(sandbox) = &self.sandbox {
            if !SANDBOX_MODES.contains(&sandbox.as_str()) {
                return Err(format!("launch.sandbox must be one of {}", SANDBOX_MODES.join(", ")));
            }
        }
        if let Some(approval) = &self.approval_policy {
            if !APPROVAL_POLICIES.contains(&approval.as_str()) {
                return Err(format!(
                    "launch.approvalPolicy must be one of {}",
                    APPROVAL_POLICIES.join(", ")
                ));
            }
        }
        if let Some(roots) = &self.writable_roots {
            validate_writable_roots(roots).map_err(|error| format!("launch.{error}"))?;
        }
        if self.model.as_deref().is_some_and(|model| !visible(model)) {
            return Err("launch.model must be a non-empty model slug".into());
        }
        if let Some(effort) = &self.reasoning_effort {
            if !visible(effort) || !effort.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err("launch.reasoningEffort must be a reasoning effort name".into());
            }
        }
        Ok(())
    }

    pub fn apply_permissions(&self, permissions: &mut LaunchPermissions) {
        if let Some(sandbox) = &self.sandbox {
            permissions.sandbox = sandbox.clone();
        }
        if let Some(approval) = &self.approval_policy {
            permissions.approval = approval.clone();
        }
        if let Some(network) = self.network_access {
            permissions.network_access = network;
        }
        if let Some(roots) = &self.writable_roots {
            permissions.writable_roots = roots.clone();
        }
    }
}

/// Defaults for a new thread of `kind` with request overrides applied and
/// validated against the filesystem.
pub fn resolve_for_create(
    kind: ThreadKind,
    overrides: Option<&LaunchOverrides>,
) -> anyhow::Result<LaunchPermissions> {
    let config = crate::config::store::load_codez_config_checked()?;
    let mut permissions = config.new_thread_defaults.resolve(kind);
    if let Some(overrides) = overrides {
        overrides.apply_permissions(&mut permissions);
    }
    permissions.validate_for_launch()?;
    Ok(permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_defaults_put_agents_in_auto_mode_and_confine_quick_chats() {
        let defaults = NewThreadDefaults::default();
        let agent = defaults.resolve(ThreadKind::Agent);
        assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("workspace-write", "on-request"));
        assert!(agent.network_access && agent.writable_roots.is_empty());
        let chat = defaults.resolve(ThreadKind::QuickChat);
        assert_eq!((chat.sandbox.as_str(), chat.approval.as_str()), ("workspace-write", "on-request"));
        assert!(!chat.network_access && chat.writable_roots.is_empty());
        assert!(!chat.has_workspace_extras());
    }

    #[test]
    fn configured_fields_override_only_themselves() {
        let defaults: NewThreadDefaults = serde_json::from_value(serde_json::json!({
            "quick_chat": { "network_access": true },
            "agent": { "sandbox": "workspace-write" }
        }))
        .unwrap();
        let chat = defaults.resolve(ThreadKind::QuickChat);
        assert!(chat.network_access && chat.has_workspace_extras());
        assert_eq!(chat.approval, "on-request");
        let agent = defaults.resolve(ThreadKind::Agent);
        assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("workspace-write", "on-request"));
        defaults.validate().unwrap();
        let bad: NewThreadDefaults =
            serde_json::from_value(serde_json::json!({"agent": {"approval": "untrusted"}})).unwrap();
        assert!(bad.validate().is_err());
        // A field from a newer cutex: read (ignored), never fatal for config.json.
        let newer: NewThreadDefaults =
            serde_json::from_value(serde_json::json!({"other": {}, "agent": {"future": 1, "sandbox": "read-only"}})).unwrap();
        assert_eq!(newer.agent.sandbox.as_deref(), Some("read-only"));
        assert_eq!(serde_json::to_value(NewThreadDefaults::default()).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn overrides_are_strict_and_validated() {
        let ok: LaunchOverrides = serde_json::from_value(serde_json::json!({
            "sandbox": "read-only", "approvalPolicy": "never", "networkAccess": true,
            "writableRoots": [std::env::temp_dir()], "model": "gpt-5.5", "reasoningEffort": "high"
        }))
        .unwrap();
        ok.validate().unwrap();
        let mut permissions = LaunchPermissions::builtin(ThreadKind::QuickChat);
        ok.apply_permissions(&mut permissions);
        assert_eq!(permissions.sandbox, "read-only");
        assert!(!permissions.has_workspace_extras());
        assert!(serde_json::from_value::<LaunchOverrides>(serde_json::json!({"sandboxMode": "x"})).is_err());
        for bad in [
            serde_json::json!({"sandbox": "full"}),
            serde_json::json!({"approvalPolicy": "untrusted"}),
            serde_json::json!({"approvalPolicy": "on-failure"}),
            serde_json::json!({"writableRoots": ["relative"]}),
            serde_json::json!({"model": " x"}),
            serde_json::json!({"reasoningEffort": "hi gh"}),
        ] {
            let parsed: LaunchOverrides = serde_json::from_value(bad.clone()).unwrap();
            assert!(parsed.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn launch_validation_canonicalizes_and_dedupes_roots() {
        let dir = std::env::temp_dir();
        let mut permissions = LaunchPermissions {
            sandbox: "workspace-write".into(),
            approval: "on-request".into(),
            network_access: false,
            writable_roots: vec![dir.display().to_string(), dir.display().to_string()],
        };
        permissions.validate_for_launch().unwrap();
        assert_eq!(permissions.writable_roots.len(), 1);
        assert!(!permissions.writable_roots[0].starts_with(r"\\?\"));
        permissions.writable_roots = vec![dir.join("cutex-missing-root-xyz").display().to_string()];
        assert!(permissions.validate_for_launch().is_err());
    }
}
