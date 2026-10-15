//! Runtime-default mutation helpers for durable `cutex_session` records.

use crate::agent_bus::groups::normalize_registered_agent_groups;
use crate::agent_bus::identity::default_agent_group_for;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::metadata::cutex_session_key_for_user_id;
use crate::session::metadata::cutex_session_launch_cwd;
use crate::session::model::CutexSessionQuickActionMode;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CutexSessionValueUpdate<T> {
    #[default]
    Unchanged,
    Set(T),
    Clear,
}

impl<T> CutexSessionValueUpdate<T> {
    pub fn set(value: T) -> Self {
        Self::Set(value)
    }

    pub fn clear() -> Self {
        Self::Clear
    }
}

#[derive(Debug, Clone, Default)]
pub struct CutexSessionRuntimeDefaultsPatch {
    pub runtime_backend: Option<CutexSessionRuntimeBackend>,
    pub managed_cwd: CutexSessionValueUpdate<String>,
    pub permission_defaults: CutexSessionValueUpdate<String>,
    pub approval_policy: CutexSessionValueUpdate<String>,
    pub sandbox_mode: CutexSessionValueUpdate<String>,
    pub model_defaults: CutexSessionValueUpdate<String>,
    pub reasoning_defaults: CutexSessionValueUpdate<String>,
    pub default_cli_args: Option<Vec<String>>,
    pub agent_groups: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct CutexSessionRuntimeDefaultsUpdateOutcome {
    pub key: String,
    pub session_id: String,
    pub groups: Vec<String>,
    pub groups_changed: bool,
}

pub fn update_cutex_session_runtime_defaults(
    store: &mut CutexSessionStore,
    id: &str,
    patch: CutexSessionRuntimeDefaultsPatch,
) -> anyhow::Result<CutexSessionRuntimeDefaultsUpdateOutcome> {
    let key = cutex_session_key_for_user_id(store, id)
        .ok_or_else(|| anyhow::anyhow!("cutex session is not known: {id}"))?;
    update_cutex_session_runtime_defaults_by_key(store, &key, id, patch)
}

pub fn update_cutex_session_runtime_defaults_by_key(
    store: &mut CutexSessionStore,
    key: &str,
    fallback_id: &str,
    patch: CutexSessionRuntimeDefaultsPatch,
) -> anyhow::Result<CutexSessionRuntimeDefaultsUpdateOutcome> {
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating defaults: {key}")
    })?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    if let Some(runtime_backend) = patch.runtime_backend {
        record.runtime_backend = runtime_backend;
    }
    apply_optional_string_patch(&mut record.managed_cwd, patch.managed_cwd);
    let alias_written = patch.permission_defaults != CutexSessionValueUpdate::Unchanged;
    let sandbox_written = patch.sandbox_mode != CutexSessionValueUpdate::Unchanged;
    apply_optional_string_patch(&mut record.permission_defaults, patch.permission_defaults);
    apply_optional_string_patch(&mut record.approval_policy, patch.approval_policy);
    apply_optional_string_patch(&mut record.sandbox_mode, patch.sandbox_mode);
    crate::runtime::args::reconcile_patched_permissions(record, alias_written, sandbox_written)?;
    apply_optional_string_patch(&mut record.model_defaults, patch.model_defaults);
    apply_optional_string_patch(&mut record.reasoning_defaults, patch.reasoning_defaults);
    if let Some(args) = patch.default_cli_args {
        record.default_cli_args = args;
    }
    let mut groups_changed = false;
    if let Some(groups) = patch.agent_groups {
        let launch_cwd = cutex_session_launch_cwd(record).to_string();
        record.agent_groups = normalize_registered_agent_groups(groups, None, &launch_cwd);
        groups_changed = true;
    }
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionRuntimeDefaultsUpdateOutcome {
        key: key.to_string(),
        session_id: session_id_for_user_output(record, fallback_id),
        groups: record.agent_groups.clone(),
        groups_changed,
    })
}

fn apply_optional_string_patch(
    target: &mut Option<String>,
    patch: CutexSessionValueUpdate<String>,
) {
    match patch {
        CutexSessionValueUpdate::Unchanged => {}
        CutexSessionValueUpdate::Set(value) => *target = Some(value),
        CutexSessionValueUpdate::Clear => *target = None,
    }
}

fn session_id_for_user_output(record: &CutexSessionRecord, fallback_id: &str) -> String {
    record
        .codex_session_id
        .clone()
        .unwrap_or_else(|| fallback_id.to_string())
}

/// Make `record` a managed (persistent) session. `_legacy_expose_to_im` is
/// ignored since `im-exposure-retired`: every managed session gets
/// `exposed_to_backend = true`, so older binaries (which still filter the
/// owner list on that flag) list it after a rollback too.
pub fn apply_managed_session_defaults(
    record: &mut CutexSessionRecord,
    display_name: Option<&str>,
    managed_cwd: Option<String>,
    groups: Vec<String>,
    _legacy_expose_to_im: bool,
    pin: bool,
) {
    if let Some(display_name) = display_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        record.display_name_hint = Some(display_name.to_string());
        record.formal_agent_name = Some(display_name.to_string());
    } else if record.display_name_hint.is_none() {
        record.display_name_hint = record.thread_name.clone();
    }
    if let Some(managed_cwd) = managed_cwd {
        record.managed_cwd = Some(crate::session::metadata::normalize_session_cwd(&managed_cwd));
    }
    record.registration_class = AgentRegistrationClass::Persistent;
    record.agent_enabled = true;
    if !groups.is_empty() {
        record.agent_groups =
            normalize_registered_agent_groups(groups, None, cutex_session_launch_cwd(record));
    } else if record.agent_groups.is_empty() {
        record.agent_groups = vec![default_agent_group_for(
            None,
            cutex_session_launch_cwd(record),
        )];
    }
    record.exposed_to_backend = true;
    if pin {
        record.quick_action = CutexSessionQuickActionMode::Pinned;
    } else if record.quick_action == CutexSessionQuickActionMode::Hidden {
        record.quick_action = CutexSessionQuickActionMode::Auto;
    }
}

pub fn default_managed_session_runtime_backend() -> CutexSessionRuntimeBackend {
    if cfg!(windows) {
        CutexSessionRuntimeBackend::HostForeground
    } else {
        CutexSessionRuntimeBackend::CuteAlden
    }
}
