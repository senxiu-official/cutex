//! Foreground-only Agent label and exact project membership; never affects identity or prompts.
use crate::session::model::CutexSessionRecord;

pub fn for_record(record: &CutexSessionRecord) -> String {
    let snapshot = crate::agent_management::AgentManagementStore::open_default()
        .and_then(|s| s.snapshot().map_err(anyhow::Error::new)).ok();
    project(record, snapshot.as_ref()).to_string()
}
fn project(record: &CutexSessionRecord, snapshot: Option<&crate::agent_management::AgentManagementSnapshot>) -> serde_json::Value {
    if !record.agent_enabled { return serde_json::json!({}); }
    let agent = snapshot.and_then(|s| s.agents.values().find(|a| a.cutex_session_id.as_str() == record.cutex_session_id));
    let name = record.formal_agent_name.as_deref().filter(|n| !n.is_empty())
        .or_else(|| agent.map(|a| a.spec.name.as_str()));
    let Some(name) = name else { return serde_json::json!({}); };
    let badge = snapshot.zip(agent).and_then(|(s,a)| {
        let id = crate::agent_management::current_project_id(s,a)?;
        let p = crate::agent_management::effective_presentation(&id,s.project_presentations.get(&id));
        Some(serde_json::json!({"label":p.badge_label,"color":p.color.token()}))
    });
    serde_json::json!({"name":name,"badge":badge})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_named_agents_are_shown_without_guessing_project_from_cwd() {
        let mut record=CutexSessionRecord::new(format!("cutex.{}",uuid::Uuid::new_v4()),None,"local".into(),"/Projects/IFM".into(),None).unwrap();
        record.agent_enabled=true;
        assert_eq!(project(&record,None),serde_json::json!({}));
        record.formal_agent_name=Some("director".into());
        assert_eq!(project(&record,None),serde_json::json!({"name":"director","badge":null}));
        record.agent_enabled=false;
        assert_eq!(project(&record,None),serde_json::json!({}));
    }
}
