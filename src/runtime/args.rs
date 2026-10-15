//! Runtime default and override argument policy for Codex session resumes.

use std::collections::HashSet;

use crate::session::model::{normalize_runtime_token, CutexSessionRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CodexCliOverrideKey {
    Model,
    Sandbox,
    ApprovalPolicy,
    ReasoningEffort,
    Cwd,
}

pub fn append_codex_cli_args_with_overrides(
    base: Vec<String>,
    overrides: Vec<String>,
) -> Vec<String> {
    let override_keys = codex_cli_override_keys(&overrides);
    if override_keys.is_empty() {
        let mut merged = base;
        merged.extend(overrides);
        return merged;
    }
    let mut merged = strip_codex_cli_args_for_override_keys(base, &override_keys);
    merged.extend(overrides);
    merged
}

fn codex_cli_override_keys(args: &[String]) -> HashSet<CodexCliOverrideKey> {
    let mut keys = HashSet::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--model" | "-m" => {
                keys.insert(CodexCliOverrideKey::Model);
                index += 2;
                continue;
            }
            "--sandbox" | "-s" => {
                keys.insert(CodexCliOverrideKey::Sandbox);
                index += 2;
                continue;
            }
            "--ask-for-approval" | "-a" => {
                keys.insert(CodexCliOverrideKey::ApprovalPolicy);
                index += 2;
                continue;
            }
            "--cd" | "-C" => {
                keys.insert(CodexCliOverrideKey::Cwd);
                index += 2;
                continue;
            }
            "-c" | "--config"
                if args
                    .get(index + 1)
                    .is_some_and(|value| is_reasoning_effort_config_arg(value)) =>
            {
                keys.insert(CodexCliOverrideKey::ReasoningEffort);
                index += 2;
                continue;
            }
            _ => {}
        }

        if arg
            .strip_prefix("--model=")
            .or_else(|| arg.strip_prefix("-m="))
            .is_some()
        {
            keys.insert(CodexCliOverrideKey::Model);
        } else if arg
            .strip_prefix("--sandbox=")
            .or_else(|| arg.strip_prefix("-s="))
            .is_some()
        {
            keys.insert(CodexCliOverrideKey::Sandbox);
        } else if arg
            .strip_prefix("--ask-for-approval=")
            .or_else(|| arg.strip_prefix("-a="))
            .is_some()
        {
            keys.insert(CodexCliOverrideKey::ApprovalPolicy);
        } else if arg
            .strip_prefix("--cd=")
            .or_else(|| arg.strip_prefix("-C="))
            .is_some()
        {
            keys.insert(CodexCliOverrideKey::Cwd);
        } else if arg
            .strip_prefix("--config=")
            .is_some_and(is_reasoning_effort_config_arg)
        {
            keys.insert(CodexCliOverrideKey::ReasoningEffort);
        }
        index += 1;
    }
    keys
}

fn strip_codex_cli_args_for_override_keys(
    args: Vec<String>,
    keys: &HashSet<CodexCliOverrideKey>,
) -> Vec<String> {
    let mut filtered = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let pair_key = match arg {
            "--model" | "-m" => Some(CodexCliOverrideKey::Model),
            "--sandbox" | "-s" => Some(CodexCliOverrideKey::Sandbox),
            "--ask-for-approval" | "-a" => Some(CodexCliOverrideKey::ApprovalPolicy),
            "--cd" | "-C" => Some(CodexCliOverrideKey::Cwd),
            "-c" | "--config"
                if args
                    .get(index + 1)
                    .is_some_and(|value| is_reasoning_effort_config_arg(value)) =>
            {
                Some(CodexCliOverrideKey::ReasoningEffort)
            }
            _ => None,
        };
        if pair_key.is_some_and(|key| keys.contains(&key)) {
            index += 2;
            continue;
        }

        let inline_key = if arg.starts_with("--model=") || arg.starts_with("-m=") {
            Some(CodexCliOverrideKey::Model)
        } else if arg.starts_with("--sandbox=") || arg.starts_with("-s=") {
            Some(CodexCliOverrideKey::Sandbox)
        } else if arg.starts_with("--ask-for-approval=") || arg.starts_with("-a=") {
            Some(CodexCliOverrideKey::ApprovalPolicy)
        } else if arg.starts_with("--cd=") || arg.starts_with("-C=") {
            Some(CodexCliOverrideKey::Cwd)
        } else if arg
            .strip_prefix("--config=")
            .is_some_and(is_reasoning_effort_config_arg)
        {
            Some(CodexCliOverrideKey::ReasoningEffort)
        } else {
            None
        };
        if inline_key.is_some_and(|key| keys.contains(&key)) {
            index += 1;
            continue;
        }

        filtered.push(args[index].clone());
        index += 1;
    }
    filtered
}

fn is_reasoning_effort_config_arg(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("model_reasoning_effort")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

pub fn cutex_session_runtime_default_cli_args(record: &CutexSessionRecord) -> Vec<String> {
    let (sandbox_mode, approval_policy) = effective_runtime_permission_defaults(record);
    let mut args = Vec::new();
    if let Some(model) = record
        .model_defaults
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sandbox) = sandbox_mode {
        args.push("--sandbox".to_string());
        args.push(sandbox);
    }
    if let Some(approval) = approval_policy {
        args.push("--ask-for-approval".to_string());
        args.push(approval);
    }
    if let Some(reasoning) = record
        .reasoning_defaults
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={reasoning}"));
    }
    args
}

pub fn effective_runtime_permission_defaults(
    record: &CutexSessionRecord,
) -> (Option<String>, Option<String>) {
    let mut sandbox_mode = record.sandbox_mode.clone();
    let mut approval_policy = record.approval_policy.clone();
    if let Some(permission) = record.permission_defaults.as_deref() {
        match normalize_runtime_token(permission).as_str() {
            "full-access" | "danger-full-access" | ":danger-full-access" | "danger" => {
                if sandbox_mode.is_none() {
                    sandbox_mode = Some("danger-full-access".to_string());
                }
                if approval_policy.is_none() {
                    approval_policy = Some("never".to_string());
                }
            }
            "workspace" | "workspace-write" | ":workspace" | "ask-for-approval" => {
                if sandbox_mode.is_none() {
                    sandbox_mode = Some("workspace-write".to_string());
                }
            }
            "read-only" | ":read-only" | "readonly" | "read" => {
                if sandbox_mode.is_none() {
                    sandbox_mode = Some("read-only".to_string());
                }
            }
            _ => {}
        }
    }
    (sandbox_mode, approval_policy)
}

/// Sandbox mode a known permission preset alias selects, or `None` for an
/// unknown (custom / named) permission profile.
pub fn permission_alias_sandbox(alias: &str) -> Option<&'static str> {
    match normalize_runtime_token(alias).as_str() {
        "full-access" | "danger-full-access" | ":danger-full-access" | "danger" => {
            Some("danger-full-access")
        }
        "workspace" | "workspace-write" | ":workspace" | "cutex-workspace" | "ask-for-approval" => {
            Some("workspace-write")
        }
        "read-only" | ":read-only" | "readonly" | "read" => Some("read-only"),
        _ => None,
    }
}

/// Canonical preset alias for an explicit sandbox mode (the sandbox name
/// itself, the spelling new-thread creation stores).
pub fn permission_alias_for_sandbox(sandbox: &str) -> Option<&'static str> {
    match normalize_runtime_token(sandbox).as_str() {
        "danger-full-access" => Some("danger-full-access"),
        "workspace-write" => Some("workspace-write"),
        "read-only" => Some("read-only"),
        _ => None,
    }
}

/// A stored preset alias that selects a different sandbox than the record's
/// explicit `sandbox_mode`: returns the alias the explicit sandbox implies.
///
/// The explicit sandbox is the newer truth: a running thread's settings
/// update writes it (older daemons wrote only it, leaving the alias stale), and
/// `effective_runtime_permission_defaults` already gives it precedence. Unknown
/// (custom) aliases and unknown sandboxes are left alone so launch validation
/// still rejects them.
pub fn inconsistent_permission_alias_repair(record: &CutexSessionRecord) -> Option<&'static str> {
    let selected = permission_alias_sandbox(record.permission_defaults.as_deref()?)?;
    let explicit = permission_alias_for_sandbox(record.sandbox_mode.as_deref()?)?;
    (selected != explicit).then_some(explicit)
}

/// Permission alias to launch with: the stored alias, or the one the explicit
/// sandbox implies when the two disagree (see
/// [`inconsistent_permission_alias_repair`]).
pub fn effective_permission_alias(record: &CutexSessionRecord) -> Option<String> {
    match inconsistent_permission_alias_repair(record) {
        Some(repaired) => Some(repaired.to_string()),
        None => record.permission_defaults.clone(),
    }
}

/// Persistable repair for an inconsistent alias. Returns `(old, new)` when the
/// record changed; the caller bumps the revision, persists and logs.
pub fn repair_inconsistent_permission_alias(
    record: &mut CutexSessionRecord,
) -> Option<(String, String)> {
    let repaired = inconsistent_permission_alias_repair(record)?;
    let old = record.permission_defaults.replace(repaired.to_string())?;
    Some((old, repaired.to_string()))
}

/// Which side of a permission write is authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionWrite {
    /// The preset alias was written: align an explicit sandbox to it.
    Alias,
    /// The explicit sandbox was written: recompute the alias from it.
    Sandbox,
}

/// Keep `permission_defaults` and `sandbox_mode` consistent after one of
/// them was written. Returns whether the other field changed.
pub fn reconcile_permission_fields(record: &mut CutexSessionRecord, written: PermissionWrite) -> bool {
    match written {
        PermissionWrite::Sandbox => {
            let (Some(sandbox), Some(alias)) =
                (record.sandbox_mode.as_deref(), record.permission_defaults.as_deref())
            else {
                return false;
            };
            let explicit = permission_alias_for_sandbox(sandbox);
            if explicit.is_some() && permission_alias_sandbox(alias) == explicit {
                return false;
            }
            // A preset or custom alias that no longer describes the sandbox:
            // recompute it, or clear it when the sandbox has no preset.
            record.permission_defaults = explicit.map(str::to_string);
            true
        }
        PermissionWrite::Alias => {
            let Some(selected) =
                record.permission_defaults.as_deref().and_then(permission_alias_sandbox)
            else {
                return false;
            };
            match record.sandbox_mode.as_deref() {
                Some(sandbox) if permission_alias_for_sandbox(sandbox) != Some(selected) => {
                    record.sandbox_mode = Some(selected.to_string());
                    true
                }
                _ => false,
            }
        }
    }
}

/// Reconcile after a defaults patch that may have written either side.
/// Writing both to different presets is rejected rather than guessed.
pub fn reconcile_patched_permissions(
    record: &mut CutexSessionRecord,
    alias_written: bool,
    sandbox_written: bool,
) -> anyhow::Result<()> {
    match (alias_written, sandbox_written) {
        (true, true) => anyhow::ensure!(
            inconsistent_permission_alias_repair(record).is_none(),
            "permissions and sandboxMode select different sandboxes"
        ),
        (true, false) => {
            reconcile_permission_fields(record, PermissionWrite::Alias);
        }
        (false, true) => {
            reconcile_permission_fields(record, PermissionWrite::Sandbox);
        }
        (false, false) => {}
    }
    Ok(())
}

#[cfg(test)]
mod permission_alias_tests {
    use super::*;

    fn record(alias: Option<&str>, sandbox: Option<&str>) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.perm".to_string(),
            Some("thread-perm".to_string()),
            "host".to_string(),
            "/tmp/perm".to_string(),
            None,
            "2026-10-08T00:00:00Z".to_string(),
        )
        .expect("record");
        record.permission_defaults = alias.map(str::to_string);
        record.sandbox_mode = sandbox.map(str::to_string);
        record
    }

    #[test]
    fn tethys_record_repairs_to_explicit_sandbox() {
        // cutex.01a1141b: full access chosen from the desktop while running.
        let mut r = record(Some("workspace-write"), Some("danger-full-access"));
        r.approval_policy = Some("never".into());
        assert_eq!(inconsistent_permission_alias_repair(&r), Some("danger-full-access"));
        assert_eq!(effective_permission_alias(&r).as_deref(), Some("danger-full-access"));
        assert_eq!(
            repair_inconsistent_permission_alias(&mut r),
            Some(("workspace-write".into(), "danger-full-access".into()))
        );
        assert_eq!(inconsistent_permission_alias_repair(&r), None);
        assert_eq!(r.sandbox_mode.as_deref(), Some("danger-full-access"));
        assert_eq!(r.approval_policy.as_deref(), Some("never"));
    }

    #[test]
    fn consistent_unknown_or_partial_records_are_left_alone() {
        for (alias, sandbox) in [
            (Some("workspace"), Some("workspace-write")),
            (Some(":workspace"), Some("workspace-write")),
            (Some("full-access"), Some("danger-full-access")),
            (Some("read-only"), Some("read-only")),
            (Some("my-custom-profile"), Some("read-only")),
            (Some("workspace"), Some("external-sandbox")),
            (Some("workspace"), None),
            (None, Some("danger-full-access")),
        ] {
            let mut r = record(alias, sandbox);
            assert_eq!(inconsistent_permission_alias_repair(&r), None, "{alias:?} {sandbox:?}");
            assert_eq!(repair_inconsistent_permission_alias(&mut r), None);
            assert_eq!(r.permission_defaults.as_deref(), alias);
        }
    }

    #[test]
    fn sandbox_write_recomputes_alias() {
        let mut r = record(Some("workspace-write"), Some("danger-full-access"));
        assert!(reconcile_permission_fields(&mut r, PermissionWrite::Sandbox));
        assert_eq!(r.permission_defaults.as_deref(), Some("danger-full-access"));
        let mut r = record(Some(":workspace"), Some("workspace-write"));
        assert!(!reconcile_permission_fields(&mut r, PermissionWrite::Sandbox));
        assert_eq!(r.permission_defaults.as_deref(), Some(":workspace"));
        let mut r = record(None, Some("read-only"));
        assert!(!reconcile_permission_fields(&mut r, PermissionWrite::Sandbox));
        assert_eq!(r.permission_defaults, None);
        let mut r = record(Some("workspace"), Some("external-sandbox"));
        assert!(reconcile_permission_fields(&mut r, PermissionWrite::Sandbox));
        assert_eq!(r.permission_defaults, None);
    }

    #[test]
    fn alias_write_aligns_explicit_sandbox() {
        let mut r = record(Some("read-only"), Some("danger-full-access"));
        assert!(reconcile_permission_fields(&mut r, PermissionWrite::Alias));
        assert_eq!(r.sandbox_mode.as_deref(), Some("read-only"));
        let mut r = record(Some("full-access"), None);
        assert!(!reconcile_permission_fields(&mut r, PermissionWrite::Alias));
        assert_eq!(r.sandbox_mode, None);
        let mut r = record(Some("custom"), Some("read-only"));
        assert!(!reconcile_permission_fields(&mut r, PermissionWrite::Alias));
    }
}
