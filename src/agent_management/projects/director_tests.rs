use super::*;
use crate::agent_management::{AgentActionId, ManagedAgentSpec};

fn session(value: &str) -> CutexSessionId {
    CutexSessionId::new(value).unwrap()
}

fn project(value: &str) -> ProjectId {
    ProjectId::new(value).unwrap()
}

fn timestamp() -> Rfc3339 {
    Rfc3339::new("2026-10-09T00:00:00Z").unwrap()
}

fn agent(project_id: Option<&ProjectId>, value: &str) -> ManagedAgentRecord {
    ManagedAgentRecord {
        project_id: project_id.cloned(),
        created_by_director_session: None,
        created_by_operator_session: None,
        cutex_session_id: session(value),
        native_session_id: format!("native-{value}"),
        spec: ManagedAgentSpec {
            name: value.to_string(),
            cwd: format!("/tmp/{value}"),
            profile: Some("default".to_string()),
            runtime_backend: "app_server".to_string(),
            model: "gpt-test".to_string(),
            reasoning: "medium".to_string(),
            permissions: "default".to_string(),
            approval_policy: "never".to_string(),
            sandbox_mode: "workspace-write".to_string(),
            groups: Vec::new(),
            expose_to_im: false,
            pin: false,
        },
        created_at: timestamp(),
        retired_at: None,
    }
}

struct Fixture {
    provider: AgentManagementProvider,
    root: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(agents: &[ManagedAgentRecord]) -> Fixture {
    let root =
        std::env::temp_dir().join(format!("cutex-optional-director-{}", uuid::Uuid::new_v4()));
    let provider = AgentManagementProvider::open(&root).unwrap();
    provider
        .store()
        .with_state(true, |mut state| {
            for record in agents {
                state
                    .agents
                    .insert(record.cutex_session_id.clone(), record.clone());
            }
            Ok((state, (), true))
        })
        .unwrap();
    Fixture { provider, root }
}

fn presentation() -> ProjectPresentationInput {
    ProjectPresentationInput {
        display_name: "Optional".to_string(),
        badge_label: "OP".to_string(),
        color: ProjectPaletteColor::Green,
    }
}

fn request(
    action: &str,
    project_id: &str,
    operation: HumanManagementProjectMutationKind,
) -> HumanManagementProjectMutationRequest {
    HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(action).unwrap(),
        project_id: project(project_id),
        expected_authority_epoch: None,
        expected_project_revision: None,
        operation,
    }
}

fn run(
    fixture: &Fixture,
    request: &HumanManagementProjectMutationRequest,
) -> Result<HumanManagementProjectMutationReceipt, AgentManagementError> {
    fixture.provider.execute_project_mutation_for_management(
        &HumanManagementPrincipal::authenticated(),
        request,
    )
}

fn summary_of(fixture: &Fixture, project_id: &str) -> CutexProjectSummary {
    let collection = fixture
        .provider
        .list_cutex_projects_for_management(&HumanManagementPrincipal::authenticated())
        .unwrap();
    collection
        .projects
        .into_iter()
        .chain(collection.archived_projects)
        .find(|summary| summary.project_id == project(project_id))
        .unwrap()
}

fn create_kind(
    director: Option<&str>,
    workspace_root: Option<String>,
) -> HumanManagementProjectMutationKind {
    HumanManagementProjectMutationKind::CreateProject {
        presentation: presentation(),
        workspace_root,
        director_cutex_session_id: director.map(session),
        create_workspace_root: false,
    }
}

fn assign(action: &str, project_id: &str, agent: &str) -> HumanManagementProjectMutationRequest {
    request(
        action,
        project_id,
        HumanManagementProjectMutationKind::AssignDirector {
            cutex_session_id: session(agent),
        },
    )
}

fn offline(id: &CutexSessionId) -> Result<AgentRuntimeObservation, AgentManagementError> {
    let _ = id;
    Err(AgentManagementError::NotFound("runtime"))
}

#[test]
fn project_without_director_is_listed_joined_archived_and_replayed() {
    let fixture = fixture(&[agent(None, "cutex.member")]);
    let dir = fixture.root.join("workspace");
    std::fs::create_dir_all(&dir).unwrap();
    let create = request(
        "create-alpha",
        "alpha",
        create_kind(None, Some(dir.to_string_lossy().into_owned())),
    );
    let receipt = run(&fixture, &create).unwrap();
    assert_eq!(receipt.project_revision, 1);
    assert_eq!(receipt.audit_event.kind, ProjectAuditKind::Created);
    assert!(receipt.project_state.as_ref().unwrap().workspace_root.is_some());
    // Exact replay returns the receipt; a different payload conflicts.
    assert_eq!(run(&fixture, &create).unwrap(), receipt);
    let mut changed = create.clone();
    changed.operation = create_kind(None, None);
    assert_eq!(
        run(&fixture, &changed),
        Err(AgentManagementError::Conflict("action_id_payload_conflict"))
    );
    assert_eq!(
        run(&fixture, &request("create-alpha-again", "alpha", create_kind(None, None))),
        Err(AgentManagementError::Conflict("project_id_already_used"))
    );

    let summary = summary_of(&fixture, "alpha");
    assert_eq!(summary.director_cutex_session_id, None);
    assert_eq!(summary.authority_epoch, 0);
    let json = serde_json::to_value(&summary).unwrap();
    assert!(json.get("director_cutex_session_id").is_none());
    let snapshot = fixture.provider.store().snapshot().unwrap();
    assert!(crate::memory::project_is_known(&snapshot, &project("alpha")));
    assert!(!snapshot.projects.contains_key(&project("alpha")));

    // Members join with the epoch-0 fence (the create-agent path).
    let mut add = request(
        "add-member",
        "alpha",
        HumanManagementProjectMutationKind::AddMember {
            cutex_session_id: session("cutex.member"),
        },
    );
    add.expected_authority_epoch = Some(0);
    add.expected_project_revision = Some(1);
    run(&fixture, &add).unwrap();
    let workspace = fixture
        .provider
        .read_cutex_project_for_management(
            &HumanManagementPrincipal::authenticated(),
            &project("alpha"),
            &offline,
        )
        .unwrap();
    assert!(workspace.director.is_none());
    assert_eq!(workspace.active_agents.len(), 1);

    // Archive / restore / remove need no Director seat.
    for (action, kind) in [
        ("archive", HumanManagementProjectMutationKind::Archive),
        ("restore", HumanManagementProjectMutationKind::Restore),
        ("archive-2", HumanManagementProjectMutationKind::Archive),
        ("remove", HumanManagementProjectMutationKind::Remove),
    ] {
        run(&fixture, &request(action, "alpha", kind)).unwrap();
    }
    let snapshot = fixture.provider.store().snapshot().unwrap();
    assert!(!project_exists(&snapshot, &project("alpha")));
    assert_eq!(
        current_project_id(&snapshot, &snapshot.agents[&session("cutex.member")]),
        None
    );
}

#[test]
fn assign_clear_and_reassign_keep_epochs_monotonic() {
    let fixture = fixture(&[
        agent(None, "cutex.first"),
        agent(None, "cutex.second"),
        agent(Some(&project("other")), "cutex.elsewhere"),
    ]);
    run(&fixture, &request("create", "beta", create_kind(None, None))).unwrap();
    // A member of another Project cannot become the Director.
    let err = run(&fixture, &assign("assign-elsewhere", "beta", "cutex.elsewhere")).unwrap_err();
    assert!(err.to_string().contains("director_already_has_active_project"), "{err}");

    let first = assign("assign-first", "beta", "cutex.first");
    let assigned = run(&fixture, &first).unwrap();
    assert_eq!(assigned.audit_event.kind, ProjectAuditKind::DirectorAssigned);
    assert_eq!(
        assigned.membership.as_ref().unwrap().project_id,
        Some(project("beta"))
    );
    assert_eq!(run(&fixture, &first).unwrap(), assigned);
    let summary = summary_of(&fixture, "beta");
    assert_eq!(summary.director_cutex_session_id, Some(session("cutex.first")));
    assert_eq!(summary.authority_epoch, 1);
    let err = run(&fixture, &assign("assign-second-early", "beta", "cutex.second")).unwrap_err();
    assert!(err.to_string().contains("project_has_director"), "{err}");

    // clear_director refuses while Agent Operators exist.
    fixture
        .provider
        .store()
        .with_state(true, |mut state| {
            state.operator_grants.entry(project("beta")).or_default().insert(
                session("cutex.second"),
                AgentOperatorGrant {
                    project_id: project("beta"),
                    operator_cutex_session_id: session("cutex.second"),
                    grant_revision: 1,
                    granted_at: timestamp(),
                    granted_by_primary_director_session: session("cutex.first"),
                    performed_by_human_management: true,
                },
            );
            Ok((state, (), true))
        })
        .unwrap();
    let clear = request("clear", "beta", HumanManagementProjectMutationKind::ClearDirector);
    let err = run(&fixture, &clear).unwrap_err();
    assert!(err.to_string().contains("project_has_agent_operators: revoke"), "{err}");
    fixture
        .provider
        .store()
        .with_state(true, |mut state| {
            state.operator_grants.remove(&project("beta"));
            Ok((state, (), true))
        })
        .unwrap();

    let cleared = run(&fixture, &clear).unwrap();
    assert_eq!(cleared.audit_event.kind, ProjectAuditKind::DirectorCleared);
    assert_eq!(run(&fixture, &clear).unwrap(), cleared);
    let summary = summary_of(&fixture, "beta");
    assert_eq!(summary.director_cutex_session_id, None);
    assert_eq!(summary.authority_epoch, 1);
    let snapshot = fixture.provider.store().snapshot().unwrap();
    // The former Director stays a member and loses the Director role.
    assert_eq!(
        current_project_id(&snapshot, &snapshot.agents[&session("cutex.first")]),
        Some(project("beta"))
    );
    assert!(!crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.first").director);
    assert_eq!(
        run(&fixture, &request("clear-again", "beta", HumanManagementProjectMutationKind::ClearDirector)),
        Err(AgentManagementError::Conflict("project_has_no_director"))
    );

    let mut reassign = assign("assign-second", "beta", "cutex.second");
    reassign.expected_authority_epoch = Some(1);
    run(&fixture, &reassign).unwrap();
    let summary = summary_of(&fixture, "beta");
    assert_eq!(summary.director_cutex_session_id, Some(session("cutex.second")));
    assert_eq!(summary.authority_epoch, 2);
}

#[test]
fn create_with_director_and_workspace_root_in_one_request() {
    let fixture = fixture(&[agent(None, "cutex.lead"), agent(Some(&project("x")), "cutex.busy")]);
    let dir = fixture.root.join("lead workspace");
    std::fs::create_dir_all(&dir).unwrap();
    let err = run(
        &fixture,
        &request("create-busy", "delta", create_kind(Some("cutex.busy"), None)),
    )
    .unwrap_err();
    assert!(err.to_string().contains("director_already_has_active_project"), "{err}");
    let receipt = run(
        &fixture,
        &request(
            "create-with-lead",
            "gamma",
            create_kind(Some("cutex.lead"), Some(dir.to_string_lossy().into_owned())),
        ),
    )
    .unwrap();
    assert_eq!(
        receipt.membership.as_ref().unwrap().cutex_session_id,
        session("cutex.lead")
    );
    let summary = summary_of(&fixture, "gamma");
    assert_eq!(summary.director_cutex_session_id, Some(session("cutex.lead")));
    assert!(summary.workspace_root.is_some());
    // r219: Agent Management only; the removed seat store is never created.
    assert!(!fixture.root.join("task-service-seat-authority-v1").exists());
}

#[test]
fn legacy_store_without_project_states_migrates_on_load() {
    let fixture = fixture(&[agent(Some(&project("legacy")), "cutex.legacy-director")]);
    // The pre-migration shape: an authority without a project_states record.
    let path = fixture.root.join("agent-management-v1.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["projects"] = serde_json::json!({
        "legacy": {
            "project_id": "legacy",
            "authorized_director_session": "cutex.legacy-director",
            "authority_epoch": 3,
            "updated_at": "2026-10-01T00:00:00Z"
        }
    });
    json["project_states"] = serde_json::json!({});
    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    crate::agent_management::store::invalidate_snapshot_cache();
    let snapshot = fixture.provider.store().snapshot().unwrap();
    let state = &snapshot.project_states[&project("legacy")];
    assert_eq!(state.lifecycle, ProjectLifecycle::Active);
    assert_eq!(state.revision, 0);
    assert_eq!(state.created_at.as_str(), "2026-10-01T00:00:00Z");
    assert_eq!(summary_of(&fixture, "legacy").authority_epoch, 3);
}

#[test]
fn director_role_follows_the_authority_through_clear_and_reassign() {
    let fixture = fixture(&[agent(None, "cutex.old"), agent(None, "cutex.new")]);
    run(
        &fixture,
        &request(
            "legacy-create",
            "seated",
            HumanManagementProjectMutationKind::Create {
                director_cutex_session_id: session("cutex.old"),
                presentation: presentation(),
            },
        ),
    )
    .unwrap();
    let snapshot = fixture.provider.store().snapshot().unwrap();
    assert!(crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.old").director);
    run(&fixture, &request("clear", "seated", HumanManagementProjectMutationKind::ClearDirector)).unwrap();
    let snapshot = fixture.provider.store().snapshot().unwrap();
    assert!(!crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.old").director);
    run(&fixture, &assign("assign-new", "seated", "cutex.new")).unwrap();
    let snapshot = fixture.provider.store().snapshot().unwrap();
    assert!(crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.new").director);
    assert!(!crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.old").director);
    assert_eq!(
        summary_of(&fixture, "seated").director_cutex_session_id,
        Some(session("cutex.new"))
    );
    run(&fixture, &request("archive", "seated", HumanManagementProjectMutationKind::Archive)).unwrap();
    assert_eq!(summary_of(&fixture, "seated").lifecycle, ProjectLifecycle::Archived);
}

fn rotate(action: &str, project_id: &str, successor: &str, epoch: Option<u64>) -> HumanManagementProjectMutationRequest {
    let mut request = request(
        action,
        project_id,
        HumanManagementProjectMutationKind::RotateDirector {
            successor_cutex_session_id: session(successor),
        },
    );
    request.expected_authority_epoch = epoch;
    request
}

#[test]
fn rotate_director_moves_the_seat_in_one_fenced_commit() {
    let fixture = fixture(&[
        agent(None, "cutex.old"),
        agent(None, "cutex.new"),
        agent(None, "cutex.operator"),
        agent(Some(&project("other")), "cutex.elsewhere"),
    ]);
    run(&fixture, &request("create", "gamma", create_kind(Some("cutex.old"), None))).unwrap();
    run(&fixture, &request("no-director", "empty", create_kind(None, None))).unwrap();
    // The epoch fence is required.
    assert_eq!(
        run(&fixture, &rotate("rotate-unfenced", "gamma", "cutex.new", None)),
        Err(AgentManagementError::InvalidRequest("rotate_director_requires_authority_epoch"))
    );
    assert_eq!(
        run(&fixture, &rotate("rotate-stale", "gamma", "cutex.new", Some(7))),
        Err(AgentManagementError::Conflict("stale_project_authority"))
    );
    assert_eq!(
        run(&fixture, &rotate("rotate-empty", "empty", "cutex.new", Some(0))),
        Err(AgentManagementError::Conflict("project_has_no_director"))
    );
    assert_eq!(
        run(&fixture, &rotate("rotate-self", "gamma", "cutex.old", Some(1))),
        Err(AgentManagementError::Conflict("successor_is_current_director"))
    );
    let err = run(&fixture, &rotate("rotate-elsewhere", "gamma", "cutex.elsewhere", Some(1))).unwrap_err();
    assert!(err.to_string().contains("director_already_has_active_project"), "{err}");
    // An Operator grant survives the rotation; an Operator cannot take the seat.
    fixture
        .provider
        .store()
        .with_state(true, |mut state| {
            state.current_project_memberships.insert(
                session("cutex.operator"),
                CurrentProjectMembership {
                    cutex_session_id: session("cutex.operator"),
                    project_id: Some(project("gamma")),
                    revision: 1,
                    updated_at: timestamp(),
                },
            );
            state.operator_grants.entry(project("gamma")).or_default().insert(
                session("cutex.operator"),
                AgentOperatorGrant {
                    project_id: project("gamma"),
                    operator_cutex_session_id: session("cutex.operator"),
                    grant_revision: 1,
                    granted_at: timestamp(),
                    granted_by_primary_director_session: session("cutex.old"),
                    performed_by_human_management: true,
                },
            );
            Ok((state, (), true))
        })
        .unwrap();
    assert_eq!(
        run(&fixture, &rotate("rotate-operator", "gamma", "cutex.operator", Some(1))),
        Err(AgentManagementError::Conflict("primary_director_cannot_be_operator"))
    );

    let rotation = rotate("rotate", "gamma", "cutex.new", Some(1));
    let receipt = run(&fixture, &rotation).unwrap();
    assert_eq!(receipt.audit_event.kind, ProjectAuditKind::DirectorRotated);
    assert_eq!(receipt.audit_event.member_cutex_session_id, Some(session("cutex.new")));
    assert_eq!(receipt.membership.as_ref().unwrap().project_id, Some(project("gamma")));
    // Exact replay returns the same receipt even though the epoch moved on.
    assert_eq!(run(&fixture, &rotation).unwrap(), receipt);
    let summary = summary_of(&fixture, "gamma");
    assert_eq!(summary.director_cutex_session_id, Some(session("cutex.new")));
    assert_eq!(summary.authority_epoch, 2);
    let snapshot = fixture.provider.store().snapshot().unwrap();
    // The former Director stays a member without the Director role.
    assert_eq!(
        current_project_id(&snapshot, &snapshot.agents[&session("cutex.old")]),
        Some(project("gamma"))
    );
    assert!(!crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.old").director);
    assert!(crate::agent_bus::tool_policy::roles_from(Some(&snapshot), "cutex.new").director);
    assert!(snapshot.operator_grants[&project("gamma")].contains_key(&session("cutex.operator")));
    // The old fence is stale now.
    assert_eq!(
        run(&fixture, &rotate("rotate-back-stale", "gamma", "cutex.old", Some(1))),
        Err(AgentManagementError::Conflict("stale_project_authority"))
    );
    run(&fixture, &rotate("rotate-back", "gamma", "cutex.old", Some(2))).unwrap();
    assert_eq!(summary_of(&fixture, "gamma").authority_epoch, 3);
}

#[test]
fn create_project_creates_a_missing_workspace_root_only_when_asked() {
    let fixture = fixture(&[agent(None, "cutex.lead"), agent(Some(&project("x")), "cutex.busy")]);
    let missing = fixture.root.join("new parent").join("new project");
    let root = missing.to_string_lossy().into_owned();
    // Without the flag: refused, nothing created.
    let err = run(&fixture, &request("create-plain", "plain", create_kind(None, Some(root.clone())))).unwrap_err();
    assert!(err.to_string().contains("workspace_root_not_found"), "{err}");
    assert!(!missing.exists());
    let with_flag = |action: &str, id: &str, director: Option<&str>, root: String| {
        let HumanManagementProjectMutationKind::CreateProject { presentation, workspace_root, director_cutex_session_id, .. } =
            create_kind(director, Some(root))
        else {
            unreachable!()
        };
        request(
            action,
            id,
            HumanManagementProjectMutationKind::CreateProject {
                presentation,
                workspace_root,
                director_cutex_session_id,
                create_workspace_root: true,
            },
        )
    };
    // A refused Director creates no directory.
    let err = run(&fixture, &with_flag("create-busy", "busy", Some("cutex.busy"), root.clone())).unwrap_err();
    assert!(err.to_string().contains("director_already_has_active_project"), "{err}");
    assert!(!missing.exists());
    // `..` components and a file in the way are refused.
    let dotted = fixture.root.join("a").join("..").join("b").to_string_lossy().into_owned();
    assert!(run(&fixture, &with_flag("create-dots", "dots", None, dotted)).unwrap_err().to_string().contains("invalid_workspace_root"));
    std::fs::create_dir_all(&fixture.root).unwrap();
    std::fs::write(fixture.root.join("file"), b"x").unwrap();
    let under_file = fixture.root.join("file").join("child").to_string_lossy().into_owned();
    assert!(run(&fixture, &with_flag("create-file", "file", None, under_file)).unwrap_err().to_string().contains("workspace_root_not_directory"));
    // With the flag: created (both levels) and stored canonical; an existing directory is fine too.
    let receipt = run(&fixture, &with_flag("create-new", "fresh", None, root.clone())).unwrap();
    assert!(missing.is_dir());
    assert_eq!(
        receipt.project_state.as_ref().unwrap().workspace_root.as_deref(),
        Some(canonical_workspace_root(&root).unwrap().as_str())
    );
    run(&fixture, &with_flag("create-again", "again", None, root)).unwrap();
    // The flag is left out of the request when false (old idempotency digests stay valid).
    let plain = serde_json::to_value(create_kind(None, None)).unwrap();
    assert!(plain.get("create_workspace_root").is_none(), "{plain}");
}
