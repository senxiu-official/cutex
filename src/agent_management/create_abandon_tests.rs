//! The two host-a cases (10-22) as fixtures: `waveline-news` stopped at
//! `configured` with a successor session and native thread; `waveline-news-glm`
//! stopped at `native_session_captured`. Fake cleanup only.
use std::cell::RefCell;

use super::*;
use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

struct Fake {
    sessions: Vec<String>,
    calls: RefCell<Vec<CreateAbandonPlan>>,
    fail: bool,
}

impl CreateAbandonCleanup for Fake {
    fn durable_session_exists(&self, id: &str) -> bool {
        self.sessions.iter().any(|s| s == id)
    }
    fn cleanup(&self, plan: &CreateAbandonPlan) -> Result<Vec<String>, String> {
        self.calls.borrow_mut().push(plan.clone());
        if self.fail {
            return Err("native history busy".into());
        }
        let mut removed = Vec::new();
        if let Some(id) = &plan.durable_session {
            removed.push(format!("deleted durable session {id}"));
        } else if let Some(thread) = &plan.native_thread {
            removed.push(format!("moved native thread {thread} aside"));
        }
        Ok(removed)
    }
}

fn fake(sessions: &[&str]) -> Fake {
    Fake { sessions: sessions.iter().map(|s| s.to_string()).collect(), calls: RefCell::new(Vec::new()), fail: false }
}

fn provider() -> (AgentManagementProvider, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("create-abandon-{}", uuid::Uuid::new_v4()));
    (AgentManagementProvider::open(&root).unwrap(), root)
}

fn seed(provider: &AgentManagementProvider, id: &str, name: &str, phase: AgentActionPhase, successor: Option<&str>, native: &str, operation: AgentOperationKind) {
    let at = Rfc3339::new("2026-10-21T17:34:40Z").unwrap();
    provider
        .store()
        .with_state(true, |mut state| {
            let successor = successor.map(|s| CutexSessionId::new(s.to_string()).unwrap());
            if let Some(successor) = &successor {
                let spec: ManagedAgentSpec = serde_json::from_value(serde_json::json!({"name":name,"cwd":"D:\\Work\\wl-news","profile":"Zooey",
                    "runtime_backend":"host","model":"glm-5.3","reasoning":"default","permissions":"danger-full-access",
                    "sandbox_mode":"danger-full-access","approval_policy":"never","groups":[]})).unwrap();
                state.agents.insert(successor.clone(), ManagedAgentRecord {
                    project_id: Some(ProjectId::new("cutex-desktop-side").unwrap()),
                    created_by_director_session: None,
                    created_by_operator_session: None,
                    cutex_session_id: successor.clone(),
                    native_session_id: native.into(),
                    spec,
                    created_at: Rfc3339::new("2026-10-21T17:34:40Z").unwrap(),
                    retired_at: None,
                });
            }
            state.actions.insert(
                AgentActionId::new(id.to_string()).unwrap(),
                AgentActionRecord {
                    created_by: None,
                    action_id: AgentActionId::new(id.to_string()).unwrap(),
                    request_sha256: Sha256::new("a".repeat(64)).unwrap(),
                    operation,
                    project_id: ProjectId::new("cutex-desktop-side").unwrap(),
                    caller_cutex_session: CutexSessionId::new("cutex.1ee3ecf9-502f-4344-938a-a6d6acb76799".to_string()).unwrap(),
                    phase,
                    phase_sequence: 5,
                    reserved_agent_name: Some(name.into()),
                    reserved_agent_cwd: Some("D:\\Work\\wl-news".into()),
                    known_successor_cutex_session: successor,
                    known_native_session_id: Some(native.into()),
                    native_bootstrap_retryable: false,
                    historical_runtime_occurrence_fence: None,
                    external_message_id: None,
                    response: None,
                    created_at: at.clone(),
                    updated_at: at,
                },
            );
            Ok((state, (), true))
        })
        .unwrap();
}

const NEWS: &str = "auto-5ae2f95202c9b1ab5a5087d0596eb256";
const GLM: &str = "auto-87e029fcf075de20b1857380ff969c04";
const SUCCESSOR: &str = "cutex.01815f87-7707-77b2-9087-1e59fc5fe83f";

fn holds_name(provider: &AgentManagementProvider, name: &str) -> bool {
    provider.store().snapshot().unwrap().actions.values().any(|a| {
        !matches!(a.phase, AgentActionPhase::Complete | AgentActionPhase::NoWrite) && a.reserved_agent_name.as_deref() == Some(name)
    })
}

#[test]
fn configured_create_with_a_session_is_abandoned_and_its_name_released() {
    let (provider, root) = provider();
    seed(&provider, NEWS, "waveline-news", AgentActionPhase::Configured, Some(SUCCESSOR), "01815f87-7707-77b2-9087-1e59fc5fe83f", AgentOperationKind::Create);
    let cleanup = fake(&[SUCCESSOR]);
    // Dry run: the plan, nothing changes.
    let plan = provider.abandon_create(NEWS, true, &cleanup).unwrap();
    assert!(plan.dry_run && plan.removed.is_empty());
    assert_eq!(plan.plan.durable_session.as_deref(), Some(SUCCESSOR));
    assert_eq!(plan.plan.phase, "configured");
    assert!(cleanup.calls.borrow().is_empty());
    assert!(holds_name(&provider, "waveline-news"));
    // Abandon.
    let receipt = provider.abandon_create(NEWS, false, &cleanup).unwrap();
    assert_eq!(receipt.removed, [format!("deleted durable session {SUCCESSOR}")]);
    assert!(!holds_name(&provider, "waveline-news"));
    let state = provider.store().snapshot().unwrap();
    let action = &state.actions[&AgentActionId::new(NEWS.to_string()).unwrap()];
    assert_eq!(action.phase, AgentActionPhase::NoWrite);
    assert!(matches!(&action.response, Some(AgentManagementResponse { outcome: AgentManagementOutcome::NoWrite { code, .. }, .. }) if code == CREATE_ABANDONED));
    assert!(!state.agents.contains_key(&CutexSessionId::new(SUCCESSOR.to_string()).unwrap()));
    assert!(state.phase_events.values().any(|e| e.action_id.as_str() == NEWS && e.phase == AgentActionPhase::NoWrite));
    assert!(root.join("abandons").join(format!("{NEWS}.json")).is_file(), "audit receipt");
    // Idempotent: the same receipt, no second cleanup.
    let again = provider.abandon_create(NEWS, false, &cleanup).unwrap();
    assert!(again.replayed);
    assert_eq!(again.removed, receipt.removed);
    assert_eq!(cleanup.calls.borrow().len(), 1);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_captured_native_thread_without_a_session_is_moved_aside() {
    let (provider, root) = provider();
    seed(&provider, GLM, "waveline-news-glm", AgentActionPhase::NativeSessionCaptured, None, "01815f98-6282-7b73-afbf-72fcf081cad2", AgentOperationKind::Create);
    let cleanup = fake(&[]);
    let receipt = provider.abandon_create(GLM, false, &cleanup).unwrap();
    assert_eq!(receipt.plan.durable_session, None);
    assert_eq!(receipt.removed, ["moved native thread 01815f98-6282-7b73-afbf-72fcf081cad2 aside"]);
    assert!(!holds_name(&provider, "waveline-news-glm"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_missing_session_record_is_not_cleaned_as_a_session() {
    // host-a waveline-news: the action names a successor whose record is gone.
    let (provider, root) = provider();
    seed(&provider, NEWS, "waveline-news", AgentActionPhase::Configured, Some(SUCCESSOR), "01815f87-7707-77b2-9087-1e59fc5fe83f", AgentOperationKind::Create);
    let receipt = provider.abandon_create(NEWS, false, &fake(&[])).unwrap();
    assert_eq!(receipt.plan.durable_session, None);
    assert_eq!(receipt.plan.successor_session.as_deref(), Some(SUCCESSOR));
    assert!(receipt.removed[0].starts_with("moved native thread"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn completed_running_foreign_and_failed_cleanups_are_refused_without_change() {
    let (provider, root) = provider();
    seed(&provider, "done", "w-done", AgentActionPhase::Complete, None, "n1", AgentOperationKind::Create);
    seed(&provider, "repl", "w-repl", AgentActionPhase::Configured, None, "n2", AgentOperationKind::Replace);
    seed(&provider, "busy", "w-busy", AgentActionPhase::Configured, None, "n3", AgentOperationKind::Create);
    let code = |result: Result<CreateAbandonReceipt, AgentManagementError>| result.unwrap_err().wire().0;
    assert_eq!(code(provider.abandon_create("done", false, &fake(&[]))), "action_completed");
    assert_eq!(code(provider.abandon_create("repl", false, &fake(&[]))), "not_a_create");
    assert_eq!(code(provider.abandon_create("nope", false, &fake(&[]))), "action_not_found");
    // A failed cleanup leaves the action (and its name) as it was.
    let failing = Fake { fail: true, ..fake(&[]) };
    assert!(provider.abandon_create("busy", false, &failing).is_err());
    assert!(holds_name(&provider, "w-busy"));
    // Running right now: the mutation lock is held.
    let held = provider.store().lock_mutations().unwrap();
    assert_eq!(code(provider.abandon_create("busy", false, &fake(&[]))), "action_in_progress");
    drop(held);
    provider.abandon_create("busy", false, &fake(&[])).unwrap();
    std::fs::remove_dir_all(root).ok();
}
