//! Test-only isolated fixture for the runtime memory switch contract.
use crate::session::model::CutexSessionRecord;
use crate::agent_management::{AgentManagementStore, ProjectAuthority, ProjectId};
use crate::role_revision::{CutexSessionId, Rfc3339};
use std::path::PathBuf;

pub(crate) struct Fixture {
    pub record: CutexSessionRecord,
    pub cwd: PathBuf,
    pub memory_dir: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cwd);
        let _ = std::fs::remove_file(&self.memory_dir);
        let _ = std::fs::remove_dir_all(&self.memory_dir);
    }
}
pub(crate) fn disabled() -> Fixture {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let cwd = std::env::temp_dir().join(format!("memory-switch-{unique}"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "Keep this project instruction.").unwrap();
    let mut record = CutexSessionRecord::new(format!("cutex.{}", uuid::Uuid::new_v4()), None,
        "test".into(), cwd.to_string_lossy().into_owned(), None).unwrap();
    record.memory_injection = false;
    let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
    sessions.sessions.insert(record.cutex_session_id.clone(), record.clone());
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let project = ProjectId::new(format!("memory-switch-{unique}")).unwrap();
    AgentManagementStore::open_default().unwrap().with_state(true, |mut state| {
        state.projects.insert(project.clone(), ProjectAuthority {
            project_id: project.clone(),
            authorized_director_session: CutexSessionId::new(record.cutex_session_id.clone()).unwrap(),
            authority_epoch: 1,
            updated_at: Rfc3339::new("2026-11-09T00:00:00Z").unwrap(),
        });
        Ok((state, (), true))
    }).unwrap();
    let memory_dir = super::project_memory_dir(&project).unwrap();
    // A file where the memory directory belongs fails any enabled launch.
    // Disabled launch must resolve the path without creating or reading it.
    std::fs::create_dir_all(memory_dir.parent().unwrap()).unwrap();
    std::fs::write(&memory_dir,"memory path blocked for the test").unwrap();
    Fixture { record, cwd, memory_dir }
}
pub(crate) fn assert_disabled(text: &str) {
    assert_eq!(text.matches("Project memory is off for this agent;").count(), 1, "{text}");
    assert!(text.contains("unless the owner personally and explicitly asks."));
    assert!(!text.contains("## Current MEMORY.md"));
    assert!(!text.contains("How to save a memory"));
    assert!(!text.contains("Before relying on memory, read the full index"));
}
