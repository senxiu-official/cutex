#[test]
fn r264_historical_archive_replay_does_not_settle_post_restore_delegations() {
    let f = Fixture::new();
    let mut runtime = Offline {
        stops: 0,
        reject: false,
    };
    let request = f.request(AgentArchiveOperation::Archive, "historical-archive");
    f.execute(&request, &mut runtime).unwrap();
    f.execute(
        &f.request(AgentArchiveOperation::Restore, "new-lifecycle"),
        &mut runtime,
    )
    .unwrap();
    let store = crate::asks::store::AskStore::open_default().unwrap();
    let now = chrono::Utc::now();
    let ask_id = store
        .with_state(|state| {
            let mut ask = crate::asks::store::tests::new_ask(
                &format!("post-restore-{}", uuid::Uuid::new_v4()),
                "caller",
            );
            ask.target_cutex_session_id = "cutex.archive-test".into();
            state.create_ask(ask, now).unwrap().0.ask_id
        })
        .unwrap();
    f.execute(&request, &mut runtime).unwrap();
    assert!(store.snapshot().unwrap().asks[&ask_id].is_open());
}

#[test]
fn r264_runtime_can_write_session_store_and_identity_change_is_fenced() {
    struct MutatingRuntime {
        path: std::path::PathBuf,
        change_identity: bool,
    }
    impl AgentArchiveRuntime for MutatingRuntime {
        fn prepare(&mut self, _: &CutexSessionRecord, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop_and_verify(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            crate::session::store::with_locked_session_store(&self.path, |sessions| {
                let record = sessions.sessions.get_mut("cutex.archive-test").unwrap();
                if self.change_identity {
                    record.codex_session_id = Some("other-native".into());
                } else {
                    record.profile = Some("current-config".into());
                }
                crate::session::store::save_locked_session_store(&self.path, sessions)
            })
        }
        fn verify_offline(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            Ok(())
        }
    }
    for change_identity in [false, true] {
        let f = Fixture::new();
        let request = f.request(AgentArchiveOperation::Archive, "runtime-write");
        let result = f.provider.execute_agent_archive(
            &HumanManagementPrincipal::authenticated(),
            &f.path,
            &request,
            &mut MutatingRuntime {
                path: f.path.clone(),
                change_identity,
            },
        );
        let saved = load_cutex_session_store_from_path(&f.path).unwrap();
        if change_identity {
            assert!(result.is_err());
            assert!(saved.sessions["cutex.archive-test"].is_active());
        } else {
            assert_eq!(result.unwrap().stage, AgentArchiveStage::Committed);
            assert_eq!(
                saved.sessions["cutex.archive-test"].profile.as_deref(),
                Some("current-config")
            );
        }
    }
}

#[test]
fn r264_authorization_precedes_effects_and_historical_receipts() {
    let f = Fixture::new();
    let request = f.request(AgentArchiveOperation::Archive, "authorized-archive");
    let mut runtime = Offline {
        stops: 0,
        reject: false,
    };
    for committed in [false, true] {
        if committed {
            f.execute(&request, &mut runtime).unwrap();
        }
        assert!(matches!(
            f.provider.execute_authorized_agent_archive(
                &f.path,
                &request,
                &mut runtime,
                |_, _, _| Err(AgentManagementError::Unauthorized)
            ),
            Err(AgentManagementError::Unauthorized)
        ));
        assert_eq!(runtime.stops, usize::from(committed));
    }
}

#[test]
fn r264_action_id_is_bound_to_target_and_operation_in_both_stores() {
    let f = Fixture::new();
    let request = f.request(AgentArchiveOperation::Archive, "bound-archive");
    let mut runtime = Offline {
        stops: 0,
        reject: false,
    };
    f.execute(&request, &mut runtime).unwrap();
    for durable_only in [false, true] {
        if durable_only {
            f.provider
                .store()
                .with_state(true, |mut s| {
                    s.agent_archive_actions.clear();
                    Ok((s, (), true))
                })
                .unwrap();
        }
        for change_target in [false, true] {
            let mut changed = request.clone();
            if change_target {
                changed.review.cutex_session_id = CutexSessionId::new("cutex.other").unwrap();
            } else {
                changed.review.operation = AgentArchiveOperation::Restore;
            }
            assert!(matches!(
                f.execute(&changed, &mut runtime),
                Err(AgentManagementError::Conflict(
                    "archive_action_target_conflict"
                ))
            ));
        }
    }
    assert_eq!(runtime.stops, 1);
}

#[test]
fn r264_soft_restore_keeps_session_thread_profile_and_offline_state() {
    let f = Fixture::new();
    let before = load_cutex_session_store_from_path(&f.path)
        .unwrap()
        .sessions["cutex.archive-test"]
        .clone();
    let mut runtime = Offline {
        stops: 0,
        reject: false,
    };
    let archived = f
        .execute(
            &f.request(AgentArchiveOperation::Archive, "soft"),
            &mut runtime,
        )
        .unwrap();
    assert_eq!(archived.archive_kind, Some(AgentArchiveKind::Soft));
    assert_eq!(archived.restorable, Some(true));
    let restored = f
        .execute(
            &f.request(AgentArchiveOperation::Restore, "restore"),
            &mut runtime,
        )
        .unwrap();
    assert_eq!(restored.archive_kind, Some(AgentArchiveKind::Active));
    assert_eq!(restored.restorable, Some(false));
    let record = restored.result.unwrap();
    assert_eq!(record.cutex_session_id, before.cutex_session_id);
    assert_eq!(record.codex_session_id, before.codex_session_id);
    assert_eq!(record.profile, before.profile);
    assert!(!record_has_runtime_claim(&record));
}

#[test]
fn r264_permanent_owner_confirmation_and_agent_denial() {
    for soft_first in [false, true] {
        let f = Fixture::new();
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        if soft_first {
            f.execute(
                &f.request(AgentArchiveOperation::Archive, "soft-first"),
                &mut runtime,
            )
            .unwrap();
        }
        let owner = HumanManagementPrincipal::authenticated();
        let confirmation = f.provider.review_permanent_agent_retirement(&owner, &f.path,
            &PermanentAgentRetirementReviewRequest { cutex_session_id: CutexSessionId::new("cutex.archive-test").unwrap() }).unwrap();
        let mut request = PermanentAgentRetirementRequest {
            action_id: AgentActionId::new("owner-permanent").unwrap(),
            cutex_session_id: CutexSessionId::new("cutex.archive-test").unwrap(),
            confirmed_name: "wrong".into(),
            confirmation_token: confirmation.confirmation_token,
            expected_revision: confirmation.revision,
        };
        let stops = runtime.stops;
        assert!(matches!(
            f.provider
                .permanently_retire_agent(&owner, &f.path, &request, &mut runtime),
            Err(AgentManagementError::Conflict(
                "permanent_retirement_name_confirmation_required"
            ))
        ));
        assert_eq!(runtime.stops, stops);
        request.confirmed_name = "Formal Agent".into();
        let receipt = f
            .provider
            .permanently_retire_agent(&owner, &f.path, &request, &mut runtime)
            .unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        assert_eq!(receipt.archive_kind, Some(AgentArchiveKind::Permanent));
        assert_eq!(receipt.restorable, Some(false));
        let state = f.provider.store().snapshot().unwrap();
        assert!(runtime_guard(&state, &request.cutex_session_id).is_err());
        assert!(f
            .provider
            .review_agent_archive(
                &owner,
                &f.path,
                &request.cutex_session_id,
                AgentArchiveOperation::Restore
            )
            .is_err());
        let stops = runtime.stops;
        assert_eq!(
            f.provider
                .permanently_retire_agent(&owner, &f.path, &request, &mut runtime)
                .unwrap(),
            receipt
        );
        assert_eq!(runtime.stops, stops);
        assert!(matches!(
            f.provider.execute_authorized_agent_archive(
                &f.path,
                &receipt.request,
                &mut runtime,
                |_, _, _| Ok(())
            ),
            Err(AgentManagementError::Unauthorized)
        ));
    }
}

#[test]
fn r264_permanent_tombstone_survives_projection_crash() {
    let f = Fixture::new();
    let owner = HumanManagementPrincipal::authenticated();
    let confirmation = f.provider.review_permanent_agent_retirement(&owner, &f.path,
        &PermanentAgentRetirementReviewRequest { cutex_session_id: CutexSessionId::new("cutex.archive-test").unwrap() }).unwrap();
    let request = PermanentAgentRetirementRequest {
        action_id: AgentActionId::new("permanent-recover").unwrap(),
        cutex_session_id: CutexSessionId::new("cutex.archive-test").unwrap(),
        confirmed_name: "Formal Agent".into(),
        confirmation_token: confirmation.confirmation_token,
        expected_revision: confirmation.revision,
    };
    let mut runtime = Offline {
        stops: 0,
        reject: false,
    };
    let receipt = f
        .provider
        .permanently_retire_agent(&owner, &f.path, &request, &mut runtime)
        .unwrap();
    f.provider
        .store()
        .with_state(true, |mut s| {
            let mut pending = receipt.clone();
            pending.stage = AgentArchiveStage::PermanentRetirementCommitted;
            s.agent_archive_actions
                .insert(request.action_id.clone(), pending);
            Ok((s, (), true))
        })
        .unwrap();
    let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
    sessions.agent_archive_receipts.clear();
    let record = sessions.sessions.get_mut("cutex.archive-test").unwrap();
    record.archive_state = crate::session::model::CutexSessionArchiveState::Active;
    record.retired_at = None;
    save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
    assert!(f
        .provider
        .review_agent_archive(
            &owner,
            &f.path,
            &request.cutex_session_id,
            AgentArchiveOperation::Restore
        )
        .is_err());
    assert_eq!(
        f.provider
            .permanently_retire_agent(&owner, &f.path, &request, &mut runtime)
            .unwrap()
            .stage,
        AgentArchiveStage::Committed
    );
}

#[test]
fn r264_permanent_confirmation_expiry_revision_target_and_single_action() {
    for case in ["expired", "revision", "target", "rename", "reuse"] {
        let f=Fixture::new();
        let owner=HumanManagementPrincipal::authenticated();
        let id=CutexSessionId::new("cutex.archive-test").unwrap();
        let review=f.provider.review_permanent_agent_retirement(&owner,&f.path,&PermanentAgentRetirementReviewRequest { cutex_session_id:id.clone() }).unwrap();
        let mut request=PermanentAgentRetirementRequest { action_id:AgentActionId::new("token-first").unwrap(), cutex_session_id:id,
            confirmed_name:review.formal_name,confirmation_token:review.confirmation_token,expected_revision:review.revision };
        let mut runtime=Offline {stops:0,reject:false};
        match case {
            "expired" => { f.provider.store().with_state(true,|mut state| {
                for value in state.extra.get_mut("permanent_retirement_confirmations_v1").unwrap().as_object_mut().unwrap().values_mut() { value["expires"]=serde_json::json!(0); }
                Ok((state,(),true))
            }).unwrap(); }
            "revision" => request.expected_revision+=1,
            "target" => request.cutex_session_id=CutexSessionId::new("cutex.other").unwrap(),
            "rename" => { let mut sessions=load_cutex_session_store_from_path(&f.path).unwrap();
                sessions.sessions.get_mut("cutex.archive-test").unwrap().formal_agent_name=Some("renamed".into());
                save_cutex_session_store_to_path(&f.path,&sessions).unwrap(); }
            "reuse" => { f.provider.permanently_retire_agent(&owner,&f.path,&request,&mut runtime).unwrap();
                request.action_id=AgentActionId::new("token-second").unwrap(); }
            _ => unreachable!(),
        }
        let stops=runtime.stops;
        assert!(f.provider.permanently_retire_agent(&owner,&f.path,&request,&mut runtime).is_err(),"{case}");
        assert_eq!(runtime.stops,stops,"{case}");
    }
}

#[test]
fn r264_permanent_unfinished_retry_never_stops_a_new_occurrence() {
    struct FailingStop { stops: usize }
    impl AgentArchiveRuntime for FailingStop {
        fn prepare(&mut self,_:&CutexSessionRecord,_:bool)->anyhow::Result<()> {Ok(())}
        fn stop_and_verify(&mut self,_:&CutexSessionRecord)->anyhow::Result<()> {self.stops+=1;anyhow::bail!("stop failed before proof")}
        fn verify_offline(&mut self,_:&CutexSessionRecord)->anyhow::Result<()> {Ok(())}
    }
    for changed in ["generation", "thread"] {
        let f=Fixture::new();let owner=HumanManagementPrincipal::authenticated();
        let review=f.provider.review_permanent_agent_retirement(&owner,&f.path,&PermanentAgentRetirementReviewRequest {
            cutex_session_id:CutexSessionId::new("cutex.archive-test").unwrap() }).unwrap();
        let request=PermanentAgentRetirementRequest { action_id:AgentActionId::new("unfinished-permanent").unwrap(),
            cutex_session_id:review.cutex_session_id,confirmed_name:review.formal_name,
            confirmation_token:review.confirmation_token,expected_revision:review.revision };
        let mut runtime=FailingStop {stops:0};
        let first=f.provider.permanently_retire_agent(&owner,&f.path,&request,&mut runtime).unwrap();
        assert_eq!(first.stage,AgentArchiveStage::Uncertain);assert_eq!(runtime.stops,1);
        let mut sessions=load_cutex_session_store_from_path(&f.path).unwrap();
        let record=sessions.sessions.get_mut("cutex.archive-test").unwrap();
        let revision=record.durable_revision();
        if changed=="generation" {record.runtime_generation+=1;} else {record.codex_session_id=Some("new-native-thread".into());}
        assert_eq!(record.durable_revision(),revision);
        save_cutex_session_store_to_path(&f.path,&sessions).unwrap();
        assert!(f.provider.permanently_retire_agent(&owner,&f.path,&request,&mut runtime).is_err(),"{changed}");
        assert_eq!(runtime.stops,1,"retry must reject before the stop effect: {changed}");
        assert!(load_cutex_session_store_from_path(&f.path).unwrap().sessions["cutex.archive-test"].is_active());
        assert_eq!(f.provider.store().snapshot().unwrap().extra["archive_storage_contract_v1"]["minimum_reader_release"],"r264");
    }
}
#[test]
fn r264_permanent_thread_changes_after_authorization_before_transaction_never_stop() {
    let f=Fixture::new(); let owner=HumanManagementPrincipal::authenticated();
    let review=f.provider.review_permanent_agent_retirement(&owner,&f.path,&PermanentAgentRetirementReviewRequest {
        cutex_session_id:CutexSessionId::new("cutex.archive-test").unwrap() }).unwrap();
    let request=PermanentAgentRetirementRequest { action_id:AgentActionId::new("between-reads-thread-change").unwrap(),
        cutex_session_id:review.cutex_session_id,confirmed_name:review.formal_name,
        confirmation_token:review.confirmation_token,expected_revision:review.revision };
    let path=f.path.clone();
    AFTER_ARCHIVE_AUTHORIZE.with(|hook| *hook.borrow_mut()=Some(Box::new(move || {
        crate::session::store::with_locked_session_store(&path, |sessions| {
            let record=sessions.sessions.get_mut("cutex.archive-test").unwrap();
            // Deliberately retain generation and durable revision: only the thread changes.
            record.codex_session_id=Some("changed-after-authorize".into());
            crate::session::store::save_locked_session_store(&path,sessions)
        }).unwrap();
    })));
    let mut runtime=Offline { stops:0,reject:false };
    let error=f.provider.permanently_retire_agent(&owner,&f.path,&request,&mut runtime).unwrap_err();
    assert!(error.to_string().contains("archive_confirmation_occurrence_changed"));
    assert_eq!(runtime.stops,0);
    assert!(load_cutex_session_store_from_path(&f.path).unwrap().sessions["cutex.archive-test"].is_active());
}
