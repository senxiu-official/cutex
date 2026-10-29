// Included in routing::tests; authority injection keeps these regressions
// independent of HOME, disk state, timers and running native processes.
use crate::agent_management::{AgentManagementError, AgentManagementSnapshot};
use std::cell::Cell;

fn projection_fixture(count: usize) -> (Vec<AgentBusAgent>, CutexSessionStore) {
    let mut sessions = CutexSessionStore::default();
    let mut agents = Vec::new();
    for index in 0..count {
        let durable = format!("cutex.projection-{index}");
        let native = format!("native-{index}");
        let runtime = format!("runtime-{index}");
        sessions.sessions.insert(durable.clone(), active_session(
            &durable, &native, Some(&runtime), 1,
        ));
        agents.push(sample_agent(&runtime, &runtime, None, Some(&native), &["alpha"]));
    }
    (agents, sessions)
}

fn projection_tombstone(state: &mut AgentManagementSnapshot, record: &CutexSessionRecord) {
    use crate::agent_management::{ManagedAgentRecord, ManagedAgentSpec};
    use crate::role_revision::{CutexSessionId, Rfc3339};
    let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
    let timestamp = Rfc3339::new("2026-10-27T00:00:00Z").unwrap();
    state.agents.insert(id.clone(), ManagedAgentRecord {
        project_id: None, created_by_director_session: None, created_by_operator_session: None,
        cutex_session_id: id, native_session_id: record.codex_session_id.clone().unwrap(),
        spec: ManagedAgentSpec {
            name: "retired".into(), cwd: record.cwd.clone(), profile: Some("test".into()),
            runtime_backend: "host".into(), model: "test".into(), reasoning: "low".into(),
            permissions: "default".into(), approval_policy: "never".into(),
            sandbox_mode: "read-only".into(), groups: vec!["alpha".into()],
            expose_to_im: false, pin: false,
        },
        created_at: timestamp.clone(), retired_at: Some(timestamp),
    });
}

#[test]
fn roster_projection_loads_one_authority_for_21_endpoints_and_321_sessions() {
    let (mut agents, mut sessions) = projection_fixture(21);
    for index in 21..321 {
        let durable = format!("cutex.offline-{index}");
        sessions.sessions.insert(durable.clone(), active_session(&durable, "offline", None, 0));
    }
    let calls = Cell::new(0);
    project_current_durable_session_ids_with_authority(&mut agents, &sessions, || {
        calls.set(calls.get() + 1);
        assert_eq!(calls.get(), 1, "one coherent authority view per roster");
        Ok(AgentManagementSnapshot::empty())
    });
    assert_eq!(calls.get(), 1);
    for (index, agent) in agents.iter().enumerate() {
        assert_eq!(agent.cutex_session_id, Some(format!("cutex.projection-{index}")));
    }
}

#[test]
fn roster_projection_clears_stale_ids_without_authority_for_ineligible_endpoints() {
    let (mut agents, mut sessions) = projection_fixture(4);
    sessions.sessions.get_mut("cutex.projection-0").unwrap().runtime_generation = 0;
    sessions.sessions.get_mut("cutex.projection-1").unwrap().current_runtime_agent_id = Some("replaced".into());
    agents[2].session_id = Some("stale-native-thread".into());
    sessions.sessions.get_mut("cutex.projection-3").unwrap().archive_state =
        crate::session::model::CutexSessionArchiveState::Retired;
    for agent in &mut agents { agent.cutex_session_id = Some("untrusted-old-projection".into()); }
    project_current_durable_session_ids_with_authority(&mut agents, &sessions, || {
        panic!("offline, replaced, stale and retired endpoints must be filtered first")
    });
    assert!(agents.iter().all(|a| a.cutex_session_id.is_none()));
}

#[test]
fn roster_projection_caches_authority_failure_and_never_falls_back_to_empty() {
    for invalid_store in [false, true] {
        let (mut agents, sessions) = projection_fixture(3);
        for agent in &mut agents { agent.cutex_session_id = Some("old-projection".into()); }
        let calls = Cell::new(0);
        project_current_durable_session_ids_with_authority(&mut agents, &sessions, || {
            calls.set(calls.get() + 1);
            // A hypothetical later read would succeed. The same roster must
            // not mix that view with an earlier failed authority read.
            if calls.get() > 1 { return Ok(AgentManagementSnapshot::empty()); }
            Err(if invalid_store { AgentManagementError::InvalidStore }
                else { AgentManagementError::PersistenceUnavailable })
        });
        assert_eq!(calls.get(), 1);
        assert!(agents.iter().all(|a| a.cutex_session_id.is_none()));
    }
}

#[test]
fn roster_projection_permanent_tombstone_overrides_active_session_claim() {
    let (mut agents, sessions) = projection_fixture(2);
    let mut authority = AgentManagementSnapshot::empty();
    let retired = &sessions.sessions["cutex.projection-0"];
    assert!(retired.is_active()); // stale session projection must not revive it
    projection_tombstone(&mut authority, retired);
    project_current_durable_session_ids_with_authority(&mut agents, &sessions, || Ok(authority.clone()));
    assert!(agents[0].cutex_session_id.is_none());
    assert_eq!(agents[1].cutex_session_id.as_deref(), Some("cutex.projection-1"));
}

#[test]
fn roster_projection_audit_only_permanent_retirement_overrides_active_session_claim() {
    use crate::agent_management::{
        AgentActionId, AgentArchiveKind, AgentArchiveOperation, AgentArchiveReceipt,
        AgentArchiveRequest, AgentArchiveReview, AgentArchiveStage,
    };
    use crate::role_revision::{CutexSessionId, Rfc3339, Sha256};

    for stage in [AgentArchiveStage::Prepared,
        AgentArchiveStage::PermanentRetirementCommitted, AgentArchiveStage::Committed] {
        let (mut agents, sessions) = projection_fixture(2);
        let record = &sessions.sessions["cutex.projection-0"];
        assert!(record.is_active());
        assert!(record.retired_at.is_none());
        let blocked = stage != AgentArchiveStage::Prepared;
        let mut authority = AgentManagementSnapshot::empty();
        authority.agent_archive_audit.insert("audit-only-retirement".into(), AgentArchiveReceipt {
            request: AgentArchiveRequest {
                reason: None,
                action_id: AgentActionId::new("audit-only-retirement").unwrap(),
                review: AgentArchiveReview {
                    cutex_session_id: CutexSessionId::new(record.cutex_session_id.clone()).unwrap(),
                    formal_name: "Retired Agent".into(),
                    operation: AgentArchiveOperation::PermanentRetire,
                    current_project_id: None,
                    revision: record.durable_revision(),
                    runtime_generation: record.runtime_generation,
                },
            },
            request_sha256: Sha256::new("a".repeat(64)).unwrap(),
            stage,
            result: None,
            error: None,
            committed_at: Rfc3339::new("2026-10-27T00:00:00Z").unwrap(),
            archive_kind: Some(AgentArchiveKind::Permanent),
            restorable: Some(false),
        });
        // No roster tombstone or alternate receipt map can explain rejection.
        assert!(authority.agents.is_empty());
        assert!(authority.agent_archive_actions.is_empty());
        agents[0].cutex_session_id = Some(record.cutex_session_id.clone());
        project_current_durable_session_ids_with_authority(&mut agents, &sessions, || Ok(authority.clone()));
        assert_eq!(agents[0].cutex_session_id.is_none(), blocked,
            "only committed permanent-retirement audit evidence blocks the stale active claim");
        assert_eq!(agents[1].cutex_session_id.as_deref(), Some("cutex.projection-1"));
    }
}

#[test]
fn roster_projection_duplicate_durable_and_ambiguous_runtime_stay_unpublished() {
    let (agents, sessions) = projection_fixture(1);
    for duplicate_durable in [false, true] {
        let mut projected = agents.clone();
        let mut store = sessions.clone();
        let mut collision = store.sessions["cutex.projection-0"].clone();
        if duplicate_durable {
            // Even an offline duplicate prevents publication of this durable ID.
            collision.runtime_generation = 0;
        } else {
            collision.cutex_session_id = "cutex.other-durable".into();
        }
        store.sessions.insert("collision".into(), collision);
        project_current_durable_session_ids_with_authority(&mut projected, &store, || Ok(AgentManagementSnapshot::empty()));
        assert!(projected[0].cutex_session_id.is_none());
    }
}

#[test]
fn roster_projection_disambiguates_only_after_archive_validation() {
    let (mut agents, mut sessions) = projection_fixture(1);
    let mut retired = sessions.sessions["cutex.projection-0"].clone();
    retired.cutex_session_id = "cutex.retired-collision".into();
    let mut authority = AgentManagementSnapshot::empty();
    projection_tombstone(&mut authority, &retired);
    sessions.sessions.insert(retired.cutex_session_id.clone(), retired);
    project_current_durable_session_ids_with_authority(&mut agents, &sessions, || Ok(authority.clone()));
    // Preserve the original policy: a retired different durable ID sharing
    // the runtime claim does not make the one eligible record ambiguous.
    assert_eq!(agents[0].cutex_session_id.as_deref(), Some("cutex.projection-0"));
}

#[test]
fn roster_projection_adapter_pid_and_thread_fences_survive_prefiltering() {
    for claude in [false, true] {
        let (agents, mut sessions) = projection_fixture(1);
        let record = sessions.sessions.get_mut("cutex.projection-0").unwrap();
        record.runtime_pid = Some(1);
        if claude {
            record.claude_session = Some(serde_json::from_value(serde_json::json!({
                "deployment":{"python":"unused","adapter_source":"unused","claude_cli":"unused",
                    "claude_home":"unused","tui_binary":"unused"},
                "thread_id":"native-0","state_dir":"unused","model":"test"
            })).unwrap());
        } else {
            record.dsh_session = Some(serde_json::from_value(serde_json::json!({
                "thread_id":"native-0","state_dir":"unused","node":"unused",
                "adapter":"unused","dsh_cli":"unused","patch":"unused","install_root":"unused"
            })).unwrap());
        }
        for (pid, native, expected) in [(1, "native-0", true), (2, "native-0", false), (1, "other-thread", false)] {
            let mut projected = agents.clone();
            projected[0].pid = pid;
            projected[0].session_id = Some(native.into());
            let calls = Cell::new(0);
            project_current_durable_session_ids_with_authority(&mut projected, &sessions, || {
                calls.set(calls.get() + 1);
                Ok(AgentManagementSnapshot::empty())
            });
            assert_eq!(projected[0].cutex_session_id.is_some(), expected);
            assert_eq!(calls.get(), usize::from(expected));
        }
    }
}
