use crate::asks::store::AskState;
use std::time::Duration;

pub const ASK_BYTES: usize = 20 * 1024 * 1024;
pub const ROSTER_BYTES: usize = 3_064_923;
pub const OWNERSHIP_BYTES: usize = 915_861;
pub fn asks() -> AskState {
    let mut state = AskState::default();
    let (mut ask, _) = state
        .create_ask(
            crate::asks::store::tests::new_ask("scale-seed", "seed"),
            chrono::Utc::now(),
        )
        .unwrap();
    state.asks.clear();
    state.actions.clear();
    state.notices.clear();
    ask.body = crate::asks::store::tests::body(&"synthetic-content ".repeat(ASK_BYTES / 1024 / 18));
    for n in 0..1024 {
        ask.ask_id = format!("scale-{n}");
        ask.action_id = format!("scale-action-{n}");
        state.asks.insert(ask.ask_id.clone(), ask.clone().into());
    }
    state
}
pub fn roster() -> crate::agent_management::AgentManagementSnapshot {
    let mut state = crate::agent_management::AgentManagementSnapshot::empty();
    // Additive retained metadata is legal schema data and has no credentials.
    // Many bounded rows model historical receipt allocation, rather than one
    // giant string that would hide the cost of cloning tree nodes.
    for n in 0..3072 {
        state.extra.insert(
            format!("synthetic-{n}"),
            serde_json::json!({"text":"receipt ".repeat(118)}),
        );
    }
    state
}
/// The same retained history plus a canonical 64-agent roster for authority
/// and concurrency regressions. The timing probe keeps its original fixture
/// unchanged so its recorded before/after samples remain comparable.
pub fn roster_with_agents() -> crate::agent_management::AgentManagementSnapshot {
    use crate::agent_management::{ManagedAgentRecord, ManagedAgentSpec};
    use crate::role_revision::{CutexSessionId, Rfc3339};
    let mut state = roster();
    for n in 0..64 {
        let id = CutexSessionId::new(format!("cutex.scale-{n}")).unwrap();
        state.agents.insert(
            id.clone(),
            ManagedAgentRecord {
                project_id: None,
                created_by_director_session: None,
                created_by_operator_session: None,
                cutex_session_id: id,
                native_session_id: format!("native-scale-{n}"),
                spec: ManagedAgentSpec {
                    name: format!("synthetic-{n}"),
                    cwd: std::env::temp_dir().to_string_lossy().into_owned(),
                    profile: Some("synthetic-profile".into()),
                    runtime_backend: "host".into(),
                    model: "synthetic-model".into(),
                    reasoning: "medium".into(),
                    permissions: "default".into(),
                    approval_policy: "never".into(),
                    sandbox_mode: "workspace-write".into(),
                    groups: vec!["synthetic".into()],
                    expose_to_im: false,
                    pin: false,
                },
                created_at: Rfc3339::new("2026-10-31T00:00:00Z").unwrap(),
                retired_at: None,
            },
        );
    }
    state
}
pub fn emit(case: &str, samples: &[Duration], work_units: u64, scale: serde_json::Value) {
    let mut ms: Vec<f64> = samples.iter().map(|t| t.as_secs_f64() * 1000.).collect();
    ms.sort_by(f64::total_cmp);
    let q = |p: f64| ms[((ms.len() - 1) as f64 * p).ceil() as usize];
    println!(
        "WASTE_SCALE {}",
        serde_json::json!({"area":"A","case":case,"phase":std::env::var("CUTEX_SCALE_PHASE").unwrap_or_else(|_|"unspecified".into()),"scale":scale,"iterations":ms.len(),"p50_ms":q(0.5),"p95_ms":q(0.95),"p99_ms":q(0.99),"work_units":work_units})
    );
}

/// Reusable offline stores in the runner's marked private home. No launch.
#[test]
#[ignore = "explicit marked-home fixture generator; no live/model IO"]
fn waste_scale_generate_private_home() {
    use crate::agent_management::{AgentManagementStore, ProjectAuthority, ProjectId};
    use crate::role_revision::{CutexSessionId, Rfc3339};
    let home = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private fixture runner"),
    );
    assert!(home.join(".cutex-test-private-home").is_file());
    assert_eq!(
        std::env::var_os("HOME").map(std::path::PathBuf::from),
        Some(home.clone())
    );
    let mut roster = roster_with_agents();
    for (n, id) in [
        crate::asks::store::tests::REQUESTER,
        crate::asks::store::tests::TARGET,
    ]
    .iter()
    .enumerate()
    {
        let old = CutexSessionId::new(format!("cutex.scale-{n}")).unwrap();
        let mut agent = roster.agents.remove(&old).unwrap();
        agent.cutex_session_id = CutexSessionId::new(*id).unwrap();
        roster.agents.insert(agent.cutex_session_id.clone(), agent);
    }
    let project = ProjectId::new("alpha").unwrap();
    roster.agents.for_each_value_mut(|_, agent| agent.project_id = Some(project.clone()));
    roster.projects.insert(
        project.clone(),
        ProjectAuthority {
            project_id: project,
            authorized_director_session: CutexSessionId::new(crate::asks::store::tests::REQUESTER)
                .unwrap(),
            authority_epoch: 1,
            updated_at: Rfc3339::new("2026-10-31T00:00:00Z").unwrap(),
        },
    );
    let sessions_path = crate::session::store::cutex_sessions_path().unwrap();
    crate::session::store::with_locked_session_store(&sessions_path, |sessions| {
        for agent in roster.agents.values() {
            let mut row = crate::session::model::CutexSessionRecord::new(
                agent.cutex_session_id.as_str().into(),
                None,
                crate::platform::host::current_host_name(),
                agent.spec.cwd.clone(),
                agent.spec.profile.clone(),
            )?;
            row.agent_enabled = true;
            row.formal_agent_name = Some(agent.spec.name.clone());
            row.agent_groups = agent.spec.groups.clone();
            sessions.sessions.insert(row.cutex_session_id.clone(), row);
        }
        crate::session::store::save_locked_session_store(&sessions_path, sessions)
    })
    .unwrap();
    assert_eq!(
        crate::session::store::load_cutex_session_store()
            .unwrap()
            .sessions
            .len(),
        64
    );
    AgentManagementStore::open_default()
        .unwrap()
        .with_state(true, |_| Ok((roster, (), true)))
        .unwrap();
    crate::asks::store::AskStore::open_default()
        .unwrap()
        .with_state(|state| *state = asks())
        .unwrap();
    crate::director_address::Store::open_default()
        .unwrap()
        .update(|state| {
            for n in 0..1024 {
                state.extra.insert(
                    format!("fixture-retained-{n}"),
                    serde_json::json!("synthetic-history ".repeat(50)),
                );
            }
            Ok(())
        })
        .unwrap();
    println!(
        "WASTE_FIXTURE {}",
        serde_json::json!({"schema":1,"area":"A","private_home":home,"agents":64,"asks":1024,
        "asks_target_bytes":ASK_BYTES,"roster_target_bytes":ROSTER_BYTES,"ownership_target_bytes":OWNERSHIP_BYTES,
        "runtime_started":false,"credentials_copied":false})
    );
}
