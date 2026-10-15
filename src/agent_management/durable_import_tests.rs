use super::*;
use crate::agent_bus::model::AgentBusAgent;
use crate::session::store::{load_cutex_session_store_from_path, save_cutex_session_store_to_path};

struct Fixture {
    root: std::path::PathBuf,
    path: std::path::PathBuf,
    provider: AgentManagementProvider,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cutex-durable-import-test-{}",
            uuid::Uuid::new_v4()
        ));
        let provider = AgentManagementProvider::open(root.join("provider")).unwrap();
        Self {
            path: root.join("sessions.json"),
            root,
            provider,
        }
    }
    fn add(&self, id: &str, name: Option<&str>) {
        let mut sessions = load_cutex_session_store_from_path(&self.path).unwrap();
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            Some(format!("native-{id}")),
            crate::platform::host::current_host_name(),
            self.root.to_string_lossy().into_owned(),
            None,
            "2026-09-08T00:00:00Z".into(),
        )
        .unwrap();
        record.thread_name = Some("This is a native title, not an Agent name".into());
        if name.is_none() {
            record.display_name_hint = record.thread_name.clone();
        }
        sessions.sessions.insert(id.into(), record);
        // Construct the durable fixture directly; importing is independent of
        // native rollout adoption and must not launch/migrate real runtimes.
        let r = sessions.sessions.get_mut(id).unwrap();
        r.formal_agent_name = name.map(str::to_string);
        r.agent_enabled = true;
        r.registration_class = AgentRegistrationClass::Persistent;
        r.agent_groups = vec!["test".into()];
        save_cutex_session_store_to_path(&self.path, &sessions).unwrap();
    }
    fn candidate(&self, id: &str) -> DurableAgentCandidate {
        self.provider
            .durable_agent_candidates(&HumanManagementPrincipal::authenticated(), &self.path)
            .unwrap()
            .into_iter()
            .find(|c| {
                c.cutex_session_id
                    .as_ref()
                    .is_some_and(|value| value.as_str() == id)
            })
            .unwrap()
    }
    fn request(&self, id: &str, name: &str, action: &str) -> DurableImportRequest {
        DurableImportRequest {
            action_id: AgentActionId::new(action).unwrap(),
            candidate: self.candidate(id),
            confirmed_formal_name: name.into(),
            assignment: None,
            detach: None,
        }
    }
    fn run(
        &self,
        request: &DurableImportRequest,
    ) -> Result<DurableImportReceipt, AgentManagementError> {
        self.provider.import_durable_agent(
            &HumanManagementPrincipal::authenticated(),
            &self.path,
            request,
        )
    }
    fn mutate(&self, id: &str, edit: impl FnOnce(&mut CutexSessionRecord)) {
        let mut sessions = load_cutex_session_store_from_path(&self.path).unwrap();
        edit(sessions.sessions.get_mut(id).unwrap());
        save_cutex_session_store_to_path(&self.path, &sessions).unwrap();
    }
    fn create(&self, id: &str, project: &str) {
        let mut request = self.request(
            id,
            self.candidate(id).formal_name.as_deref().unwrap(),
            &format!("import-{project}"),
        );
        request.assignment = Some(create(id, project));
        let receipt = self.run(&request).unwrap();
        assert!(receipt.complete, "{:?}", receipt.error);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn create(id: &str, project: &str) -> HumanManagementProjectMutationRequest {
    HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(format!("create-{project}")).unwrap(),
        project_id: ProjectId::new(project).unwrap(),
        expected_authority_epoch: Some(0),
        expected_project_revision: Some(0),
        operation: HumanManagementProjectMutationKind::Create {
            director_cutex_session_id: CutexSessionId::new(id).unwrap(),
            presentation: ProjectPresentationInput {
                display_name: project.into(),
                badge_label: "CX".into(),
                color: ProjectPaletteColor::Cyan,
            },
        },
    }
}
fn member(
    id: &str,
    project: &str,
    action: &str,
    revision: u64,
    detach: bool,
) -> HumanManagementProjectMutationRequest {
    let id = CutexSessionId::new(id).unwrap();
    HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(action).unwrap(),
        project_id: ProjectId::new(project).unwrap(),
        expected_authority_epoch: Some(1),
        expected_project_revision: Some(revision),
        operation: if detach {
            HumanManagementProjectMutationKind::DetachMember {
                cutex_session_id: id,
            }
        } else {
            HumanManagementProjectMutationKind::AddMember {
                cutex_session_id: id,
            }
        },
    }
}

#[test]
fn rejected_rows_preserve_valid_candidates_and_reject_duplicate_native_ids() {
    let f = Fixture::new();
    f.add("cutex.valid", Some("Valid"));
    let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
    let mut malformed = sessions.sessions["cutex.valid"].clone();
    malformed.codex_session_id = Some("native-bad".into());
    sessions.sessions.insert("bad key".into(), malformed);
    save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
    let rows = f
        .provider
        .durable_agent_candidates(&HumanManagementPrincipal::authenticated(), &f.path)
        .unwrap();
    assert_eq!(rows.len(), 2);
    let bad = rows.iter().find(|r| r.raw_store_key == "bad key").unwrap();
    assert!(bad.cutex_session_id.is_none());
    assert!(bad.rejection.as_deref().unwrap().contains("malformed"));
    let invalid_request = DurableImportRequest {
        action_id: AgentActionId::new("bad").unwrap(),
        candidate: bad.clone(),
        confirmed_formal_name: "Bad".into(),
        assignment: None,
        detach: None,
    };
    assert!(f.run(&invalid_request).is_err());
    assert!(f.candidate("cutex.valid").rejection.is_none());
    f.create("cutex.valid", "alpha");
    sessions.sessions.remove("cutex.valid");
    save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
    let rows = f
        .provider
        .durable_agent_candidates(&HumanManagementPrincipal::authenticated(), &f.path)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].cutex_session_id.is_none() && rows[0].rejection.is_some());

    f.add("cutex.one", Some("Same"));
    f.add("cutex.two", Some("Same"));
    f.mutate("cutex.two", |r| {
        r.codex_session_id = Some("native-cutex.one".into())
    });
    for id in ["cutex.one", "cutex.two"] {
        assert_eq!(
            f.candidate(id).rejection.as_deref(),
            Some("ambiguous_durable_native_identity")
        );
    }
    // Corrupt whole stores must remain an error, not an empty successful query.
    // (The SQLite database since session store Phase 2.)
    std::fs::write(crate::session::store::session_database_path(&f.path), b"not json").unwrap();
    assert!(f
        .provider
        .durable_agent_candidates(&HumanManagementPrincipal::authenticated(), &f.path)
        .is_err());
}

#[test]
fn pre_repair_candidate_wire_shape_replays_without_digest_changes() {
    let f = Fixture::new();
    f.add("cutex.legacy-wire", Some("Original"));
    let mut request = f.request("cutex.legacy-wire", "Original", "legacy-wire-import");
    request.candidate.raw_store_key.clear();
    let wire = serde_json::to_value(&request).unwrap();
    assert!(wire["candidate"].get("raw_store_key").is_none());
    let request: DurableImportRequest = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&request).unwrap(), wire);
    let original = f.run(&request).unwrap();
    assert!(original.complete);
    f.mutate("cutex.legacy-wire", |r| {
        r.formal_agent_name = Some("Renamed".into())
    });
    assert_eq!(f.run(&request).unwrap(), original);
}

#[test]
fn offline_named_and_historical_adopt_import_create_add_and_profile_remains_mutable() {
    let f = Fixture::new();
    f.add("cutex.director", Some("Formal Director"));
    let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
    assert!(!f.candidate("cutex.director").online);
    assert!(!f.candidate("cutex.director").in_roster);
    assert_eq!(
        crate::session::store::session_store_json_bytes(&f.path).unwrap(),
        before,
        "candidate selection cannot write"
    );
    f.create("cutex.director", "alpha");
    f.add("cutex.worker", None);
    assert_eq!(
        f.candidate("cutex.worker").formal_name,
        None,
        "never use hint/title"
    );
    let mut request = f.request("cutex.worker", "Explicit Worker", "worker-import-add");
    request.assignment = Some(member("cutex.worker", "alpha", "add-worker", 1, false));
    let receipt = f.run(&request).unwrap();
    assert!(
        receipt.complete && receipt.named && receipt.imported,
        "{:?}",
        receipt.error
    );
    let sessions = load_cutex_session_store_from_path(&f.path).unwrap();
    let record = &sessions.sessions["cutex.worker"];
    assert_eq!(record.profile, None);
    assert_eq!(record.formal_agent_name.as_deref(), Some("Explicit Worker"));
    assert_ne!(record.formal_agent_name, record.thread_name);
    assert!(sessions
        .formal_name_receipts
        .contains_key("worker-import-add"));
    f.mutate("cutex.worker", |record| {
        record.profile = Some("new-profile".into());
        record.bump_durable_revision().unwrap();
    });
    assert_eq!(
        f.candidate("cutex.worker")
            .current_project_id
            .unwrap()
            .as_str(),
        "alpha"
    );
    assert_eq!(
        f.run(&request).unwrap(),
        receipt,
        "completed replay is exact after profile changes"
    );
}

#[test]
fn runtime_registration_observation_does_not_stale_reviewed_import_create() {
    let f = Fixture::new();
    f.add("cutex.director", Some("Formal Director"));
    f.mutate("cutex.director", |record| {
        record.last_runtime_agent_id = Some("runtime-director".into());
    });
    let mut request = f.request(
        "cutex.director",
        "Formal Director",
        "observation-import-create",
    );
    request.assignment = Some(create("cutex.director", "observation-project"));
    let before = load_cutex_session_store_from_path(&f.path).unwrap();
    let before_record = before.sessions["cutex.director"].clone();
    let mut observed = before;
    let registration = AgentBusAgent {
        id: "runtime-director".into(),
        name: "runtime-director".into(),
        base_name: None,
        thread_name: None,
        path_key: None,
        session_id: None,
        cutex_session_id: None,
        profile: "not-authority".into(),
        cwd: "/not-authority".into(),
        pid: 4242,
        host_id: Some(crate::platform::host::current_host_name()),
        groups: Vec::new(),
        registration_class: AgentRegistrationClass::Persistent,
        last_seen_epoch_secs: 1,
    };
    let outcome =
        crate::session::runtime_reconciliation::reconcile_cutex_session_store_for_registration(
            &mut observed,
            &registration,
            &crate::platform::host::current_host_name(),
            "2026-09-09T01:02:03Z",
        )
        .unwrap();
    assert!(outcome.store_fence_required && outcome.outcome.is_none());
    let observed_record = &observed.sessions["cutex.director"];
    assert_eq!(observed_record.revision, before_record.revision);
    assert_ne!(observed_record.updated_at, before_record.updated_at);
    assert_ne!(
        observed_record.current_runtime_agent_id,
        before_record.current_runtime_agent_id
    );
    assert_eq!(
        durable_candidate_digest(observed_record).unwrap(),
        request.candidate.durable_sha256
    );
    save_cutex_session_store_to_path(&f.path, &observed).unwrap();
    let receipt = f.run(&request).unwrap();
    assert!(receipt.complete && receipt.imported, "{:?}", receipt.error);
    assert_eq!(
        f.candidate("cutex.director")
            .current_project_id
            .unwrap()
            .as_str(),
        "observation-project"
    );
    assert_eq!(f.run(&request).unwrap(), receipt);
}

#[test]
fn candidate_fence_excludes_only_presentation_and_runtime_observation_fields() {
    let f = Fixture::new();
    f.add("cutex.worker", Some("Worker"));
    let original = load_cutex_session_store_from_path(&f.path)
        .unwrap()
        .sessions["cutex.worker"]
        .clone();
    let digest = durable_candidate_digest(&original).unwrap();
    let mut observation = original.clone();
    observation.thread_name = Some("new native title".into());
    observation.display_name_hint = Some("new display hint".into());
    observation.pending_launch_id = Some("launch-observation".into());
    observation.alden_session_name = Some("alden-observation".into());
    observation.alden_pid = Some(10);
    observation.runtime_pid = Some(11);
    observation.current_runtime_agent_id = Some("runtime-observation".into());
    observation.runtime_generation = 99;
    observation.runtime_history_known = !observation.runtime_history_known;
    observation.last_runtime_agent_id = Some("old-runtime".into());
    observation.last_seen_at = Some("2026-09-09T02:00:00Z".into());
    observation.last_user_selected_at = Some("2026-09-09T02:00:01Z".into());
    observation.updated_at = "2026-09-09T02:00:02Z".into();
    assert_eq!(durable_candidate_digest(&observation).unwrap(), digest);

    for semantic in [
        "revision",
        "identity",
        "native",
        "name",
        "cwd",
        "profile",
        "archive",
        "registration",
        "permissions",
        "launch-claim",
    ] {
        let mut changed = original.clone();
        match semantic {
            "revision" => changed.revision += 1,
            "identity" => changed.cutex_session_id = "cutex.other".into(),
            "native" => changed.codex_session_id = Some("native-other".into()),
            "name" => changed.formal_agent_name = Some("Other".into()),
            "cwd" => changed.managed_cwd = Some("/other".into()),
            "profile" => changed.profile = Some("other".into()),
            "archive" => {
                changed.archive_state = crate::session::model::CutexSessionArchiveState::Retired
            }
            "registration" => changed.registration_class = AgentRegistrationClass::Ephemeral,
            "permissions" => changed.permission_defaults = Some("other".into()),
            "launch-claim" => changed.app_server_launch_claim_id = Some("claim".into()),
            _ => unreachable!(),
        }
        assert_ne!(
            durable_candidate_digest(&changed).unwrap(),
            digest,
            "{semantic}"
        );
    }
}

#[test]
fn former_whole_record_digest_requires_fresh_review_without_write() {
    let f = Fixture::new();
    f.add("cutex.worker", Some("Worker"));
    let mut request = f.request("cutex.worker", "Worker", "old-digest-import");
    let sessions = load_cutex_session_store_from_path(&f.path).unwrap();
    request.candidate.durable_sha256 =
        super::super::store::request_sha256(&sessions.sessions["cutex.worker"]).unwrap();
    assert_ne!(
        request.candidate.durable_sha256,
        durable_candidate_digest(&sessions.sessions["cutex.worker"]).unwrap()
    );
    assert_eq!(
        f.run(&request).unwrap_err(),
        conflict("stale_durable_candidate")
    );
    let state = f.provider.store().snapshot().unwrap();
    assert!(state.agents.is_empty() && state.durable_import_actions.is_empty());
}

#[test]
fn stale_identity_config_and_retirement_fail_closed_without_roster_write() {
    for field in [
        "profile",
        "name",
        "cwd",
        "retire",
        "registration",
        "identity",
    ] {
        let f = Fixture::new();
        f.add("cutex.worker", Some("Worker"));
        let request = f.request("cutex.worker", "Worker", "import");
        f.mutate("cutex.worker", |record| {
            match field {
                "profile" => record.profile = Some("changed".into()),
                "name" => record.formal_agent_name = Some("changed".into()),
                "cwd" => record.cwd = "/changed".into(),
                "retire" => {
                    record.archive_state = crate::session::model::CutexSessionArchiveState::Retired
                }
                "registration" => record.registration_class = AgentRegistrationClass::Ephemeral,
                "identity" => record.cutex_session_id = "cutex.other".into(),
                _ => unreachable!(),
            }
            record.bump_durable_revision().unwrap();
        });
        assert!(f.run(&request).is_err(), "{field}");
        let state = f.provider.store().snapshot().unwrap();
        assert!(state.agents.is_empty() && state.durable_import_actions.is_empty());
    }
}

#[test]
fn replay_after_retirement_is_exact_but_new_import_cannot_resurrect_roster() {
    let f = Fixture::new();
    f.add("cutex.worker", Some("Worker"));
    let request = f.request("cutex.worker", "Worker", "import");
    let receipt = f.run(&request).unwrap();
    assert!(receipt.complete);
    f.provider
        .store()
        .with_state(true, |mut state| {
            state
                .agents
                .get_mut(request.candidate.cutex_session_id.as_ref().unwrap())
                .unwrap()
                .retired_at = Some(super::super::now());
            Ok((state, (), true))
        })
        .unwrap();
    assert_eq!(f.run(&request).unwrap(), receipt);
    let mut changed = request.clone();
    changed.confirmed_formal_name = "Changed".into();
    assert_eq!(
        f.run(&changed).unwrap_err(),
        conflict("action_id_payload_conflict")
    );
    let new = f.request("cutex.worker", "Worker", "new-import");
    assert!(new.candidate.rejection.is_some());
    assert!(f.run(&new).is_err());
}

#[test]
fn partial_import_preserves_unassigned_receipt_and_stale_assignment_never_reports_success() {
    let f = Fixture::new();
    f.add("cutex.director", Some("Director"));
    f.create("cutex.director", "alpha");
    f.add("cutex.worker", None);
    let mut request = f.request("cutex.worker", "Worker", "partial");
    request.assignment = Some(member("cutex.worker", "alpha", "stale-add", 99, false));
    let receipt = f.run(&request).unwrap();
    assert!(receipt.named && receipt.imported && !receipt.complete);
    assert!(receipt.steps.is_empty());
    assert!(f.candidate("cutex.worker").current_project_id.is_none());
    let replay = f.run(&request).unwrap();
    assert!(!replay.complete);
    assert_eq!(replay.error, receipt.error);
    f.mutate("cutex.worker", |r| {
        r.profile = Some("changed".into());
        r.bump_durable_revision().unwrap();
    });
    let changed = f.run(&request).unwrap();
    assert!(!changed.complete);
    assert!(changed
        .error
        .unwrap()
        .contains("formal_name_receipt_conflict"));
}

#[test]
fn explicit_move_uses_source_destination_cas_and_director_guard() {
    let f = Fixture::new();
    f.add("cutex.a", Some("A"));
    f.create("cutex.a", "alpha");
    f.add("cutex.b", Some("B"));
    f.create("cutex.b", "beta");
    f.add("cutex.worker", Some("Worker"));
    let mut add = f.request("cutex.worker", "Worker", "add");
    add.assignment = Some(member("cutex.worker", "alpha", "add-alpha", 1, false));
    assert!(f.run(&add).unwrap().complete);
    let mut movement = f.request("cutex.worker", "Worker", "move");
    movement.assignment = Some(member("cutex.worker", "beta", "add-beta", 1, false));
    assert_eq!(
        f.run(&movement).unwrap_err(),
        conflict("explicit_source_detach_required")
    );
    movement.detach = Some(member("cutex.worker", "alpha", "detach-alpha", 2, true));
    let receipt = f.run(&movement).unwrap();
    assert!(receipt.complete, "{:?}", receipt.error);
    assert_eq!(receipt.steps.len(), 2);
    assert_eq!(
        f.candidate("cutex.worker")
            .current_project_id
            .unwrap()
            .as_str(),
        "beta"
    );
    let mut director = f.request("cutex.a", "A", "move-director");
    director.detach = Some(member("cutex.a", "alpha", "detach-director", 3, true));
    director.assignment = Some(member("cutex.a", "beta", "add-director", 2, false));
    let denied = f.run(&director).unwrap();
    assert!(!denied.complete);
    assert!(denied.steps.is_empty());
    assert_eq!(
        f.candidate("cutex.a").current_project_id.unwrap().as_str(),
        "alpha"
    );
}

#[test]
fn imported_formal_name_projection_follows_explicit_rename_only() {
    let f = Fixture::new();
    f.add("cutex.worker", Some("Worker"));
    let request = f.request("cutex.worker", "Worker", "import");
    f.run(&request).unwrap();
    f.mutate("cutex.worker", |r| {
        r.formal_agent_name = Some("Renamed Agent".into());
        r.thread_name = Some("Other title".into());
        r.bump_durable_revision().unwrap();
    });
    assert_eq!(
        f.candidate("cutex.worker").formal_name.as_deref(),
        Some("Renamed Agent")
    );
}

#[test]
fn interrupted_name_and_assignment_recover_the_exact_action_without_duplicate_effects() {
    for stage in ["after_name", "assignment"] {
        let f = Fixture::new();
        f.add("cutex.director", None);
        let mut request = f.request("cutex.director", "Human Name", "recover");
        request.assignment = Some(create("cutex.director", "alpha"));
        INTERRUPT_IMPORT.with(|point| point.set(Some(stage)));
        let partial = f.run(&request).unwrap();
        assert!(!partial.complete);
        let receipt = f.run(&request).unwrap();
        assert!(
            receipt.complete && receipt.named && receipt.imported,
            "{stage}: {:?}",
            receipt.error
        );
        let state = f.provider.store().snapshot().unwrap();
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.human_management_project_mutations.len(), 1);
        assert_eq!(state.durable_import_audit.len(), 3);
        assert_eq!(
            state.current_project_memberships[request.candidate.cutex_session_id.as_ref().unwrap()]
                .revision,
            2
        );
        assert_eq!(f.run(&request).unwrap(), receipt);
    }
}

#[test]
fn identical_external_name_after_interruption_is_not_our_naming_receipt() {
    let f = Fixture::new();
    f.add("cutex.worker", None);
    let request = f.request("cutex.worker", "Human Name", "recover");
    INTERRUPT_IMPORT.with(|point| point.set(Some("before_name")));
    assert!(!f.run(&request).unwrap().complete);
    f.mutate("cutex.worker", |record| {
        record.formal_agent_name = Some("Human Name".into());
        record.bump_durable_revision().unwrap();
    });
    let receipt = f.run(&request).unwrap();
    assert!(!receipt.complete && !receipt.imported);
    assert!(receipt
        .error
        .unwrap()
        .contains("formal_name_changed_by_other_action"));
}

#[test]
fn local_only_unsupported_malformed_and_duplicate_native_records_are_rejected() {
    for kind in [
        "local",
        "future",
        "missing-native",
        "empty-profile",
        "hidden",
        "duplicate-native",
    ] {
        let f = Fixture::new();
        f.add("cutex.worker", Some("Worker"));
        if kind == "duplicate-native" {
            f.add("cutex.other", Some("Other"));
            f.mutate("cutex.other", |r| {
                r.codex_session_id = Some("native-cutex.worker".into())
            });
        } else {
            f.mutate("cutex.worker", |r| match kind {
                "local" => r.registration_class = AgentRegistrationClass::LocalOnly,
                "future" => r.runtime_backend = CutexSessionRuntimeBackend::Future,
                "missing-native" => r.codex_session_id = None,
                "empty-profile" => r.profile = Some(String::new()),
                "hidden" => {
                    r.quick_action = crate::session::model::CutexSessionQuickActionMode::Hidden
                }
                _ => unreachable!(),
            });
        }
        assert!(
            f.run(&f.request("cutex.worker", "Worker", "import"))
                .is_err(),
            "{kind}"
        );
        assert!(f.provider.store().snapshot().unwrap().agents.is_empty());
    }
}

#[test]
fn explicit_operator_detach_revokes_grant() {
    let f = Fixture::new();
    f.add("cutex.a", Some("A"));
    f.create("cutex.a", "alpha");
    f.add("cutex.b", Some("B"));
    f.create("cutex.b", "beta");
    f.add("cutex.worker", Some("Worker"));
    let mut add = f.request("cutex.worker", "Worker", "add");
    add.assignment = Some(member("cutex.worker", "alpha", "add-alpha", 1, false));
    assert!(f.run(&add).unwrap().complete);
    f.provider
        .execute_operator_action_for_management(
            &HumanManagementPrincipal::authenticated(),
            &HumanManagementOperatorActionRequest {
                schema: HumanManagementOperatorSchema::V1,
                action_id: AgentActionId::new("grant").unwrap(),
                project_id: ProjectId::new("alpha").unwrap(),
                expected_authority_epoch: Some(1),
                expected_grant_revision: Some(0),
                operation: HumanManagementOperatorKind::Grant,
                operator_cutex_session_id: CutexSessionId::new("cutex.worker").unwrap(),
            },
        )
        .unwrap();
    let mut request = f.request("cutex.worker", "Worker", "move-operator");
    request.detach = Some(member("cutex.worker", "alpha", "detach-op", 2, true));
    request.assignment = Some(member("cutex.worker", "beta", "add-op", 1, false));
    // r219: no Task Service guard; the move runs directly.
    let complete = f.run(&request).unwrap();
    assert!(complete.complete, "{:?}", complete.error);
    let state = f.provider.store().snapshot().unwrap();
    assert!(!state
        .operator_grants
        .get(&ProjectId::new("alpha").unwrap())
        .is_some_and(
            |grants| grants.contains_key(request.candidate.cutex_session_id.as_ref().unwrap())
        ));
}

#[test]
fn nullable_profile_wire_preserves_legacy_string_and_create_validation() {
    let f = Fixture::new();
    f.add("cutex.worker", Some("Worker"));
    let receipt = f
        .run(&f.request("cutex.worker", "Worker", "import"))
        .unwrap();
    let spec = receipt.imported_agent.unwrap().spec;
    assert_eq!(spec.profile, None);
    let mut wire = serde_json::to_value(&spec).unwrap();
    assert!(wire["profile"].is_null());
    wire["profile"] = serde_json::json!("existing-client-profile");
    let existing: ManagedAgentSpec = serde_json::from_value(wire).unwrap();
    assert_eq!(existing.profile.as_deref(), Some("existing-client-profile"));
    assert_eq!(
        spec.validate().unwrap_err(),
        AgentManagementError::InvalidRequest("invalid_profile")
    );
}

#[test]
fn claude_agents_import_with_distinct_adapter_threads_without_codex_rollouts() {
    let f = Fixture::new();
    for (id, name) in [("cutex.claude-a", "claude-a"), ("cutex.claude-b", "claude-b")] {
        f.add(id, Some(name));
        f.mutate(id, |r| {
            r.codex_session_id = None;
            r.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
            r.claude_session = Some(serde_json::from_value(serde_json::json!({
                "deployment": {"python":"/unused/python", "adapter_source":"/unused/adapter",
                    "claude_cli":"/unused/claude", "claude_home":"/unused/home", "tui_binary":"/unused/tui"},
                "thread_id": uuid::Uuid::new_v4().to_string(),
                "state_dir":"/unused/state", "model":"sonnet"
            })).unwrap());
        });
    }
    assert!(f.candidate("cutex.claude-a").rejection.is_none());
    assert!(f.candidate("cutex.claude-b").rejection.is_none());
    let receipt = f.run(&f.request("cutex.claude-a", "claude-a", "import-claude-a")).unwrap();
    assert!(receipt.complete, "{:?}", receipt.error);
    let state = f.provider.store().snapshot().unwrap();
    let agent = &state.agents[&CutexSessionId::new("cutex.claude-a").unwrap()];
    assert_eq!(agent.spec.runtime_backend, "claude_subscription");
    let sessions = load_cutex_session_store_from_path(&f.path).unwrap();
    assert_eq!(Some(agent.native_session_id.as_str()), sessions.sessions["cutex.claude-a"].app_server_thread_id());
}
