//! Typed routing mutations for durable `cutex_session` records.

use crate::agent_bus::identity::normalize_agent_groups;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::model::CutexSessionQuickActionMode;
use crate::session::model::CutexSessionStore;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CutexSessionRoutingPatch {
    pub agent_groups: Option<Vec<String>>,
    pub exposed_to_backend: Option<bool>,
    pub quick_action: Option<CutexSessionQuickActionMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionRoutingUpdateOutcome {
    pub key: String,
    pub session_id: String,
    pub groups: Vec<String>,
    pub groups_changed: bool,
}

pub fn update_cutex_session_routing_by_key(
    store: &mut CutexSessionStore,
    key: &str,
    fallback_id: &str,
    patch: CutexSessionRoutingPatch,
) -> anyhow::Result<CutexSessionRoutingUpdateOutcome> {
    let groups = patch.agent_groups.map(normalize_agent_groups);
    if groups.as_ref().is_some_and(Vec::is_empty) {
        anyhow::bail!("At least one non-empty group is required");
    }
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while updating routing: {key}")
    })?;
    if crate::agent_management::agent_execution_blocked(&record) {
        anyhow::bail!("cutex session is retired: {key}");
    }
    if patch.exposed_to_backend == Some(true)
        && record.registration_class != AgentRegistrationClass::Persistent
    {
        anyhow::bail!("Adopt the cutex session before exposing it to the workbench");
    }

    let groups_changed = groups
        .as_ref()
        .is_some_and(|groups| *groups != record.agent_groups);
    let changed = groups_changed
        || patch
            .exposed_to_backend
            .is_some_and(|visible| visible != record.exposed_to_backend)
        || patch
            .quick_action
            .is_some_and(|mode| mode != record.quick_action);
    if let Some(groups) = groups {
        record.agent_groups = groups;
    }
    if let Some(visible) = patch.exposed_to_backend {
        record.exposed_to_backend = visible;
    }
    if let Some(mode) = patch.quick_action {
        record.quick_action = mode;
    }
    if changed {
        record.bump_durable_revision()?;
        record.updated_at = chrono::Utc::now().to_rfc3339();
    }

    Ok(CutexSessionRoutingUpdateOutcome {
        key: key.to_string(),
        session_id: record
            .codex_session_id
            .clone()
            .unwrap_or_else(|| fallback_id.to_string()),
        groups: record.agent_groups.clone(),
        groups_changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::model::CutexSessionRecord;

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.routing".to_string(),
            Some("019e-routing".to_string()),
            "devbox".to_string(),
            "/tmp/routing".to_string(),
            Some("alpha".to_string()),
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("record")
    }

    #[test]
    fn routing_patch_updates_all_fields_once() {
        let mut record = record();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.agent_groups = vec!["cutex".to_string()];
        let mut store = CutexSessionStore::default();
        store.sessions.insert("cutex.routing".to_string(), record);

        let outcome = update_cutex_session_routing_by_key(
            &mut store,
            "cutex.routing",
            "fallback",
            CutexSessionRoutingPatch {
                agent_groups: Some(vec![
                    " waveline ".to_string(),
                    "cutex".to_string(),
                    "waveline".to_string(),
                ]),
                exposed_to_backend: Some(true),
                quick_action: Some(CutexSessionQuickActionMode::Pinned),
            },
        )
        .expect("routing update");

        assert_eq!(outcome.session_id, "019e-routing");
        assert_eq!(outcome.groups, ["waveline", "cutex"]);
        assert!(outcome.groups_changed);
        let record = &store.sessions["cutex.routing"];
        assert!(record.exposed_to_backend);
        assert_eq!(record.quick_action, CutexSessionQuickActionMode::Pinned);
        assert_ne!(record.updated_at, "2026-08-06T00:00:00Z");
    }

    #[test]
    fn exposing_local_session_is_rejected_without_mutation() {
        let record = record();
        let original = record.clone();
        let mut store = CutexSessionStore::default();
        store.sessions.insert("cutex.routing".to_string(), record);

        let error = update_cutex_session_routing_by_key(
            &mut store,
            "cutex.routing",
            "fallback",
            CutexSessionRoutingPatch {
                exposed_to_backend: Some(true),
                ..CutexSessionRoutingPatch::default()
            },
        )
        .expect_err("local exposure must fail");

        assert!(error.to_string().contains("Adopt"));
        assert_eq!(store.sessions["cutex.routing"], original);
    }

    #[test]
    fn retired_session_rejects_routing_patch_without_mutation() {
        let mut record = record();
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        let original = record.clone();
        let mut store = CutexSessionStore::default();
        store.sessions.insert("cutex.routing".to_string(), record);

        let error = update_cutex_session_routing_by_key(
            &mut store,
            "cutex.routing",
            "fallback",
            CutexSessionRoutingPatch {
                agent_groups: Some(vec!["waveline".to_string()]),
                ..CutexSessionRoutingPatch::default()
            },
        )
        .expect_err("retired routing mutation must fail");

        assert!(error.to_string().contains("is retired"));
        assert_eq!(store.sessions["cutex.routing"], original);
    }
}
