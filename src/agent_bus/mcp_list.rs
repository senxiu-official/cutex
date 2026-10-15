//! The agent rows the Bus answers on its fenced agent list (`/api/agents`): existing Bus
//! visibility plus the authoritative durable mapping. Alarms resolve names against it.
use crate::agent_bus::{model::AgentBusAgent, routing::project_current_durable_session_ids};
use crate::session::model::CutexSessionStore;
use serde_json::{json, Value};

/// The registration's profile label is the launch-time name; when the record's
/// bound runtime is this registration, show that binding's current name (a
/// live rename relabels it; resolved by profile id).
fn observed_profile(agent: &AgentBusAgent, record: Option<&crate::session::model::CutexSessionRecord>) -> String {
    record
        .and_then(|r| r.app_server_runtime.as_ref())
        .filter(|binding| binding.pid == agent.pid)
        .and_then(|binding| binding.current_launched_profile())
        .unwrap_or_else(|| agent.profile.clone())
}

pub(crate) fn project(
    requester: &str,
    mut agents: Vec<AgentBusAgent>,
    sessions: &CutexSessionStore,
) -> Value {
    project_current_durable_session_ids(&mut agents, sessions);
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now.saturating_sub(crate::agent_bus::store::AGENT_BUS_STALE_HEARTBEAT_SECS);
    let rows:Vec<_>=agents.iter().map(|a| {
        let record=a.cutex_session_id.as_ref().and_then(|id|sessions.sessions.get(id)).filter(|r| {
            a.cutex_session_id.as_ref()==Some(&r.cutex_session_id)
                && r.app_server_thread_id().is_some()
                && sessions.sessions.values().filter(|other|other.app_server_thread_id()==r.app_server_thread_id()).count()==1
                && crate::agent_bus::routing::is_full_durable_cutex_session_id(&r.cutex_session_id)
        });
        let name=record.and_then(|r|r.formal_agent_name.as_deref()).filter(|s|!s.trim().is_empty());
        let current=a.last_seen_epoch_secs>=cutoff && crate::platform::process::process_is_running(a.pid);
        json!({"runtime_agent_id":a.id,"cutex_session_id":record.map(|r|&r.cutex_session_id),"native_session_id":a.session_id,"formal_name":name,
            "supported_delivery_modes":if record.is_some_and(|r|r.claude_session.is_some()){json!(["after_turn"])}else{json!(["after_turn","soon","passive"])},
            "receipt_scope":if record.is_some_and(|r|r.claude_session.is_some()){"local_inbox"}else{"native_context"},
            "mapping_observation":if record.is_some(){"current"}else{"unavailable_or_ambiguous"},
            "name_observation":if name.is_some(){"durable_formal_name"}else{"unavailable"},
            "runtime_observation":if current {"registered_online"}else{"stale_or_unavailable"},
            "runtime_generation":record.map(|r|r.runtime_generation),"configured_profile":record.and_then(|r|r.profile.as_deref()),
            "observed_runtime_profile":observed_profile(a, record),"cwd":a.cwd,"groups":a.groups,"registration_class":a.registration_class,
            "last_seen_epoch_secs":a.last_seen_epoch_secs,"this":a.id==requester})
    }).collect();
    json!({"ok":true,"current_agent_id":requester,"scope":"local_group_visible","agents":rows})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_projection_preserves_formal_name_and_rejects_duplicate_thread() {
        use crate::session::model::CutexSessionRecord;
        let id = "cutex.018e0000-0000-7000-8000-0000a000037e";
        let thread = "018e0000-0000-7000-8000-0000a000037f";
        let mut record = CutexSessionRecord::new_at(id.into(), None, "host-a".into(),
            "/fixture".into(), None, "2026-09-17T00:00:00Z".into()).unwrap();
        record.claude_session = Some(serde_json::from_value(json!({
            "deployment":{"python":"/fixture/python","adapter_source":"/fixture/adapter",
                "claude_cli":"/fixture/claude","claude_home":"/fixture/home","tui_binary":"/fixture/tui"},
            "thread_id":thread,"state_dir":"/fixture/state","model":"sonnet"
        })).unwrap());
        record.current_runtime_agent_id = Some("claude.connection".into());
        record.runtime_pid = Some(std::process::id());
        record.runtime_generation = 1;
        record.agent_enabled = true;
        record.formal_agent_name = Some("Claude Reviewer".into());
        let agent: AgentBusAgent = serde_json::from_value(json!({
            "id":"claude.connection","name":"display","session_id":thread,"profile":"Claude",
            "cwd":"/fixture","pid":std::process::id(),"host_id":"host-a","groups":[],
            "registration_class":"persistent","last_seen_epoch_secs":0
        })).unwrap();
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(id.into(), record.clone());
        let row = project("claude.connection", vec![agent.clone()], &sessions)["agents"][0].clone();
        assert_eq!(row["formal_name"], "Claude Reviewer");
        assert_eq!(row["cutex_session_id"], id);
        sessions.sessions.insert("duplicate-thread".into(), record);
        let row = project("claude.connection", vec![agent], &sessions)["agents"][0].clone();
        assert!(row["formal_name"].is_null());
        assert!(row["cutex_session_id"].is_null());
    }

    #[test]
    fn projection_uses_exact_current_mapping_and_never_name_title_or_profile_identity() {
        use crate::session::model::CutexSessionRecord;
        let id = "cutex.018e0000-0000-7000-8000-0000a000037e";
        let native = "018e0000-0000-7000-8000-0000a000037e";
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            Some(native.into()),
            "host-a".into(),
            "/private".into(),
            None,
            "2026-09-09T00:00:00Z".into(),
        )
        .unwrap();
        record.current_runtime_agent_id = Some("owned-runtime".into());
        record.runtime_generation = 3;
        record.agent_enabled = true;
        record.formal_agent_name = Some("Formal".into());
        let agent:AgentBusAgent=serde_json::from_value(json!({"id":"owned-runtime","name":"NOT FORMAL","thread_name":"TITLE NOT NAME",
            "session_id":native,"profile":"mutable","cwd":"/private","pid":std::process::id(),"host_id":"host-a",
            "groups":["private"],"registration_class":"persistent","last_seen_epoch_secs":0})).unwrap();
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(id.into(), record.clone());
        let row = project("owned-runtime", vec![agent.clone()], &sessions)["agents"][0].clone();
        assert_eq!(row["cutex_session_id"], id);
        assert_eq!(row["formal_name"], "Formal");
        assert_eq!(row["runtime_observation"], "stale_or_unavailable");
        record.formal_agent_name = None;
        record.profile = Some("changed-profile".into());
        sessions.sessions.insert(id.into(), record.clone());
        let row = project("owned-runtime", vec![agent.clone()], &sessions)["agents"][0].clone();
        assert_eq!(row["cutex_session_id"], id);
        assert!(row["formal_name"].is_null());
        assert_eq!(row["name_observation"], "unavailable");
        sessions.sessions.insert("duplicate-native".into(), record);
        let row = project("owned-runtime", vec![agent.clone()], &sessions)["agents"][0].clone();
        assert!(row["cutex_session_id"].is_null());
        assert_eq!(row["mapping_observation"], "unavailable_or_ambiguous");
        assert!(
            project("owned-runtime", vec![agent], &CutexSessionStore::default())["agents"][0]
                ["cutex_session_id"]
                .is_null()
        );
    }
}
