//! Durable `cutex_session` service helpers shared by CLI, wizard, and
//! management code.

use crate::agent_bus::groups::apply_group_update;
use crate::agent_bus::identity::normalize_agent_groups;
use crate::agent_bus::model::AgentGroupUpdateMode;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::identity::default_cutex_session_id_for_codex_session;
use crate::session::identity::normalize_codex_session_id;
use crate::session::model::CutexSessionQuickActionMode;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;

pub use super::im_bridge::coding_registration_from_cutex_session_record;
pub use super::im_bridge::im_registry_from_cutex_session_store;
pub use super::im_bridge::persist_cutex_session_store_and_im_record;
pub use super::im_bridge::reconcile_cutex_session_from_im_registration;
pub use super::im_bridge::reconcile_cutex_session_store_from_im_registration;
pub use super::metadata::cutex_session_display_name;
pub use super::metadata::cutex_session_is_managed;
pub use super::metadata::cutex_session_key_for_codex_session;
pub use super::metadata::cutex_session_key_for_codex_session_including_retired;
pub use super::metadata::cutex_session_key_for_user_id;
pub use super::metadata::cutex_session_key_for_user_id_including_retired;
pub use super::metadata::cutex_session_launch_cwd;
pub use super::metadata::normalize_cutex_session_managed_cwd_path;
pub use super::routing::update_cutex_session_routing_by_key;
pub use super::routing::CutexSessionRoutingPatch;
pub use super::routing::CutexSessionRoutingUpdateOutcome;
pub use super::runtime_defaults::apply_managed_session_defaults;
pub use super::runtime_defaults::default_managed_session_runtime_backend;
pub use super::runtime_defaults::update_cutex_session_runtime_defaults;
pub use super::runtime_defaults::update_cutex_session_runtime_defaults_by_key;
pub use super::runtime_defaults::CutexSessionRuntimeDefaultsPatch;
pub use super::runtime_defaults::CutexSessionRuntimeDefaultsUpdateOutcome;
pub use super::runtime_defaults::CutexSessionValueUpdate;
pub use super::runtime_reconciliation::apply_session_online_runtime_observation;
pub use super::runtime_reconciliation::clear_cutex_session_runtime_record;
pub use super::runtime_reconciliation::reconcile_cutex_session_store_from_agent;

#[derive(Debug, Clone)]
pub struct CutexSessionUpdateOutcome {
    pub key: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct CutexSessionGroupsUpdateOutcome {
    pub key: String,
    pub session_id: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CutexSessionEnsureSeed {
    pub host_id: String,
    pub cwd: String,
    pub profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CutexSessionAdoptOptions<'a> {
    pub display_name: Option<&'a str>,
    pub managed_cwd: Option<String>,
    pub groups: Vec<String>,
    pub expose_to_im: bool,
    pub pin: bool,
}

#[derive(Debug, Clone)]
pub struct CutexSessionAdoptOutcome {
    pub key: String,
    pub session_id: String,
    pub display_name: String,
    pub runtime_backend: CutexSessionRuntimeBackend,
    pub launch_cwd: String,
    pub im_visible: bool,
    pub groups: Vec<String>,
}

pub fn set_cutex_session_quick_action(
    store: &mut CutexSessionStore,
    id: &str,
    mode: CutexSessionQuickActionMode,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    let key = require_cutex_session_key_for_user_id(store, id)?;
    let record = store.sessions.get_mut(&key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating quick action: {key}")
    })?;
    record.quick_action = mode;
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
    })
}

pub fn update_cutex_session_groups(
    store: &mut CutexSessionStore,
    id: &str,
    groups: Vec<String>,
    mode: AgentGroupUpdateMode,
) -> anyhow::Result<CutexSessionGroupsUpdateOutcome> {
    let groups = normalize_agent_groups(groups);
    if groups.is_empty() {
        anyhow::bail!("At least one non-empty group is required");
    }
    let key = require_cutex_session_key_for_user_id(store, id)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while updating groups: {key}"))?;
    record.agent_groups = apply_group_update(&record.agent_groups, &groups, mode);
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionGroupsUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
        groups: record.agent_groups.clone(),
    })
}

pub fn set_cutex_session_managed_cwd(
    store: &mut CutexSessionStore,
    id: &str,
    managed_cwd: Option<String>,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    let key = require_cutex_session_key_for_user_id(store, id)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while updating cwd: {key}"))?;
    record.managed_cwd = managed_cwd.map(|cwd| super::metadata::normalize_session_cwd(&cwd));
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
    })
}

pub fn set_cutex_session_profile_by_key(
    store: &mut CutexSessionStore,
    key: &str,
    profile: Option<String>,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    if profile
        .as_deref()
        .is_some_and(|profile| profile.trim().is_empty())
    {
        anyhow::bail!("Profile name cannot be empty");
    }
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating profile: {key}")
    })?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    crate::runtime::claude_accounts::check_profile_change(record, profile.as_deref())
        .map_err(anyhow::Error::new)?;
    record.profile = profile;
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key: key.to_string(),
        session_id: session_id_for_user_output(record, key),
    })
}

/// Change durable configured profile intent under the caller's session-revision
/// fence.  The comparison is deliberately made while this mutable store view is
/// held, immediately before the write, rather than against a Management
/// projection that may already be stale.
pub fn set_cutex_session_profile_by_key_with_expected_revision(
    store: &mut CutexSessionStore,
    key: &str,
    profile: Option<String>,
    expected_revision: u64,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    if profile
        .as_deref()
        .is_some_and(|profile| profile.trim().is_empty())
    {
        anyhow::bail!("Profile name cannot be empty");
    }
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating profile: {key}")
    })?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    let current_revision = record.durable_revision();
    if current_revision != expected_revision {
        anyhow::bail!(
            "session revision conflict: expected {expected_revision}, current {current_revision}"
        );
    }
    crate::runtime::claude_accounts::check_profile_change(record, profile.as_deref())
        .map_err(anyhow::Error::new)?;
    record.profile = profile;
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key: key.to_string(),
        session_id: session_id_for_user_output(record, key),
    })
}

pub fn set_cutex_session_display_name_by_key(
    store: &mut CutexSessionStore,
    key: &str,
    display_name: &str,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    let display_name = display_name.trim();
    if display_name.is_empty() {
        anyhow::bail!("Agent name cannot be empty");
    }
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating agent name: {key}")
    })?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    record.display_name_hint = Some(display_name.to_string());
    record.formal_agent_name = Some(display_name.to_string());
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key: key.to_string(),
        session_id: session_id_for_user_output(record, key),
    })
}

pub fn ensure_cutex_session_record_for_user_id(
    store: &mut CutexSessionStore,
    id: &str,
    seed: CutexSessionEnsureSeed,
) -> anyhow::Result<String> {
    if let Some(key) = cutex_session_key_for_user_id_including_retired(store, id) {
        let record = store
            .sessions
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while ensuring: {key}"))?;
        if record.is_retired() {
            anyhow::bail!("cutex session is retired: {id}");
        }
    }
    if let Some(key) = cutex_session_key_for_user_id(store, id) {
        return Ok(key);
    }
    let codex_session_id = normalize_codex_session_id(id)?;
    let key = default_cutex_session_id_for_codex_session(&codex_session_id);
    let record = CutexSessionRecord::new(
        key.clone(),
        Some(codex_session_id),
        seed.host_id,
        seed.cwd,
        seed.profile,
    )?;
    store.sessions.insert(key.clone(), record);
    Ok(key)
}

pub fn adopt_cutex_session(
    store: &mut CutexSessionStore,
    id: &str,
    seed: CutexSessionEnsureSeed,
    options: CutexSessionAdoptOptions<'_>,
) -> anyhow::Result<CutexSessionAdoptOutcome> {
    adopt_cutex_session_from(store, id, seed, options, None)
}

/// [`adopt_cutex_session`] with the native thread's history in
/// `history_home` rather than the selected deployment's native home (see
/// `LocalDeployment::adopt_from`). Without a selected deployment the session
/// stays on the host's shared home, as before.
pub fn adopt_cutex_session_from(
    store: &mut CutexSessionStore,
    id: &str,
    seed: CutexSessionEnsureSeed,
    options: CutexSessionAdoptOptions<'_>,
    history_home: Option<&std::path::Path>,
) -> anyhow::Result<CutexSessionAdoptOutcome> {
    let key = ensure_cutex_session_record_for_user_id(store, id, seed)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while adopting: {key}"))?;
    let newly_managed = record.registration_class != AgentRegistrationClass::Persistent;
    if newly_managed {
        record.runtime_backend = default_managed_session_runtime_backend();
    }
    apply_managed_session_defaults(
        record,
        options.display_name,
        options.managed_cwd,
        options.groups,
        options.expose_to_im,
        options.pin,
    );
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    if newly_managed
        && record.explicit_launch.is_none()
        && crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name(),
        )
    {
        if let Some(deployment) = crate::launch::local_deployment::LocalDeployment::selected()? {
            let mut candidate = record.clone();
            deployment.adopt_from(&mut candidate, store, history_home)?;
            *store
                .sessions
                .get_mut(&key)
                .expect("existing adoption record") = candidate;
        }
    }
    let record = store
        .sessions
        .get_mut(&key)
        .expect("existing adoption record");

    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionAdoptOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
        display_name: cutex_session_display_name(record),
        runtime_backend: record.runtime_backend,
        launch_cwd: cutex_session_launch_cwd(record).to_string(),
        im_visible: record.exposed_to_backend,
        groups: record.agent_groups.clone(),
    })
}

pub fn expose_cutex_session(
    store: &mut CutexSessionStore,
    id: &str,
    seed: CutexSessionEnsureSeed,
    display_name: Option<&str>,
    groups: Vec<String>,
) -> anyhow::Result<CutexSessionGroupsUpdateOutcome> {
    let key = ensure_cutex_session_record_for_user_id(store, id, seed)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while exposing: {key}"))?;
    if record.registration_class != AgentRegistrationClass::Persistent {
        record.runtime_backend = default_managed_session_runtime_backend();
    }
    apply_managed_session_defaults(record, display_name, None, groups, true, false);
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionGroupsUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
        groups: record.agent_groups.clone(),
    })
}

pub fn hide_cutex_session(
    store: &mut CutexSessionStore,
    id: &str,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    let key = require_cutex_session_key_for_user_id(store, id)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while hiding: {key}"))?;
    record.exposed_to_backend = false;
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
    })
}

pub fn unmanage_cutex_session(
    store: &mut CutexSessionStore,
    id: &str,
) -> anyhow::Result<CutexSessionUpdateOutcome> {
    let key = require_cutex_session_key_for_user_id(store, id)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared while unmanaging: {key}"))?;
    record.registration_class = AgentRegistrationClass::LocalOnly;
    record.exposed_to_backend = false;
    record.agent_enabled = false;
    record.managed_cwd = None;
    record.quick_action = CutexSessionQuickActionMode::Auto;
    record.default_cli_args.clear();
    record.permission_defaults = None;
    record.approval_policy = None;
    record.sandbox_mode = None;
    record.model_defaults = None;
    record.reasoning_defaults = None;
    record.service_tier = None;
    record.bump_durable_revision()?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(CutexSessionUpdateOutcome {
        key,
        session_id: session_id_for_user_output(record, id),
    })
}

fn require_cutex_session_key_for_user_id(
    store: &CutexSessionStore,
    id: &str,
) -> anyhow::Result<String> {
    cutex_session_key_for_user_id(store, id)
        .ok_or_else(|| anyhow::anyhow!("cutex session is not known: {id}"))
}

fn session_id_for_user_output(record: &CutexSessionRecord, fallback_id: &str) -> String {
    record
        .codex_session_id
        .clone()
        .unwrap_or_else(|| fallback_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionArchiveState;

    fn retired_store() -> CutexSessionStore {
        let mut record = CutexSessionRecord::new_at(
            "cutex.retired-settings".to_string(),
            Some("thread-retired-settings".to_string()),
            "devbox".to_string(),
            "/tmp/retired-settings".to_string(),
            Some("alpha".to_string()),
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("retired-agent".to_string());
        record.archive_state = CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert("cutex.retired-settings".to_string(), record);
        store
    }

    #[test]
    fn retired_session_rejects_profile_and_display_mutations_without_changes() {
        let mut store = retired_store();
        let original = store.sessions["cutex.retired-settings"].clone();

        assert!(set_cutex_session_profile_by_key(
            &mut store,
            "cutex.retired-settings",
            Some("beta".to_string())
        )
        .is_err());
        assert_eq!(store.sessions["cutex.retired-settings"], original);

        assert!(set_cutex_session_display_name_by_key(
            &mut store,
            "cutex.retired-settings",
            "renamed"
        )
        .is_err());
        assert_eq!(store.sessions["cutex.retired-settings"], original);
    }

    #[test]
    fn profile_mutation_fences_stale_revision_without_changing_durable_intent() {
        let mut store = CutexSessionStore::default();
        let record = CutexSessionRecord::new_at(
            "cutex-profile-fence".to_string(),
            Some("thread-profile-fence".to_string()),
            "devbox".to_string(),
            "/tmp/profile-fence".to_string(),
            Some("alpha".to_string()),
            "2026-08-15T00:00:00Z".to_string(),
        )
        .expect("record");
        store
            .sessions
            .insert("cutex-profile-fence".to_string(), record);
        let original = store.sessions["cutex-profile-fence"].clone();
        assert!(set_cutex_session_profile_by_key_with_expected_revision(
            &mut store,
            "cutex-profile-fence",
            None,
            original.durable_revision() + 1,
        )
        .is_err());
        assert_eq!(store.sessions["cutex-profile-fence"], original);
        set_cutex_session_profile_by_key_with_expected_revision(
            &mut store,
            "cutex-profile-fence",
            None,
            original.durable_revision(),
        )
        .expect("clear configured profile");
        assert_eq!(store.sessions["cutex-profile-fence"].profile, None);
    }
}
