#[test]
fn r264_legacy_half_closed_predecessor_is_not_revived_or_permanently_finished() {
    let root = root("legacy-half-close");
    let provider = AgentManagementProvider::open(&root)
        .unwrap()
        .with_fail_after_predecessor_close_once();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let agent = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("worker", "worker", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    let replace = AgentManagementRequest {
        schema: AgentManagementSchema::V1,
        action_id: action("replace"),
        project_id: Some(project()),
        operation: AgentOperation::Replace {
            bootstrap_intent: None,
            predecessor_cutex_session_id: agent.cutex_session_id.clone(),
            policy: AgentReplacePolicy::CloseBeforeCreate,
            successor: spec("successor"),
            start_mode: AgentStartMode::BootstrapOnly,
            frozen_message: None,
        },
    };
    provider.execute(&invocation("cutex.director"), &replace, &lifecycle);
    let path = lifecycle.archive_session_store_path().unwrap();
    // The pre-r264 crash boundary has no durable reversible archive evidence.
    provider
        .store()
        .with_state(true, |mut s| {
            s.agent_archive_actions.clear();
            s.agent_archive_audit.clear();
            Ok((s, (), true))
        })
        .unwrap();
    let mut sessions = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
    sessions.agent_archive_receipts.clear();
    crate::session::store::save_cutex_session_store_to_path(&path, &sessions).unwrap();
    let launches = lifecycle.bootstrap_count();
    assert!(
        matches!(provider.execute(&invocation("cutex.director"), &replace, &lifecycle).outcome,
        AgentManagementOutcome::NoWrite { ref code, .. } if code == "legacy_close_requires_owner_review")
    );
    let restore = ManagedAgentArchiveRequest {
        action_id: action("restore"),
        project_id: Some(project()),
        cutex_session_id: agent.cutex_session_id.clone(),
        operation: AgentArchiveOperation::Restore,
    };
    assert!(matches!(
        provider.execute_managed_archive(
            &invocation("cutex.director"),
            &restore,
            &path,
            &mut ManagedArchiveRuntime(&lifecycle, agent.cutex_session_id.clone())
        ),
        Err(AgentManagementError::Conflict(
            "legacy_close_requires_owner_review"
        ))
    ));
    assert_eq!(lifecycle.bootstrap_count(), launches);
    assert_eq!(lifecycle.retire_count(&agent.cutex_session_id), 0);
    assert!(
        provider.store().snapshot().unwrap().agents[&agent.cutex_session_id]
            .retired_at
            .is_none()
    );
}

struct ManagedArchiveRuntime<'a>(&'a FakeLifecycle, CutexSessionId);
impl AgentArchiveRuntime for ManagedArchiveRuntime<'_> {
    fn prepare(
        &mut self,
        _: &crate::session::model::CutexSessionRecord,
        _: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_and_verify(
        &mut self,
        _: &crate::session::model::CutexSessionRecord,
    ) -> anyhow::Result<()> {
        self.0
            .offline(&self.1)
            .map_err(|e| anyhow::anyhow!(e.detail))
    }
    fn verify_offline(
        &mut self,
        _: &crate::session::model::CutexSessionRecord,
    ) -> anyhow::Result<()> {
        let state = self.0.observe(&self.1).unwrap();
        anyhow::ensure!(
            !state.app_server_runtime && state.agent_bus_endpoint_ids.is_empty(),
            "not offline"
        );
        Ok(())
    }
}

#[test]
fn r264_managed_archive_restore_scope_and_permanent_denial() {
    let root = root("managed-archive");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let agent = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("worker", "worker", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    let path = lifecycle.archive_session_store_path().unwrap();
    let mut runtime = ManagedArchiveRuntime(&lifecycle, agent.cutex_session_id.clone());
    let mut request = ManagedAgentArchiveRequest {
        action_id: action("soft-archive"),
        project_id: Some(project()),
        cutex_session_id: agent.cutex_session_id.clone(),
        operation: AgentArchiveOperation::Archive,
    };
    let log = lifecycle.log();
    assert!(provider
        .execute_managed_archive(
            &invocation("cutex.unprivileged"),
            &request,
            &path,
            &mut runtime
        )
        .is_err());
    request.project_id = Some(ProjectId::new("another-project").unwrap());
    assert!(provider
        .execute_managed_archive(&invocation("cutex.director"), &request, &path, &mut runtime)
        .is_err());
    request.project_id = Some(project());
    request.operation = AgentArchiveOperation::PermanentRetire;
    assert!(matches!(
        provider.execute_managed_archive(
            &invocation("cutex.director"),
            &request,
            &path,
            &mut runtime
        ),
        Err(AgentManagementError::Unauthorized)
    ));
    assert_eq!(lifecycle.log(), log);
    request.operation = AgentArchiveOperation::Archive;
    let archived = provider
        .execute_managed_archive(&invocation("cutex.director"), &request, &path, &mut runtime)
        .unwrap();
    assert_eq!(archived.restorable, Some(true));
    assert!(
        provider.store().snapshot().unwrap().agents[&agent.cutex_session_id]
            .retired_at
            .is_none()
    );
    let restore = ManagedAgentArchiveRequest {
        action_id: action("restore"),
        operation: AgentArchiveOperation::Restore,
        ..request.clone()
    };
    let restored = provider
        .execute_managed_archive(&invocation("cutex.director"), &restore, &path, &mut runtime)
        .unwrap();
    assert_eq!(
        restored
            .result
            .as_ref()
            .unwrap()
            .codex_session_id
            .as_deref(),
        Some(agent.native_session_id.as_str())
    );
    assert!(!crate::session::archive::record_has_runtime_claim(
        restored.result.as_ref().unwrap()
    ));
    assert_eq!(
        provider
            .execute_managed_archive(&invocation("cutex.director"), &request, &path, &mut runtime)
            .unwrap(),
        archived
    );
    // Same caller/action with changed operation cannot borrow the archive receipt.
    let changed = ManagedAgentArchiveRequest {
        operation: AgentArchiveOperation::Restore,
        ..request
    };
    assert!(provider
        .execute_managed_archive(&invocation("cutex.director"), &changed, &path, &mut runtime)
        .is_err());
}

#[test]
fn r264_operator_restore_rechecks_grant_on_replay() {
    let root = root("operator-restore");
    let provider = AgentManagementProvider::open(&root).unwrap();
    bind(&provider, "bind", "cutex.director", None);
    let lifecycle = FakeLifecycle::default();
    let operator = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("operator", "operator", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    let target = created_agent(&completed(provider.execute(
        &invocation("cutex.director"),
        &create_request("target", "target", AgentStartMode::BootstrapOnly),
        &lifecycle,
    )));
    completed(provider.execute(
        &invocation("cutex.director"),
        &operator_request("grant", "grant", &operator.cutex_session_id, 0),
        &lifecycle,
    ));
    let path = lifecycle.archive_session_store_path().unwrap();
    let mut runtime = ManagedArchiveRuntime(&lifecycle, target.cutex_session_id.clone());
    let request = ManagedAgentArchiveRequest {
        action_id: action("archive"),
        project_id: Some(project()),
        cutex_session_id: target.cutex_session_id.clone(),
        operation: AgentArchiveOperation::Archive,
    };
    let caller = invocation(operator.cutex_session_id.as_str());
    provider
        .execute_managed_archive(&caller, &request, &path, &mut runtime)
        .unwrap();
    let restore = ManagedAgentArchiveRequest {
        action_id: action("restore"),
        operation: AgentArchiveOperation::Restore,
        ..request
    };
    provider
        .execute_managed_archive(&caller, &restore, &path, &mut runtime)
        .unwrap();
    completed(provider.execute(
        &invocation("cutex.director"),
        &operator_request("revoke", "revoke", &operator.cutex_session_id, 1),
        &lifecycle,
    ));
    assert!(provider
        .execute_managed_archive(&caller, &restore, &path, &mut runtime)
        .is_err());
}
