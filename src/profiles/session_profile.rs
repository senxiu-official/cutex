//! Read-only Profile selection for tool policy; never invent runtime evidence.
use super::model::{AccountsStore, StoredAccount};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

fn unique<'a>(store: &'a AccountsStore, matches: impl Fn(&StoredAccount) -> bool) -> Option<&'a StoredAccount> {
    let mut rows = store.accounts.iter().filter(|row| matches(row));
    let row = rows.next()?;
    rows.next().is_none().then_some(row)
}

fn selected<'a>(store: &'a AccountsStore, profile: &str) -> Option<&'a StoredAccount> {
    if store.accounts.iter().any(|row| row.name == profile) {
        unique(store, |row| row.name == profile)
    } else {
        unique(store, |row| row.id == profile)
    }
}

fn inherits_launch_default(record: &CutexSessionRecord) -> bool {
    matches!(record.runtime_backend, CutexSessionRuntimeBackend::Host | CutexSessionRuntimeBackend::HostForeground)
        && record.app_server_runtime.is_none() && record.current_runtime_agent_id.is_none() && record.runtime_pid.is_none()
}

/// Running occurrence evidence wins over configured intent. A missing launched
/// identity never falls through to a different Profile or today's default.
/// With no runtime binding, native launch selection is configured Profile, else
/// config.default_profile (the same precedence as stock configuration).
fn effective_in<'a>(record: &CutexSessionRecord, store: &'a AccountsStore, default_profile: Option<&str>) -> Option<&'a StoredAccount> {
    if record.is_retired() { return None; }
    if let Some(binding) = &record.app_server_runtime {
        if let Some(id) = binding.launched_profile_id.as_deref() {
            return unique(store, |row| row.id == id);
        }
        // Name-only occurrences require the history-aware path in effective.
        return None;
    }
    if let Some(profile) = record.profile.as_deref() {
        // Names take precedence in the launch selector, then stable IDs.
        return selected(store, profile);
    }
    if !inherits_launch_default(record) {
        return None;
    }
    let profile = default_profile?;
    selected(store, profile)
}

pub fn effective<'a>(record: &CutexSessionRecord, store: &'a AccountsStore) -> Option<&'a StoredAccount> {
    if record.is_retired() { return None; }
    if let Some(binding) = record.app_server_runtime.as_ref().filter(|binding| binding.launched_profile_id.is_none()) {
        let name=binding.launched_profile.as_deref()?;
        let at=chrono::DateTime::parse_from_rfc3339(&binding.started_at).ok()?.with_timezone(&chrono::Utc);
        let history=match std::fs::read(super::identity::history_path().ok()?) {
            Ok(bytes)=>serde_json::from_slice::<super::identity::NameHistory>(&bytes).ok()?,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>super::identity::NameHistory {version:1,renames:Vec::new()},
            Err(_)=>return None,
        };
        if history.version!=1 || history.renames.iter().any(|entry|chrono::DateTime::parse_from_rfc3339(&entry.renamed_at).is_err()) {return None;}
        // Use the same occurrence-time rename attribution as the runtime binding;
        // a newly reused name must never opt in the old occurrence's tools.
        let directory=super::identity::ProfileNameDirectory::from_parts(store,&history,&[]);
        let id=directory.resolve(None,Some(name),Some(at))?.id?;
        return unique(store,|row|row.id==id);
    }
    // Avoid depending on global settings when an occurrence/configured identity
    // is already present. A plain Claude session never inherits a Codex default.
    let default = if record.profile.is_none() && inherits_launch_default(record) {
        crate::config::store::load_codez_config_checked().ok()?.default_profile
    } else { None };
    effective_in(record, store, default.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new_at("cutex.profile-tools".into(), None, "host".into(), "/tmp".into(), None, "2026-10-24T00:00:00Z".into()).unwrap()
    }
    fn profiles() -> AccountsStore {
        serde_json::from_value(json!({"version":crate::profiles::model::STORE_VERSION,"active_account_id":"other-id",
            "accounts":[{"id":"on-id","name":"Enabled"},{"id":"other-id","name":"Other"}]})).unwrap()
    }
    fn binding(id: Option<&str>, name: Option<&str>) -> crate::session::model::CutexAppServerRuntimeBinding {
        serde_json::from_value(json!({"transport":"loopback_web_socket","endpoint":"ws://127.0.0.1:1","pid":1,
            "runtime_dir":"/tmp","launched_profile_id":id,"launched_profile":name,
            "diagnostic_journal_path":"/tmp/log","schema_version":"test","schema_sha256":"a".repeat(64),"started_at":"2026-10-24T00:00:00Z"})).unwrap()
    }
    #[test]
    fn effective_profile_default_is_launch_default_not_active_account() {
        let store=profiles(); let mut record=record();
        assert_eq!(effective_in(&record,&store,Some("Enabled")).unwrap().id,"on-id");
        assert_eq!(effective_in(&record,&store,Some("on-id")).unwrap().id,"on-id");
        assert!(effective_in(&record,&store,None).is_none());
        assert!(effective_in(&record,&store,Some("Missing")).is_none());
        record.current_runtime_agent_id=Some("unknown-live-runtime".into());
        assert!(effective_in(&record,&store,Some("Enabled")).is_none(),"an unidentified running occurrence cannot inherit today's default");
        record.current_runtime_agent_id=None;
        record.profile=Some("Other".into());
        assert_eq!(effective_in(&record,&store,Some("Enabled")).unwrap().id,"other-id");
        record.profile=Some("Missing".into());
        assert!(effective_in(&record,&store,Some("Enabled")).is_none());
    }
    #[test]
    fn effective_profile_occurrence_identity_wins_and_missing_never_falls_back() {
        let mut store=profiles(); let mut record=record(); record.profile=Some("Other".into());
        record.app_server_runtime=Some(binding(Some("on-id"),Some("old-name")));
        store.accounts[0].name="Renamed".into();
        assert_eq!(effective_in(&record,&store,Some("Other")).unwrap().name,"Renamed");
        record.app_server_runtime=Some(binding(Some("deleted-id"),Some("Other")));
        assert!(effective_in(&record,&store,Some("Other")).is_none());
        record.app_server_runtime=Some(binding(None,Some("Renamed")));
        assert!(effective_in(&record,&store,None).is_none(),"name-only evidence requires occurrence-time history resolution");
        record.app_server_runtime=Some(binding(None,None));
        assert!(effective_in(&record,&store,Some("Other")).is_none());
    }
    #[test]
    fn effective_profile_retired_and_plain_claude_do_not_inherit_native_default() {
        let store=profiles(); let mut record=record();
        record.runtime_backend=CutexSessionRuntimeBackend::ClaudeSubscription;
        assert!(effective_in(&record,&store,Some("Enabled")).is_none());
        record.profile=Some("Enabled".into());
        assert!(effective_in(&record,&store,None).is_some());
        record.archive_state=crate::session::model::CutexSessionArchiveState::Retired;
        assert!(effective_in(&record,&store,Some("Enabled")).is_none());
    }
    #[test]
    fn effective_profile_legacy_name_uses_launch_time_history_not_reused_name() {
        use crate::config::instance::{set_test_selection,Selection};
        let previous=crate::config::instance::selected().unwrap();
        let root=std::env::temp_dir().join(format!("tool-profile-history-{}",uuid::Uuid::new_v4()));
        set_test_selection(Selection::Custom(root.clone()));std::fs::create_dir_all(&root).unwrap();
        let mut store=profiles();store.accounts[1].name="Old".into();
        let mut record=record();record.app_server_runtime=Some(binding(None,Some("Old")));
        let path=super::super::identity::history_path().unwrap();
        std::fs::write(&path,serde_json::to_vec(&json!({"version":1,"renames":[{
            "profileId":"on-id","from":"Old","to":"Enabled","renamedAt":"2026-10-25T00:00:00Z"
        }]})).unwrap()).unwrap();
        assert_eq!(effective(&record,&store).unwrap().id,"on-id");
        store.accounts.remove(0);
        assert!(effective(&record,&store).is_none(),"deleted original must not fall through to reused name");
        std::fs::write(&path,b"corrupt").unwrap();
        assert!(effective(&record,&store).is_none());
        set_test_selection(previous);let _=std::fs::remove_dir_all(root);
    }
}
