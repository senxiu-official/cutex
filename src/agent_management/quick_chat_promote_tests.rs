use super::*;
use crate::session::model::{CutexSessionRuntimeBackend as Backend, CutexSessionStore};
use serde_json::json;

struct Fixture {
    root: std::path::PathBuf,
    path: std::path::PathBuf,
    provider: AgentManagementProvider,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("qp-{}", uuid::Uuid::new_v4().simple()));
        Self {
            provider: AgentManagementProvider::open(root.join("p")).unwrap(),
            path: root.join("s.json"),
            root,
        }
    }
    fn chat(&self, id: &str, backend: Backend) -> CutexSessionRecord {
        let native = uuid::Uuid::new_v4().to_string();
        let mut record = CutexSessionRecord::new(
            id.into(),
            Some(native.clone()),
            crate::platform::host::current_host_name(),
            self.root.to_string_lossy().into_owned(),
            Some("test-profile".into()),
        )
        .unwrap();
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
        record.agent_enabled = false;
        record.runtime_backend = backend;
        record.model_defaults = Some("test-model".into());
        record.thread_name = Some("native title".into());
        record.display_name_hint = Some("chat display title".into());
        record.default_cli_args = vec!["--test-arg".into()];
        if backend == Backend::ClaudeSubscription {
            record.codex_session_id = None;
            record.claude_session = Some(serde_json::from_value(json!({
                "deployment":{"python":"unused","adapter_source":"unused","claude_cli":"unused","claude_home":"unused","tui_binary":"unused"},
                "thread_id":native,"state_dir":"unused","model":"claude-test","native_session_id":"claude-native"
            })).unwrap());
        }
        if backend.is_acp() {
            record.codex_session_id = None;
            record.dsh_session = Some(serde_json::from_value(json!({
                "thread_id":native,"state_dir":"unused","node":"unused","adapter":"unused","dsh_cli":"unused","patch":"unused","install_root":"unused",
                "flavor":if backend == Backend::Kimi {"kimi"} else {"dsh"}, "spike":false
            })).unwrap());
        }
        let mut store = load_cutex_session_store_from_path(&self.path).unwrap();
        store.sessions.insert(id.into(), record.clone());
        save_cutex_session_store_to_path(&self.path, &store).unwrap();
        record
    }
    fn run(
        &self,
        id: &str,
        request: &QuickChatPromoteRequest,
    ) -> anyhow::Result<QuickChatPromoteResult> {
        self.provider.promote_quick_chat(
            &HumanManagementPrincipal::authenticated(),
            &self.path,
            id,
            request,
        )
    }
    fn store(&self) -> CutexSessionStore {
        load_cutex_session_store_from_path(&self.path).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        crate::session::store::close_session_store(&self.path);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn request(record: &CutexSessionRecord, name: &str) -> QuickChatPromoteRequest {
    QuickChatPromoteRequest {
        request_id: "promote-1".into(),
        expected_revision: record.revision,
        name: name.into(),
        project_id: None,
    }
}
fn error_code(result: anyhow::Result<QuickChatPromoteResult>) -> &'static str {
    result.unwrap_err().downcast::<NameError>().unwrap().code
}

#[test]
fn quick_chat_promote_all_backends_keep_native_identity_and_launch_defaults() {
    for backend in [
        Backend::Host,
        Backend::ClaudeSubscription,
        Backend::Dsh,
        Backend::Kimi,
    ] {
        let f = Fixture::new();
        let before = f.chat("cutex.quick", backend);
        let req = request(&before, "Promoted");
        let result = f.run(&before.cutex_session_id, &req).unwrap();
        assert!(result.complete, "{result:?}");
        let stored = f.store();
        let after = &stored.sessions["cutex.quick"];
        let mut expected = before.clone();
        expected.agent_enabled = true;
        expected.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        expected.formal_agent_name = Some(req.name.clone());
        expected.agent_groups = after.agent_groups.clone();
        expected.revision += 1;
        expected.updated_at = after.updated_at.clone();
        assert_eq!(
            after, &expected,
            "conversion changed unrelated durable/native facts"
        );
        assert!(!after.agent_groups.is_empty());
        assert!(!after.is_owned_session());
        let state = f.provider.store().snapshot().unwrap();
        let agent = &state.agents[&CutexSessionId::new("cutex.quick").unwrap()];
        assert_eq!(
            agent.native_session_id,
            before.app_server_thread_id().unwrap()
        );
        assert_eq!(agent.spec.name, "Promoted");
        assert_eq!(f.run("cutex.quick", &req).unwrap(), result);
        assert_eq!(f.store().quick_chat_promotion_receipts.len(), 1);
        let mut changed = req.clone();
        changed.name = "Different".into();
        assert_eq!(
            error_code(f.run("cutex.quick", &changed)),
            "request_id_payload_conflict"
        );
    }
}

#[test]
fn quick_chat_promote_refuses_running_stale_invalid_and_name_conflict_without_writes() {
    let f = Fixture::new();
    let record = f.chat("cutex.chat", Backend::Host);
    let req = request(&record, "Name");
    let mut store = f.store();
    store.sessions.get_mut("cutex.chat").unwrap().runtime_pid = Some(std::process::id());
    save_cutex_session_store_to_path(&f.path, &store).unwrap();
    assert_eq!(error_code(f.run("cutex.chat", &req)), "quick_chat_running");
    assert!(f.store().quick_chat_promotion_receipts.is_empty());
    let other = f.chat("cutex.other", Backend::Host);
    let first = f.run("cutex.other", &request(&other, "Taken")).unwrap();
    assert!(first.complete);
    store.sessions.get_mut("cutex.chat").unwrap().runtime_pid = None;
    let mut latest = f.store();
    latest
        .sessions
        .insert("cutex.chat".into(), store.sessions["cutex.chat"].clone());
    save_cutex_session_store_to_path(&f.path, &latest).unwrap();
    let mut invalid = req.clone();
    invalid.name = " director ".into();
    assert_eq!(
        error_code(f.run("cutex.chat", &invalid)),
        "agent_name_invalid"
    );
    let mut stale = req.clone();
    stale.expected_revision += 1;
    assert_eq!(error_code(f.run("cutex.chat", &stale)), "revision_conflict");
    let mut clash = req.clone();
    clash.name = "taken".into();
    assert_eq!(
        error_code(f.run("cutex.chat", &clash)),
        "agent_name_conflict"
    );
    assert!(!f.store().sessions["cutex.chat"].agent_enabled);
    assert_eq!(
        error_code(f.run("cutex.missing", &req)),
        "session_not_found"
    );
}

#[test]
fn quick_chat_promote_recovers_crash_between_session_and_roster_writes() {
    let f = Fixture::new();
    let record = f.chat("cutex.crash", Backend::Host);
    let req = request(&record, "Recovered");
    INTERRUPT.with(|flag| flag.set(true));
    assert_eq!(
        error_code(f.run("cutex.crash", &req)),
        "simulated_interruption"
    );
    assert!(f.store().sessions["cutex.crash"].agent_enabled);
    assert_eq!(f.store().quick_chat_promotion_receipts.len(), 1);
    assert!(f.provider.store().snapshot().unwrap().agents.is_empty());
    crate::session::store::close_session_store(&f.path);
    let restarted = AgentManagementProvider::open(f.root.join("p")).unwrap();
    assert!(restarted
        .recover_quick_chat_promotions(&f.path)
        .unwrap()
        .is_empty());
    assert!(f.run("cutex.crash", &req).unwrap().complete);
    assert!(restarted
        .recover_quick_chat_promotions(&f.path)
        .unwrap()
        .is_empty());
    assert_eq!(restarted.store().snapshot().unwrap().agents.len(), 1);
}

#[test]
fn quick_chat_promote_project_assignment_is_replayable_and_keeps_scratch_cwd() {
    let f = Fixture::new();
    let record = f.chat("cutex.member", Backend::Host);
    let project = ProjectId::new("test-project").unwrap();
    // An existing directorless project is also a valid destination.
    f.provider
        .store()
        .with_state(true, |mut state| {
            state.project_states.insert(
                project.clone(),
                ProjectStateRecord {
                    project_id: project.clone(),
                    lifecycle: ProjectLifecycle::Active,
                    revision: 1,
                    created_at: super::super::now(),
                    updated_at: super::super::now(),
                    archived_at: None,
                    workspace_root: None,
                    last_director_epoch: None,
                },
            );
            Ok((state, (), true))
        })
        .unwrap();
    let mut req = request(&record, "Member");
    req.project_id = Some(project.clone());
    INTERRUPT.with(|flag| flag.set(true));
    assert!(f.run("cutex.member", &req).is_err());
    assert!(f
        .provider
        .recover_quick_chat_promotions(&f.path)
        .unwrap()
        .is_empty());
    let result = f.run("cutex.member", &req).unwrap();
    assert!(result.complete, "{result:?}");
    let state = f.provider.store().snapshot().unwrap();
    let member = state.current_project_memberships.values().next().unwrap();
    assert_eq!(member.project_id, Some(project));
    assert_eq!(f.store().sessions["cutex.member"].cwd, record.cwd);
    assert_eq!(state.human_management_project_mutations.len(), 1);
    assert_eq!(f.run("cutex.member", &req).unwrap(), result);
}

#[test]
fn quick_chat_promote_recovers_after_import_before_final_receipt_and_never_refreshes_cas() {
    let f = Fixture::new();
    let record = f.chat("cutex.finalize", Backend::Host);
    let req = request(&record, "Finalized");
    let result = f.run("cutex.finalize", &req).unwrap();
    assert!(result.complete);
    let mut store = f.store();
    let key = store
        .quick_chat_promotion_receipts
        .keys()
        .next()
        .unwrap()
        .clone();
    store
        .quick_chat_promotion_receipts
        .get_mut(&key)
        .unwrap()
        .result
        .complete = false;
    save_cutex_session_store_to_path(&f.path, &store).unwrap();
    assert!(f
        .provider
        .recover_quick_chat_promotions(&f.path)
        .unwrap()
        .is_empty());
    assert_eq!(f.run("cutex.finalize", &req).unwrap(), result);
    assert_eq!(
        f.provider
            .store()
            .snapshot()
            .unwrap()
            .durable_import_actions
            .len(),
        1
    );

    let other = f.chat("cutex.fenced", Backend::Host);
    let other_req = request(&other, "Fenced");
    INTERRUPT.with(|flag| flag.set(true));
    assert!(f.run("cutex.fenced", &other_req).is_err());
    let mut store = f.store();
    let changed = store.sessions.get_mut("cutex.fenced").unwrap();
    changed.model_defaults = Some("new-model".into());
    changed.bump_durable_revision().unwrap();
    save_cutex_session_store_to_path(&f.path, &store).unwrap();
    let incomplete = f.run("cutex.fenced", &other_req).unwrap();
    assert!(!incomplete.complete);
    assert!(
        incomplete
            .error
            .as_ref()
            .unwrap()
            .contains("stale_durable_candidate"),
        "{incomplete:?}"
    );
    assert_eq!(
        f.store().sessions["cutex.fenced"].model_defaults.as_deref(),
        Some("new-model")
    );
}

#[test]
fn quick_chat_promote_keeps_former_name_reservations_and_rejects_invalid_projects() {
    let f = Fixture::new();
    let first = f.chat("cutex.named", Backend::Host);
    let promoted = f.run("cutex.named", &request(&first, "Reserved")).unwrap();
    assert!(promoted.complete);
    crate::session::store::rename_formal_name_at(
        &f.path,
        "cutex.named",
        &RenameRequest {
            request_id: "rename".into(),
            expected_revision: promoted.revision,
            name: "Renamed".into(),
        },
        &NameScope::Unassigned,
        None,
        "human-management",
    )
    .unwrap();
    let second = f.chat("cutex.new", Backend::Host);
    let req = request(&second, "reserved");
    assert_eq!(error_code(f.run("cutex.new", &req)), "agent_name_reserved");
    let mut project_req = request(&second, "Valid");
    project_req.project_id = Some(ProjectId::new("missing").unwrap());
    assert_eq!(
        error_code(f.run("cutex.new", &project_req)),
        "project_not_found"
    );
    assert!(!f.store().sessions["cutex.new"].agent_enabled);
}

#[test]
fn quick_chat_promote_discovery_returns_only_unfinished_receipts() {
    let f = Fixture::new();
    let first = f.chat("cutex.done", Backend::Host);
    assert!(
        f.run("cutex.done", &request(&first, "Done"))
            .unwrap()
            .complete
    );
    let second = f.chat("cutex.pending", Backend::Host);
    assert!(
        f.run("cutex.pending", &request(&second, "Pending"))
            .unwrap()
            .complete
    );
    assert!(pending_quick_chat_promotions_at(&f.path)
        .unwrap()
        .is_empty());
    let mut store = f.store();
    let key = store
        .quick_chat_promotion_receipts
        .iter()
        .find(|(_, r)| r.record.cutex_session_id == "cutex.pending")
        .unwrap()
        .0
        .clone();
    let receipt = store.quick_chat_promotion_receipts.get_mut(&key).unwrap();
    receipt.result.complete = false;
    let expected = receipt.clone();
    save_cutex_session_store_to_path(&f.path, &store).unwrap();
    assert_eq!(
        pending_quick_chat_promotions_at(&f.path).unwrap(),
        vec![expected]
    );
}
