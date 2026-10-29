//! One fail-closed Session projection for active, soft and permanent records.
use crate::agent_management::{agent_archive_disposition, AgentArchiveKind, AgentManagementSnapshot};
use crate::session::model::{CutexSessionRecord, CutexSessionStore};
use serde_json::{json, Value};

pub fn fields(record: &CutexSessionRecord, authority: Option<&AgentManagementSnapshot>, sessions: Option<&CutexSessionStore>) -> Value {
    let unknown = || json!({"archiveKind":"unknown","restorable":false,
        "restoreBlockers":["archive_state_unavailable"],"allowedActions":[]});
    let Some(state) = authority else { return unknown(); };
    let Ok(disposition) = agent_archive_disposition(state, record) else { return unknown(); };
    let mut blockers = Vec::<&str>::new();
    let mut actions = Vec::<&str>::new();
    match disposition.archive_kind {
        AgentArchiveKind::Permanent => blockers.push("permanently_closed"),
        AgentArchiveKind::Active => {
            blockers.push("not_archived");
            actions.extend(["start", "send"]);
            if !state.projects.values().any(|p| p.authorized_director_session.as_str() == record.cutex_session_id) {
                actions.extend(["archive", "permanentRetire"]);
            }
        }
        AgentArchiveKind::Soft => {
            if crate::session::archive::record_has_runtime_claim(record) { blockers.push("runtime_not_offline"); }
            if let Some(sessions) = sessions {
                if crate::session::metadata::live_formal_name_holder(sessions, record).is_some() { blockers.push("agent_name_taken"); }
            } else { blockers.push("archive_state_unavailable"); }
            if let Some(agent) = state.agents.values().find(|a| a.cutex_session_id.as_str() == record.cutex_session_id) {
                let project = crate::agent_management::current_project_id(state, agent);
                if project.as_ref().is_some_and(|p| !state.projects.contains_key(p) || state.project_tombstones.contains_key(p)
                    || state.project_states.get(p).is_some_and(|s| s.lifecycle != crate::agent_management::ProjectLifecycle::Active)) {
                    blockers.push("restore_project_first");
                }
            }
            if blockers.is_empty() { actions.push("restore"); }
            actions.push("permanentRetire");
        }
    }
    json!({"archiveKind":disposition.archive_kind,"restorable":disposition.restorable,
        "restoreBlockers":blockers,"allowedActions":actions})
}

/// Presentation-only request cache. Never use this for execute authorization.
#[derive(Default)]
pub struct Batch {
    inputs: std::cell::OnceCell<(Option<AgentManagementSnapshot>, Option<std::sync::Arc<CutexSessionStore>>)>,
}
impl Batch {
    pub fn annotate(&self, value: &mut Value, record: &CutexSessionRecord) {
        self.annotate_with(value, record, || (
            crate::agent_management::archive_authority_snapshot().ok(),
            crate::session::store::load_cutex_session_store_shared().ok(),
        ));
    }
    fn annotate_with(&self, value: &mut Value, record: &CutexSessionRecord,
        load: impl FnOnce() -> (Option<AgentManagementSnapshot>, Option<std::sync::Arc<CutexSessionStore>>)) {
        let (authority, sessions) = self.inputs.get_or_init(load);
        apply_fields(value, fields(record, authority.as_ref(), sessions.as_deref()));
    }
}

pub fn annotate(value: &mut Value, record: &CutexSessionRecord) {
    Batch::default().annotate(value, record);
}

fn apply_fields(value: &mut Value, extra: Value) {
    if extra["archiveKind"] == "permanent" {
        value["lifecycle"] = json!("retired");
        if value["runtime"].is_object() { value["runtime"]["status"] = json!("offline"); }
    }
    if let (Some(object), Some(extra)) = (value.as_object_mut(),extra.as_object()) {
        object.extend(extra.iter().map(|(k,v)| (k.clone(),v.clone())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new_at("cutex.projection".into(),Some("thread".into()),"host".into(),"cwd".into(),None,"2026-10-26T00:00:00Z".into()).unwrap()
    }
    #[test]
    fn batch_loads_once_and_keeps_failed_authority_fail_closed() {
        let loads = std::cell::Cell::new(0);
        let batch = Batch::default();
        for _ in 0..3 {
            let mut value = json!({});
            batch.annotate_with(&mut value, &record(), || {
                loads.set(loads.get()+1);
                (None, Some(std::sync::Arc::new(CutexSessionStore::default())))
            });
            assert_eq!(value["archiveKind"], "unknown");
            assert_eq!(value["allowedActions"], json!([]));
        }
        assert_eq!(loads.get(), 1);
        let mut value=json!({});
        Batch::default().annotate_with(&mut value, &record(), || {
            loads.set(loads.get()+1);
            (Some(AgentManagementSnapshot::empty()), None)
        });
        assert_eq!(loads.get(), 2);
        assert_eq!(value["archiveKind"], "active");
    }
    #[test]
    fn unknown_is_not_soft_and_offers_no_actions() {
        let value=fields(&record(),None,None);
        assert_eq!(value["archiveKind"],"unknown");
        assert_eq!(value["restorable"],false);
        assert_eq!(value["allowedActions"],json!([]));
    }
    #[test]
    fn soft_requires_offline_state_and_name_availability() {
        let mut record=record();
        record.archive_state=crate::session::model::CutexSessionArchiveState::Retired;
        let state=AgentManagementSnapshot::empty();
        let sessions=CutexSessionStore::default();
        let value=fields(&record,Some(&state),Some(&sessions));
        assert_eq!(value["archiveKind"],"soft");
        assert!(value["allowedActions"].as_array().unwrap().contains(&json!("restore")));
        assert!(!fields(&record,Some(&state),None)["allowedActions"].as_array().unwrap().contains(&json!("restore")));
    }
}
