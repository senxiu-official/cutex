use std::collections::BTreeMap;

#[test]
fn directory_legacy_archive_evidence_matches_complete_snapshot_after_foreign_update() {
    let fixture = Fixture::new("legacy-directory");
    let mut state = eva_roster(1);
    let ids: Vec<_> = state.actions.keys().take(4).cloned().collect();
    for (id, operation, phase) in [
        (&ids[0], AgentOperationKind::Close, AgentActionPhase::NoWrite),
        (&ids[1], AgentOperationKind::Close, AgentActionPhase::Complete),
        (&ids[2], AgentOperationKind::Replace, AgentActionPhase::PredecessorClosing),
        (&ids[3], AgentOperationKind::Replace, AgentActionPhase::Complete),
    ] {
        let action = state.actions.get_mut(id).unwrap();
        action.operation = operation;
        action.phase = phase;
    }
    for (index, action) in ids.iter().enumerate() {
        let events: Vec<_> = state.phase_events.iter().filter(|(_, event)| &event.action_id == action)
            .map(|(id, _)| id.clone()).collect();
        for event in events {
            let event = state.phase_events.get_mut(&event).unwrap();
            event.subject_cutex_session_id = Some(CutexSessionId::new(format!("cutex.legacy-subject-{index}")).unwrap());
            event.predecessor_cutex_session_id = Some(CutexSessionId::new(format!("cutex.legacy-predecessor-{index}")).unwrap());
        }
    }
    fixture.write_previous(&state);
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    let directory = store.directory_snapshot_shared().unwrap();
    let full = store.snapshot_shared().unwrap();
    let compare = |directory: &AgentManagementSnapshot, full: &AgentManagementSnapshot| {
        let targets: std::collections::BTreeSet<_> = full.phase_events.values()
            .flat_map(|event| event.subject_cutex_session_id.iter().chain(event.predecessor_cutex_session_id.iter()))
            .cloned().collect();
        assert!(!targets.is_empty());
        let mut saw_legacy = false;
        for id in targets {
            let mut record = crate::session::model::CutexSessionRecord::new_at(
                id.as_str().into(), None, "host".into(), "/tmp".into(), None,
                "2026-11-04T00:00:00Z".into()).unwrap();
            record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
            let expected = format!("{:?}", crate::agent_management::agent_archive_disposition(full, &record));
            let actual = format!("{:?}", crate::agent_management::agent_archive_disposition(directory, &record));
            saw_legacy |= expected.contains("legacy_close_requires_owner_review");
            assert_eq!(actual, expected, "{id:?}");
        }
        assert!(saw_legacy, "exercise a real legacy close refusal");
    };
    compare(&directory, &full);
    let mut next = (*full).clone();
    next.actions.get_mut(&ids[1]).unwrap().phase = AgentActionPhase::NoWrite;
    next.actions.get_mut(&ids[2]).unwrap().phase = AgentActionPhase::Complete;
    next.store_revision += 1;
    let changes = diff(&full, &next).unwrap();
    commit(&database_path(&fixture.root), full.store_revision, &next.schema, next.store_revision, &changes).unwrap();
    compare(&store.directory_snapshot_shared().unwrap(), &next);
    compare(&directory, &full); // previously acquired authority view remains stable
}

#[test]
fn directory_cold_decode_is_flat_with_tenfold_unrelated_action_history() {
    let mut measured = Vec::new();
    for scale in [1, 10] {
        let fixture = Fixture::new("directory-scale");
        let mut state = eva_roster(1);
        let mut history = eva_roster(scale);
        let keys: Vec<_> = history.actions.keys().cloned().collect();
        for key in keys {
            let action = history.actions.get_mut(&key).unwrap();
            action.operation = AgentOperationKind::QueryManaged;
            action.phase = AgentActionPhase::Complete;
            action.response = None;
        }
        state.actions = history.actions;
        state.phase_events = history.phase_events;
        state.failure_events = history.failure_events;
        fixture.write_previous(&state);
        std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
        let store = AgentManagementStore::open(&fixture.root).unwrap();
        store.import_now().unwrap();
        let (directory, counts) = crate::store::metrics::measure(|| store.directory_snapshot_shared().unwrap());
        assert_eq!(directory.agents, state.agents);
        assert!(directory.actions.is_empty());
        assert!(counts.bytes_decoded > 0 && counts.rows_materialized > 0);
        assert_eq!(counts.whole_state_copies, 0);
        let db = open_ro(&database_path(&fixture.root)).unwrap();
        let mut query = db.prepare(LEGACY_CLOSE_SQL).unwrap();
        let proof_rows = query.query_map(params![text(&AgentOperationKind::Close),text(&AgentActionPhase::NoWrite),
            text(&AgentOperationKind::Replace),text(&AgentActionPhase::PredecessorClosing),text(&AgentActionPhase::PredecessorClosed)],
            |row| row.get::<_,Vec<u8>>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(proof_rows.is_empty());
        let steps = query.get_status(rusqlite::StatementStatus::VmStep);
        assert!(steps > 0, "the physical work counter must be enabled");
        assert_eq!(query.get_status(rusqlite::StatementStatus::FullscanStep), 0);
        measured.push((counts.bytes_decoded, counts.rows_materialized, steps));
    }
    assert_eq!(measured[0], measured[1], "directory must not load action/event history");
}

#[test]
fn directory_snapshot_preserves_authority_without_materializing_action_history() {
    let fixture = Fixture::new("directory");
    fixture.write_previous(&eva_roster(1));
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    store.import_now().unwrap();
    let (directory, counts) = crate::store::metrics::measure(|| store.directory_snapshot_shared().unwrap());
    assert_eq!(counts.whole_state_copies, 0);
    assert!(counts.rows_materialized > 0);
    assert!(directory.actions.is_empty());
    assert!(directory.phase_events.is_empty());
    let mut expected = (*store.snapshot_shared().unwrap()).clone();
    let action = expected.actions.keys().next().unwrap().clone();
    let agent = expected.agents.keys().next().unwrap().clone();
    let full_action = expected.actions[&action].clone();
    expected.actions.clear(); expected.phase_events.clear(); expected.failure_events.clear();
    expected.sparse_requests.clear(); expected.tool_action_receipts.clear();
    expected.legacy_close_projection = directory.legacy_close_projection.clone();
    assert_eq!(*directory, expected);
    let (selected, counts) = crate::store::metrics::measure(|| store.action(&action).unwrap());
    assert_eq!(selected, Some(full_action));
    assert_eq!(counts.rows_materialized, 1);
    assert_eq!(counts.whole_state_copies, 0);
    assert_eq!(store.agent(&agent).unwrap(), directory.agents.get(&agent).cloned());
    assert!(std::sync::Arc::ptr_eq(&directory, &store.directory_snapshot_shared().unwrap()));
    // Full snapshots are an independent, complete view; using a catalog must
    // never make a subsequent mutation/export lose historical fields.
    assert!(!store.snapshot_shared().unwrap().actions.is_empty());
}

#[test]
fn directory_snapshot_follows_foreign_commit_and_keeps_history_lazy() {
    let fixture = Fixture::new("directory-refresh");
    fixture.write_previous(&eva_roster(1));
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    let first = store.directory_snapshot_shared().unwrap();
    let full = store.snapshot_shared().unwrap();
    let mut next = (*full).clone();
    let id = next.agents.keys().next().unwrap().clone();
    next.agents.get_mut(&id).unwrap().spec.name = "foreign name".into();
    let action_id = next.actions.keys().next().unwrap().clone();
    next.actions.remove(&action_id);
    next.store_revision += 1;
    let changes = diff(&full, &next).unwrap();
    commit(&database_path(&fixture.root), full.store_revision, &next.schema, next.store_revision, &changes).unwrap();
    let (second, counts) = crate::store::metrics::measure(|| store.directory_snapshot_shared().unwrap());
    assert_eq!(second.agents[&id].spec.name, "foreign name");
    assert_eq!(first.agents[&id].spec.name, full.agents[&id].spec.name);
    assert!(second.actions.is_empty());
    assert!(counts.rows_materialized > 0);
    assert_eq!(counts.whole_state_copies, 0);
    assert!(store.action(&action_id).unwrap().is_none());
    assert_eq!(*store.snapshot_shared().unwrap(), next);
}
use std::path::{Path, PathBuf};

use super::*;
use crate::agent_management::{
    AgentActionId, AgentActionPhase, AgentActionRecord, AgentManagementFailureEvent,
    AgentManagementFailureSchema, AgentManagementOutcome, AgentManagementPhaseEvent,
    AgentManagementResponse, AgentManagementSchema, AgentManagementStore, AgentOperationKind,
    AgentOperatorAuditEvent, AgentOperatorAuditKind, AgentOperatorGrant, CurrentProjectMembership,
    FailureRoutingStatus, ManagedAgentRecord, ManagedAgentSpec, ProjectAuthority, ProjectId,
};
use crate::role_revision::{CutexSessionId, Rfc3339, Sha256 as Digest256};

/// EVA roster shape 2026-11-03 (counts only, no contents): 226 agents, 11
/// projects, 66 memberships, 256 actions, 1663 phase events, 25 failures,
/// 54 tool receipts in `extra`, 4,470,674 B in total. `scale` multiplies the
/// inactive history (retired agents, completed actions, events).
pub(crate) fn eva_roster(scale: usize) -> AgentManagementSnapshot {
    let ts = |n: usize| Rfc3339::new(format!("2026-10-{:02}T{:02}:{:02}:{:02}Z", 1 + n % 28, n % 24, n % 60, (n / 60) % 60)).unwrap();
    let sid = |n: usize| CutexSessionId::new(format!("cutex.{:08x}-synthetic-{n}", n * 2654435761 % 0xffff_ffff)).unwrap();
    let pid = |n: usize| ProjectId::new(format!("project-synthetic-{n}")).unwrap();
    let aid = |n: usize| AgentActionId::new(format!("action-synthetic-{n:06}")).unwrap();
    let sha = |n: usize| Digest256::new(format!("{:064x}", n as u128 * 0x9e37_79b9_7f4a_7c15)).unwrap();
    let mut state = AgentManagementSnapshot::empty();
    state.store_revision = 2563 * scale as u64;
    let projects = 11;
    for p in 0..projects {
        state.projects.insert(pid(p), ProjectAuthority {
            project_id: pid(p),
            authorized_director_session: sid(p),
            authority_epoch: 3 + p as u64,
            updated_at: ts(p),
        });
    }
    let agents = 196 + 30 * scale;
    for n in 0..agents {
        let retired = n >= 196;
        let project = (n % 5 != 0).then(|| pid(n % projects));
        state.agents.insert(sid(n), ManagedAgentRecord {
            project_id: project.clone(),
            created_by_director_session: project.as_ref().map(|_| sid(n % projects)),
            created_by_operator_session: None,
            cutex_session_id: sid(n),
            native_session_id: format!("019a{:028x}", n),
            spec: ManagedAgentSpec {
                name: format!("synthetic-agent-{n}"),
                cwd: format!("/home/synthetic/projects/p{}/agent-{n}", n % projects),
                profile: Some(format!("profile-{}", n % 7)),
                runtime_backend: "codex".into(),
                model: "synthetic-model".into(),
                reasoning: "medium".into(),
                permissions: "default".into(),
                approval_policy: "never".into(),
                sandbox_mode: "workspace-write".into(),
                groups: vec![format!("group-{}", n % 3)],
                expose_to_im: n % 11 == 0,
                pin: n % 13 == 0,
            },
            created_at: ts(n),
            retired_at: retired.then(|| ts(n + 1)),
        });
        if n < 66 {
            state.current_project_memberships.insert(sid(n), CurrentProjectMembership {
                cutex_session_id: sid(n),
                project_id: Some(pid((n + 1) % projects)),
                revision: 1 + n as u64 % 4,
                updated_at: ts(n),
            });
        }
    }
    let actions = 34 + 222 * scale;
    for n in 0..actions {
        let phase = if n < 34 { AgentActionPhase::Configured } else { AgentActionPhase::Complete };
        let operation = [AgentOperationKind::Create, AgentOperationKind::Close, AgentOperationKind::Online, AgentOperationKind::Offline][n % 4];
        let response = (n >= 34).then(|| AgentManagementResponse {
            schema: AgentManagementSchema::V1,
            action_id: aid(n),
            outcome: AgentManagementOutcome::NoWrite { code: "synthetic_no_write".into(), detail: "synthetic detail ".repeat(150) },
        });
        state.actions.insert(aid(n), AgentActionRecord {
            action_id: aid(n),
            request_sha256: sha(n),
            operation,
            project_id: pid(n % projects),
            caller_cutex_session: sid(n % projects),
            created_by: None,
            phase,
            phase_sequence: 6,
            reserved_agent_name: Some(format!("synthetic-agent-{n}")),
            reserved_agent_cwd: Some(format!("/home/synthetic/projects/p{}/agent-{n}", n % projects)),
            known_successor_cutex_session: Some(sid(n % agents)),
            known_native_session_id: Some(format!("019a{:028x}", n)),
            native_bootstrap_retryable: false,
            historical_runtime_occurrence_fence: None,
            external_message_id: Some(format!("msg-{n}")),
            response,
            created_at: ts(n),
            updated_at: ts(n + 2),
        });
        for seq in 0..((1663 * scale) / (222 * scale + 34)).max(1) as u64 + 1 {
            let event_id = format!("phase-{n:06}-{seq}");
            state.phase_events.insert(event_id.clone(), AgentManagementPhaseEvent {
                event_id,
                action_id: aid(n),
                project_id: pid(n % projects),
                operation,
                phase: if seq == 0 { AgentActionPhase::Prepared } else { phase },
                phase_sequence: seq,
                committed_at: ts(n + seq as usize),
                presentation_owner_cutex_session_id: sid(n % projects),
                subject_cutex_session_id: Some(sid(n % agents)),
                subject_agent_name: Some(format!("synthetic-agent-{n}")),
                predecessor_cutex_session_id: None,
                successor_cutex_session_id: None,
                replace_policy: None,
                rotation_mode: None,
                authority_epoch: Some(3),
            });
        }
        if n % 10 == 0 {
            let event_id = format!("failure-{n:06}");
            state.failure_events.insert(event_id.clone(), AgentManagementFailureEvent {
                schema: AgentManagementFailureSchema::V1,
                event_id,
                action_id: aid(n),
                project_id: pid(n % projects),
                operation,
                code: "synthetic_failure".into(),
                detail: "synthetic failure detail ".repeat(10),
                routing_status: if n % 20 == 0 { FailureRoutingStatus::Routable } else { FailureRoutingStatus::Unrouted },
                route_to_director_session: Some(sid(n % projects)),
                target_cutex_session_id: None,
                created_at: ts(n),
            });
        }
    }
    let grant = AgentOperatorGrant {
        project_id: pid(0),
        operator_cutex_session_id: sid(20),
        grant_revision: 1,
        granted_at: ts(1),
        granted_by_primary_director_session: sid(0),
        performed_by_human_management: true,
    };
    state.operator_grants.insert(pid(0), BTreeMap::from([(sid(20), grant)]));
    // An explicitly empty grant set left behind by a revoke.
    state.operator_grants.insert(pid(1), BTreeMap::new());
    state.operator_grant_revisions.insert(pid(0), 1);
    state.operator_grant_revisions.insert(pid(1), 2);
    state.operator_audit_events.insert("operator-audit-0".into(), AgentOperatorAuditEvent {
        event_id: "operator-audit-0".into(),
        action_id: aid(0),
        project_id: pid(0),
        operator_cutex_session_id: sid(20),
        kind: AgentOperatorAuditKind::Granted,
        previous_grant_revision: 0,
        grant_revision: 1,
        primary_director_cutex_session_id: sid(0),
        performed_by_human_management: true,
        committed_at: ts(1),
    });
    // Foreign additive keys other modules keep in the same document.
    let mut receipts = serde_json::Map::new();
    for n in 0..(54 * scale) {
        receipts.insert(format!("tool-receipt-{n}"), serde_json::json!({
            "action_id": format!("tool-{n}"), "tool": "agent_create", "state": "complete",
            "request": {"name": format!("synthetic-agent-{n}"), "note": "synthetic ".repeat(300)},
        }));
    }
    // Receipt-heavy history (EVA: archive/import/sparse receipts ~1.5 MB)
    // modelled as additive foreign receipts of the same size class.
    let mut history = serde_json::Map::new();
    for n in 0..(640 * scale) {
        history.insert(format!("history-receipt-{n}"), serde_json::json!({
            "action_id": format!("history-{n}"), "stage": "committed",
            "result": {"cutex_session_id": format!("cutex.history-{n}"), "note": "history ".repeat(300)},
        }));
    }
    state.extra.insert("tool_action_receipts_v1".into(), serde_json::Value::Object(receipts));
    state.extra.insert("synthetic_receipt_history_v1".into(), serde_json::Value::Object(history));
    state.extra.insert("archive_storage_contract_v1".into(), serde_json::json!({"version": 1, "mode": "synthetic", "z": [1, 2, 3]}));
    state
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cutex-roster-sql-{label}-{}", uuid::Uuid::new_v4())).join("agent-management").join("v1");
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn legacy(&self) -> PathBuf {
        self.root.join(LEGACY_FILE)
    }
    /// What the previous build writes: the whole typed snapshot, pretty.
    fn write_previous(&self, state: &AgentManagementSnapshot) -> Vec<u8> {
        // The previous build keeps tool receipts inside `extra`.
        let state = &fold_tool_receipts(state).unwrap();
        crate::config::atomic::write_private_pretty_json_atomic(&self.legacy(), state, "fixture").unwrap();
        std::fs::read(self.legacy()).unwrap()
    }
    /// What the previous build reads (its reader minus nothing: store.rs
    /// `read_snapshot_file` at 5c8eb804 = serde parse + validate + in-memory
    /// project-state migration).
    fn read_previous(&self) -> AgentManagementSnapshot {
        let mut state = decode_legacy(&std::fs::read(self.legacy()).unwrap()).unwrap();
        crate::agent_management::projects::migrate_project_states(&mut state);
        state
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        super::super::store::forget_cached(&self.root);
        let top = self.root.parent().and_then(Path::parent).unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(top);
    }
}

#[test]
fn import_then_export_reproduces_the_previous_file_byte_for_byte() {
    let fixture = Fixture::new("roundtrip");
    let original = fixture.write_previous(&eva_roster(1));
    let report = import_legacy(&fixture.root).unwrap();
    assert!(!report.already_authoritative);
    assert!(report.rows > 2000);
    // The source stays untouched.
    assert_eq!(std::fs::read(fixture.legacy()).unwrap(), original);
    assert!(import_legacy(&fixture.root).unwrap().already_authoritative);
    export_previous(&fixture.root).unwrap();
    assert_eq!(std::fs::read(fixture.legacy()).unwrap(), original, "R1 export must equal the previous build's own bytes");
    assert!(!is_authoritative(&database_path(&fixture.root)).unwrap());
}

#[test]
fn every_collection_has_a_spec_and_rows_cover_the_snapshot() {
    let state = eva_roster(1);
    let rows = snapshot_rows(&state).unwrap();
    let mut rebuilt = AgentManagementSnapshot::empty();
    rebuilt.store_revision = state.store_revision;
    for row in &rows {
        apply_row(&mut rebuilt, row.spec.field, &row.key, Some(&row.body)).unwrap();
    }
    assert_eq!(rebuilt, state);
    // A serialised top-level key without a spec would be silently dropped.
    let value = serde_json::to_value(AgentManagementSnapshot::empty()).unwrap();
    for key in value.as_object().unwrap().keys() {
        if key == "schema" || key == "store_revision" {
            continue;
        }
        assert!(spec_by_field(key).is_some(), "roster field {key} has no SQL table");
    }
}

#[test]
fn diff_encodes_only_changed_rows() {
    let old = eva_roster(1);
    let mut new = old.clone();
    let agent = new.agents.keys().next().unwrap().clone();
    new.agents.get_mut(&agent).unwrap().spec.pin ^= true;
    let gone = new.phase_events.keys().next().unwrap().clone();
    new.phase_events.remove(&gone);
    new.operator_grants.get_mut(&ProjectId::new("project-synthetic-0").unwrap()).unwrap().clear();
    let changes = diff(&old, &new).unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
}

/// The R1 rehearsal: previous file -> import -> writes through the new store
/// (create, update, delete, unfinished action, grant revoke, foreign extra) ->
/// export to previous -> previous reader sees exactly the new state -> the
/// previous build writes -> this build re-imports that write.
#[test]
fn rehearsal_import_new_writes_export_previous_reads() {
    let fixture = Fixture::new("rehearsal");
    fixture.write_previous(&eva_roster(1));
    // The new build's first locked access imports.
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let before = store.snapshot().unwrap();
    let p0 = ProjectId::new("project-synthetic-0").unwrap();
    store.with_state(true, |mut state| {
        let (id, mut agent) = state.agents.iter().next().map(|(k, v)| (k.clone(), v.clone())).unwrap();
        agent.spec.name = "renamed-after-upgrade".into();
        state.agents.insert(id, agent.clone());
        let new_id = CutexSessionId::new("cutex.after-upgrade").unwrap();
        agent.cutex_session_id = new_id.clone();
        agent.spec.name = "created-after-upgrade".into();
        state.agents.insert(new_id, agent);
        let membership = state.current_project_memberships.keys().next().unwrap().clone();
        state.current_project_memberships.remove(&membership);
        let action = state.actions.keys().next().unwrap().clone();
        state.actions.get_mut(&action).unwrap().phase = AgentActionPhase::NativeBootstrapPending;
        state.operator_grants.get_mut(&p0).unwrap().clear();
        *state.operator_grant_revisions.get_mut(&p0).unwrap() += 1;
        state.extra.insert("future_additive_field".into(), serde_json::json!({"kept": true}));
        Ok((state, (), true))
    }).unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(after.store_revision, before.store_revision + 1);
    assert_eq!(after.agents.len(), before.agents.len() + 1);
    // A second handle in a "foreign process" (cleared cache) observes it.
    super::super::store::forget_cached(&fixture.root);
    assert_eq!(*AgentManagementStore::open(&fixture.root).unwrap().snapshot_shared().unwrap(), after);

    let report = export_previous(&fixture.root).unwrap();
    assert_eq!(report.store_revision, after.store_revision);
    let previous_view = fixture.read_previous();
    assert_eq!(previous_view, after, "previous build must see every post-upgrade write");

    // The previous build writes once more, then this build starts again.
    let mut previous_write = previous_view;
    previous_write.store_revision += 1;
    previous_write.extra.insert("written_by_previous".into(), serde_json::json!(1));
    fixture.write_previous(&previous_write);
    super::super::store::forget_cached(&fixture.root);
    let again = AgentManagementStore::open(&fixture.root).unwrap().snapshot().unwrap();
    assert_eq!(again, previous_write, "re-import after an R1 handoff takes the JSON, never the stale rows");
    // The first epoch's audit is sealed, not overwritten; the new epoch drains
    // its own seeded history into a fresh file.
    let sealed = std::fs::read_dir(&fixture.root).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("roster-audit-sealed-")).count();
    assert_eq!(sealed, 1);
    drain_audit(&fixture.root, usize::MAX >> 1).unwrap();
    let fresh = std::fs::read_to_string(database_dir(&fixture.root).join(AUDIT_FILE)).unwrap();
    assert_eq!(fresh.lines().count(), [&again.phase_events.len(), &again.failure_events.len(), &again.operator_audit_events.len()].iter().map(|n| **n).sum::<usize>()
        + again.agent_archive_audit.len() + again.durable_import_audit.len() + again.project_audit_events.len() + again.reservation_reconciliation_events.len());
}

#[test]
fn shared_reads_reuse_the_generation_and_refresh_changed_rows_only() {
    let fixture = Fixture::new("cache");
    fixture.write_previous(&eva_roster(1));
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    let first = store.snapshot_shared().unwrap();
    for _ in 0..16 {
        assert!(std::sync::Arc::ptr_eq(&first, &store.snapshot_shared().unwrap()));
    }
    let token = store.change_token().unwrap();
    // Foreign commit straight into SQL (another process).
    let mut changed = (*first).clone();
    let id = changed.agents.keys().next().unwrap().clone();
    changed.agents.get_mut(&id).unwrap().spec.pin ^= true;
    changed.store_revision += 1;
    let changes = diff(&first, &changed).unwrap();
    commit(&database_path(&fixture.root), first.store_revision, &changed.schema, changed.store_revision, &changes).unwrap();
    assert_ne!(store.change_token().unwrap(), token);
    let second = store.snapshot_shared().unwrap();
    assert!(!std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(*second, changed);
    // A stale CAS is refused.
    assert!(commit(&database_path(&fixture.root), first.store_revision, &changed.schema, changed.store_revision, &changes).is_err());
}

#[test]
fn audit_outbox_drains_once_and_repairs_a_torn_tail() {
    let fixture = Fixture::new("audit");
    fixture.write_previous(&eva_roster(1));
    import_legacy(&fixture.root).unwrap();
    let audit = database_dir(&fixture.root).join(AUDIT_FILE);
    let imported = drain_audit(&fixture.root, usize::MAX >> 1).unwrap();
    assert!(imported > 1000);
    let lines = || std::fs::read_to_string(&audit).unwrap().lines().count();
    assert_eq!(lines(), imported);
    assert_eq!(drain_audit(&fixture.root, 1024).unwrap(), 0);
    // New event + a torn half-line from a crash after the last checkpoint.
    let db = database_path(&fixture.root);
    let (meta, state) = load_all(&open_ro(&db).unwrap()).unwrap();
    let mut next = state.clone();
    let mut event = next.phase_events.values().next().unwrap().clone();
    event.event_id = "phase-after-upgrade".into();
    next.phase_events.insert(event.event_id.clone(), event);
    commit(&db, meta.store_revision, &next.schema, meta.store_revision + 1, &diff(&state, &next).unwrap()).unwrap();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&audit).unwrap();
        file.write_all(b"{\"seq\":99999999,\"trunc").unwrap();
    }
    assert_eq!(drain_audit(&fixture.root, 1024).unwrap(), 1);
    assert_eq!(lines(), imported + 1);
    let last: serde_json::Value = serde_json::from_str(std::fs::read_to_string(&audit).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["key"], "phase-after-upgrade");
}

#[test]
fn corrupt_legacy_is_refused_and_leaves_no_authority() {
    let fixture = Fixture::new("corrupt");
    std::fs::write(fixture.legacy(), b"{\"truncated").unwrap();
    assert!(import_legacy(&fixture.root).is_err());
    assert!(!is_authoritative(&database_path(&fixture.root)).unwrap());
    assert_eq!(std::fs::read(fixture.legacy()).unwrap(), b"{\"truncated");
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map(|entries| entries.flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum()).unwrap_or(0)
}

/// EVA-scale pause/disk probe (1x and 10x inactive history). Prints one
/// `ROSTER_SCALE` JSON line per scale.
#[test]
#[ignore = "EVA-scale roster migration probe"]
fn roster_scale_import_export_and_point_write() {
    for scale in [1usize, 10] {
        let fixture = Fixture::new(&format!("scale{scale}"));
        let source = fixture.write_previous(&eva_roster(scale));
        let started = std::time::Instant::now();
        let import = import_legacy(&fixture.root).unwrap();
        let import_ms = started.elapsed().as_millis();
        let drained = std::time::Instant::now();
        drain_audit(&fixture.root, usize::MAX >> 1).unwrap();
        let audit_ms = drained.elapsed().as_millis();
        let v2 = dir_bytes(&database_dir(&fixture.root)) - source.len() as u64;
        std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
        let store = AgentManagementStore::open(&fixture.root).unwrap();
        let cold = std::time::Instant::now();
        store.snapshot_shared().unwrap();
        let cold_ms = cold.elapsed().as_millis();
        let mut writes = Vec::new();
        for n in 0..20 {
            let t = std::time::Instant::now();
            store.with_state(true, |mut state| {
                let id = state.agents.keys().nth(n).unwrap().clone();
                state.agents.get_mut(&id).unwrap().spec.pin ^= true;
                Ok((state, (), true))
            }).unwrap();
            writes.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        writes.sort_by(f64::total_cmp);
        // The previous build's cost for the same one-row change: parse is
        // cached there too, so clone + whole pretty rewrite of the document.
        let legacy_state = store.snapshot().unwrap();
        let mut legacy = Vec::new();
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let mut state = legacy_state.clone();
            state.store_revision += 1;
            crate::config::atomic::write_private_pretty_json_atomic(&fixture.root.join("legacy-probe.json"), &state, "probe").unwrap();
            legacy.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        legacy.sort_by(f64::total_cmp);
        let db_bytes = std::fs::metadata(database_path(&fixture.root)).unwrap().len();
        let wal_bytes = std::fs::metadata(format!("{}-wal", database_path(&fixture.root).display())).map(|m| m.len()).unwrap_or(0);
        let audit_bytes = std::fs::metadata(database_dir(&fixture.root).join(AUDIT_FILE)).map(|m| m.len()).unwrap_or(0);
        let exported = std::time::Instant::now();
        let export = export_previous(&fixture.root).unwrap();
        let export_ms = exported.elapsed().as_millis();
        println!("ROSTER_SCALE {}", serde_json::json!({
            "scale": scale, "source_bytes": source.len(), "rows": import.rows,
            "import_ms": import_ms, "audit_seed_ms": audit_ms, "cold_load_ms": cold_ms,
            "new_files_bytes_after_import": v2, "new_files_bytes_final": dir_bytes(&database_dir(&fixture.root)) - source.len() as u64 - std::fs::metadata(fixture.root.join("legacy-probe.json")).unwrap().len(),
            "one_row_write_ms_p50": writes[writes.len()/2], "one_row_write_ms_max": writes[writes.len()-1],
            "legacy_whole_write_ms_p50": legacy[2], "db_bytes": db_bytes, "wal_bytes": wal_bytes, "audit_bytes": audit_bytes,
            "export_ms": export_ms, "export_bytes": export.bytes,
        }));
    }
}

/// `extra.tool_action_receipts_v1` is one row per receipt in SQL; a new
/// receipt writes only its own row, and the R1 export folds the rows back
/// into the previous build's single object (also when the source had none).
#[test]
fn tool_receipts_are_rows_and_the_export_folds_them_back() {
    use crate::store::metrics::measure;
    let fixture = Fixture::new("tool-receipts");
    let original = fixture.write_previous(&eva_roster(1));
    import_legacy(&fixture.root).unwrap();
    std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    let snapshot = store.snapshot_shared().unwrap();
    assert_eq!(snapshot.tool_action_receipts.len(), 54);
    assert!(!snapshot.extra.contains_key(TOOL_RECEIPTS_EXTENSION));
    assert_eq!(snapshot.tool_action_receipt("tool-receipt-7").unwrap()["action_id"], "tool-7");
    // The first write also persists the read-time project states.
    store.with_state(true, |state| Ok((state, (), true))).unwrap();
    let ((), counts) = measure(|| {
        store.with_state(true, |mut state| {
            state.tool_action_receipts.insert("tool-receipt-new".into(), serde_json::json!({"caller": "c"}));
            Ok((state, (), true))
        }).unwrap()
    });
    assert_eq!((counts.rows_visited, counts.bytes_written), (1, br#"{"caller":"c"}"#.len() as u64), "one receipt row, not the whole object");
    // Untouched rows export byte-for-byte; the new one joins the object.
    export_previous(&fixture.root).unwrap();
    let exported = std::fs::read(fixture.legacy()).unwrap();
    let previous: AgentManagementSnapshot = serde_json::from_slice(&exported).unwrap();
    let receipts = previous.extra[TOOL_RECEIPTS_EXTENSION].as_object().unwrap();
    assert_eq!(receipts.len(), 55);
    assert_eq!(receipts["tool-receipt-new"]["caller"], "c");
    let mut expected: serde_json::Value = serde_json::from_slice(&original).unwrap();
    expected["tool_action_receipts_v1"]["tool-receipt-new"] = serde_json::json!({"caller": "c"});
    expected["store_revision"] = serde_json::json!(previous.store_revision);
    let mut actual: serde_json::Value = serde_json::from_slice(&exported).unwrap();
    actual["store_revision"] = expected["store_revision"].clone();
    // The warm-up write persisted the read-time project states.
    expected["project_states"] = actual["project_states"].clone();
    assert_eq!(actual, expected);

    // A source without the key: the first receipt creates it on export.
    let fresh = Fixture::new("tool-receipts-fresh");
    let mut state = eva_roster(1);
    state.extra.remove(TOOL_RECEIPTS_EXTENSION);
    fresh.write_previous(&state);
    import_legacy(&fresh.root).unwrap();
    std::fs::write(fresh.root.join("agent-management-v1.lock"), b"").unwrap();
    let store = AgentManagementStore::open(&fresh.root).unwrap();
    store.with_state(true, |mut state| {
        state.tool_action_receipts.insert("only".into(), serde_json::json!(1));
        Ok((state, (), true))
    }).unwrap();
    export_previous(&fresh.root).unwrap();
    let previous: serde_json::Value = serde_json::from_slice(&std::fs::read(fresh.legacy()).unwrap()).unwrap();
    assert_eq!(previous[TOOL_RECEIPTS_EXTENSION], serde_json::json!({"only": 1}));

    // An empty receipt object round-trips byte-for-byte.
    let empty = Fixture::new("tool-receipts-empty");
    let mut state = eva_roster(1);
    state.extra.insert(TOOL_RECEIPTS_EXTENSION.into(), serde_json::json!({}));
    let original = empty.write_previous(&state);
    import_legacy(&empty.root).unwrap();
    export_previous(&empty.root).unwrap();
    assert_eq!(std::fs::read(empty.legacy()).unwrap(), original);
}

/// 058 §7 gate: with a warm cache, a point read, a fixed page and a one-row
/// write (a real management API and a keyed `with_state` edit) do the same
/// store work over 1x and 10x inactive history, and none copies or
/// serialises the whole roster. Prints `ROSTER_EQUAL_WORK` per scale.
#[test]
fn roster_point_read_page_and_one_row_write_do_equal_work_at_1x_and_10x() {
    use crate::management::control_plane::{
        HumanManagementPresentationSchema, HumanManagementPresentationUpdateRequest, HumanManagementPrincipal,
    };
    use crate::store::metrics::{measure, OpCounts};
    let principal = HumanManagementPrincipal::authenticated();
    let agent = CutexSessionId::new(format!("cutex.{:08x}-synthetic-3", 3usize * 2654435761 % 0xffff_ffff)).unwrap();
    let project = ProjectId::new("project-synthetic-2").unwrap();
    let probe = AgentActionId::new("action-equal-work-probe").unwrap();
    let p50 = |samples: &mut Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    let mut runs: Vec<BTreeMap<&'static str, OpCounts>> = Vec::new();
    for scale in [1usize, 10] {
        let fixture = Fixture::new(&format!("equal-work-{scale}"));
        fixture.write_previous(&eva_roster(scale));
        import_legacy(&fixture.root).unwrap();
        std::fs::write(fixture.root.join("agent-management-v1.lock"), b"").unwrap();
        let provider = crate::agent_management::AgentManagementProvider::open(&fixture.root).unwrap();
        let store = provider.store();
        // A probe action with exactly five phase events: the same page at
        // every scale (the fixture's per-action event count varies).
        store.with_state(true, |mut state| {
            let mut action = state.actions.values().next().unwrap().clone();
            action.action_id = probe.clone();
            state.actions.insert(probe.clone(), action);
            let template = state.phase_events.values().next().unwrap().clone();
            for seq in 0..5u64 {
                let mut event = template.clone();
                event.event_id = format!("phase-equal-work-{seq}");
                event.action_id = probe.clone();
                event.phase_sequence = seq;
                state.phase_events.insert(event.event_id.clone(), event);
            }
            Ok((state, (), true))
        }).unwrap();
        let mut ops = BTreeMap::new();
        let mut timings = BTreeMap::new();
        for round in 0..9u64 {
            let mut record = |name: &'static str, counts: OpCounts, ms: f64| {
                if round == 0 {
                    ops.insert(name, counts);
                }
                timings.entry(name).or_insert_with(Vec::new).push(ms);
            };
            let started = std::time::Instant::now();
            let (found, counts) = measure(|| {
                (store.agent(&agent).unwrap().is_some(), store.action(&probe).unwrap().is_some())
            });
            record("point_read", counts, started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(found, (true, true));

            let started = std::time::Instant::now();
            let (events, counts) = measure(|| store.phase_events_for_action(&probe).unwrap());
            record("page_phase_events_of_action", counts, started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(events.len(), 5);

            let request = HumanManagementPresentationUpdateRequest {
                schema: HumanManagementPresentationSchema::V1,
                project_id: project.clone(),
                expected_authority_epoch: None,
                expected_presentation_revision: Some(round),
                presentation: crate::agent_management::ProjectPresentationInput {
                    display_name: format!("Equal work {}", round % 2),
                    badge_label: "EW".into(),
                    color: crate::agent_management::ProjectPaletteColor::Magenta,
                },
            };
            let started = std::time::Instant::now();
            let (saved, counts) =
                measure(|| provider.update_project_presentation_for_management(&principal, &request).unwrap());
            record("one_row_write_management_presentation", counts, started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(saved.revision, round + 1);

            let started = std::time::Instant::now();
            let ((), counts) = measure(|| {
                store.with_state(true, |mut state| {
                    state.agents.get_mut(&agent).unwrap().spec.pin ^= true;
                    Ok((state, (), true))
                }).unwrap()
            });
            record("one_row_write_keyed_with_state", counts, started.elapsed().as_secs_f64() * 1e3);

            let started = std::time::Instant::now();
            let (pin, counts) = measure(|| store.agent(&agent).unwrap().unwrap().spec.pin);
            record("point_read_after_write", counts, started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(pin, round % 2 == 0);
        }
        let p50s: BTreeMap<_, _> = timings.iter_mut().map(|(name, samples)| (*name, p50(samples))).collect();
        println!("ROSTER_EQUAL_WORK {}", serde_json::json!({
            "scale": scale,
            "rows": store.snapshot_shared().unwrap().agents.len() + store.snapshot_shared().unwrap().phase_events.len(),
            "ops": ops.iter().map(|(name, c)| (name.to_string(), serde_json::json!({
                "statements": c.sql_statements, "rows_visited": c.rows_visited, "rows_materialized": c.rows_materialized,
                "bytes_read": c.bytes_read, "bytes_decoded": c.bytes_decoded, "bytes_encoded": c.bytes_encoded,
                "bytes_written": c.bytes_written, "whole_state_copies": c.whole_state_copies,
                "whole_state_serializations": c.whole_state_serializations,
            }))).collect::<serde_json::Map<_, _>>(),
            "p50_ms": p50s,
        }));
        runs.push(ops);
    }
    for (name, one) in &runs[0] {
        let ten = &runs[1][name];
        assert_eq!(one.whole_state_copies + ten.whole_state_copies, 0, "{name}: whole-state copy");
        assert_eq!(one.whole_state_serializations + ten.whole_state_serializations, 0, "{name}: whole-state encode");
        assert_eq!(
            (one.sql_statements, one.rows_visited, one.rows_materialized),
            (ten.sql_statements, ten.rows_visited, ten.rows_materialized),
            "{name}: statements/rows differ between 1x and 10x"
        );
        for (a, b, what) in [
            (one.bytes_read, ten.bytes_read, "read"),
            (one.bytes_decoded, ten.bytes_decoded, "decoded"),
            (one.bytes_encoded, ten.bytes_encoded, "encoded"),
            (one.bytes_written, ten.bytes_written, "written"),
        ] {
            // Same rows; only digits of counters/timestamps may differ.
            assert!(a.abs_diff(b) <= 16, "{name}: bytes {what} {a} at 1x vs {b} at 10x");
        }
    }
}

/// Opt-in check on a private copy of a real roster (never the live path):
/// `CUTEX_ROSTER_SQL_SAMPLE=<copy>`; prints sizes/hashes only.
#[test]
#[ignore = "explicit private copy of a real roster"]
fn real_roster_copy_round_trips_byte_for_byte() {
    let Some(sample) = std::env::var_os("CUTEX_ROSTER_SQL_SAMPLE") else { return };
    let fixture = Fixture::new("sample");
    std::fs::copy(sample, fixture.legacy()).unwrap();
    let original = std::fs::read(fixture.legacy()).unwrap();
    let started = std::time::Instant::now();
    let import = import_legacy(&fixture.root).unwrap();
    let import_ms = started.elapsed().as_millis();
    let export = export_previous(&fixture.root).unwrap();
    let same = std::fs::read(fixture.legacy()).unwrap() == original;
    println!("ROSTER_SAMPLE {}", serde_json::json!({"bytes": original.len(), "rows": import.rows, "import_ms": import_ms, "export_ms": export.millis, "byte_identical": same}));
    assert!(same);
}

/// Binary-level R1 rehearsal step 1: write the synthetic EVA roster as the
/// previous build would (`CUTEX_ROSTER_SQL_REHEARSAL_ROOT` = a private
/// home's `runtime/agent-management/v1`).
#[test]
#[ignore = "binary R1 rehearsal step"]
fn rehearsal_step_write_previous_fixture() {
    let Some(root) = std::env::var_os("CUTEX_ROSTER_SQL_REHEARSAL_ROOT") else { return };
    let root = PathBuf::from(root);
    std::fs::create_dir_all(&root).unwrap();
    crate::config::atomic::write_private_pretty_json_atomic(&root.join(LEGACY_FILE), &eva_roster(1), "fixture").unwrap();
    std::fs::write(root.join("agent-management-v1.lock"), b"").unwrap();
}

/// Binary-level R1 rehearsal step 2 (after `cutex storage roster-import`):
/// new-build writes through the store — create, update, delete, an
/// unfinished action, a revoked grant set, a new history event and a foreign
/// additive key. Prints the expected post-write facts.
#[test]
#[ignore = "binary R1 rehearsal step"]
fn rehearsal_step_new_writes() {
    let Some(root) = std::env::var_os("CUTEX_ROSTER_SQL_REHEARSAL_ROOT") else { return };
    let root = PathBuf::from(root);
    assert!(is_authoritative(&database_path(&root)).unwrap(), "run roster-import first");
    let store = AgentManagementStore::open(&root).unwrap();
    let p0 = ProjectId::new("project-synthetic-0").unwrap();
    store.with_state(true, |mut state| {
        let mut agent = state.agents.values().next().unwrap().clone();
        agent.cutex_session_id = CutexSessionId::new("cutex.created-after-upgrade").unwrap();
        agent.spec.name = "created-after-upgrade".into();
        state.agents.insert(agent.cutex_session_id.clone(), agent);
        let renamed = state.agents.keys().next().unwrap().clone();
        state.agents.get_mut(&renamed).unwrap().spec.name = "renamed-after-upgrade".into();
        let membership = state.current_project_memberships.keys().next().unwrap().clone();
        state.current_project_memberships.remove(&membership);
        let action = state.actions.keys().last().unwrap().clone();
        let record = state.actions.get_mut(&action).unwrap();
        record.phase = AgentActionPhase::NativeBootstrapPending;
        record.response = None;
        state.operator_grants.get_mut(&p0).unwrap().clear();
        *state.operator_grant_revisions.get_mut(&p0).unwrap() += 1;
        let mut event = state.phase_events.values().next().unwrap().clone();
        event.event_id = "phase-after-upgrade".into();
        event.action_id = action.clone();
        state.phase_events.insert(event.event_id.clone(), event);
        state.extra.insert("written_after_upgrade".into(), serde_json::json!({"kept": true}));
        Ok((state, (), true))
    }).unwrap();
    let after = store.snapshot().unwrap();
    println!("REHEARSAL_EXPECT {}", serde_json::json!({
        "store_revision": after.store_revision,
        "agents": after.agents.len(),
        "memberships": after.current_project_memberships.len(),
        "phase_events": after.phase_events.len(),
        "sha256_of_committed": format!("{:x}", Sha256::digest(serde_json::to_vec_pretty(&read_committed(&root).unwrap().unwrap()).unwrap())),
    }));
}


#[test]
fn startup_recovery_and_selected_witnesses_do_not_load_unrelated_history() {
    let mut measured = Vec::new();
    for scale in [1,10] {
        let fixture = Fixture::new("real-startup");
        let mut state = eva_roster(1);
        let history = eva_roster(scale);
        state.actions = history.actions;
        state.phase_events = history.phase_events;
        state.failure_events = history.failure_events;
        let keys = state.actions.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            let row = state.actions.get_mut(&key).unwrap();
            row.operation = AgentOperationKind::QueryManaged;
            row.phase = AgentActionPhase::Complete;
        }
        let action = state.actions.values().next().unwrap().clone();
        fixture.write_previous(&state);
        std::fs::write(fixture.root.join("agent-management-v1.lock"),b"").unwrap();
        let provider = crate::agent_management::AgentManagementProvider::open(&fixture.root).unwrap()
            .with_authority_hook(std::sync::Arc::new(crate::director_address::management::ManagementHook::new(
                std::sync::Arc::new(crate::director_address::management::MemoryExtensionStore::default()))));
        let store = provider.store();store.import_now().unwrap();
        let (_, counts) = crate::store::metrics::measure(|| {
            assert_eq!(provider.finish_removed_director_rotations().unwrap(),0);
            provider.recover_authority_hook().unwrap();
            let selected = store.directory_for_actions(&[action.action_id.clone()]).unwrap();
            assert_eq!(selected.actions.len(),1);
            assert_eq!(selected.actions.get(&action.action_id),Some(&action));
            let by_message = store.directory_for_starts(&[action.external_message_id.clone().unwrap()]).unwrap();
            assert_eq!(by_message.actions.len(),1);
            assert_eq!(by_message.actions.get(&action.action_id),Some(&action));
            assert!(store.directory_snapshot_shared().unwrap().actions.is_empty(),"selected actions must not enter the directory cache");
        });
        assert_eq!(counts.whole_state_copies,0,"{counts:?}");
        assert!(counts.bytes_decoded>0 && counts.rows_materialized>0);
        measured.push((counts.bytes_decoded,counts.rows_materialized));
    }
    assert_eq!(measured[0],measured[1]);
}

#[test]
fn selected_start_witnesses_keep_duplicate_refusal_and_foreign_changes() {
    let fixture = Fixture::new("selected-starts");
    let state = eva_roster(1);fixture.write_previous(&state);
    std::fs::write(fixture.root.join("agent-management-v1.lock"),b"").unwrap();
    let store = AgentManagementStore::open(&fixture.root).unwrap();
    let keys = state.actions.keys().take(2).cloned().collect::<Vec<_>>();
    let external = state.actions[&keys[0]].external_message_id.clone().unwrap();
    assert_eq!(store.directory_for_starts(&[external.clone()]).unwrap().actions.len(),1);
    let full = store.snapshot().unwrap();let mut next = full.clone();
    next.actions.get_mut(&keys[1]).unwrap().external_message_id = Some(external.clone());
    next.store_revision+=1;
    commit(&database_path(&fixture.root),full.store_revision,&next.schema,next.store_revision,&diff(&full,&next).unwrap()).unwrap();
    let duplicated = store.directory_for_starts(&[external.clone()]).unwrap();
    assert_eq!(duplicated.actions.len(),2,"two witnesses must remain ambiguous");
    let mut deleted = next.clone();deleted.actions.remove(&keys[0]);deleted.store_revision+=1;
    commit(&database_path(&fixture.root),next.store_revision,&deleted.schema,deleted.store_revision,&diff(&next,&deleted).unwrap()).unwrap();
    assert_eq!(store.directory_for_starts(&[external]).unwrap().actions.len(),1);
    assert_eq!(duplicated.actions.len(),2,"old witness view stays immutable");
}
