use super::*;
use crate::session::store;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    id: String,
    scope: NameScope,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("name-ledger-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let fixture = Self {
            path: directory.join("cutex-sessions.json"),
            directory,
            id: format!("cutex.{}", uuid::Uuid::new_v4()),
            scope: NameScope::for_project(Some("project-a")),
        };
        fixture.insert(&fixture.id, "A");
        fixture
    }
    fn insert(&self, id: &str, name: &str) {
        let mut record = CutexSessionRecord::new(
            id.into(),
            Some(uuid::Uuid::new_v4().to_string()),
            "host".into(),
            "/workspace".into(),
            None,
        )
        .unwrap();
        record.formal_agent_name = Some(name.into());
        record.display_name_override = Some("A".into());
        record.agent_enabled = true;
        record.runtime_generation = 7;
        store::with_locked_session_rows(&self.path, |rows| {
            rows.insert_session(id, record);
            rows.commit()?;
            Ok(())
        })
        .unwrap();
    }
    fn record(&self) -> CutexSessionRecord {
        store::load_session_row(&self.path, &self.id)
            .unwrap()
            .unwrap()
            .1
    }
    fn request(&self, name: &str) -> RenameRequest {
        RenameRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            expected_revision: self.record().durable_revision(),
            name: name.into(),
        }
    }
    fn rename(&self, request: &RenameRequest) -> anyhow::Result<RenameCommit> {
        store::rename_formal_name_at(
            &self.path,
            &self.id,
            request,
            &self.scope,
            Some("cutex.director"),
            "human-management",
        )
    }
    fn history(&self) -> NameHistoryPage {
        history_at(&self.path, &self.id, None, 100).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        store::close_session_store(&self.path);
        fs::remove_dir_all(&self.directory).unwrap();
    }
}
fn code(error: &anyhow::Error) -> &str {
    error.downcast_ref::<NameError>().unwrap().code
}

#[test]
fn archived_session_without_an_own_formal_name_has_empty_history() {
    let fixture=Fixture::new();
    let mut record=fixture.record();
    record.formal_agent_name=None;
    record.archive_state=crate::session::model::CutexSessionArchiveState::Retired;
    store::with_locked_session_rows(&fixture.path,|rows| {
        rows.insert_session(&fixture.id,record);rows.commit()?;Ok(())
    }).unwrap();
    let page=fixture.history();
    assert!(page.events.is_empty());assert_eq!(page.next,None);
}

#[test]
fn archived_rename_preserves_archive_and_prior_replay_and_history() {
    let fixture=Fixture::new();
    let request=fixture.request("B");
    let committed=fixture.rename(&request).unwrap();
    let mut record=fixture.record();
    store::with_locked_session_rows(&fixture.path,|rows| {
        record.archive_state=crate::session::model::CutexSessionArchiveState::Retired;
        rows.insert_session(&fixture.id,record);rows.commit()?;Ok(())
    }).unwrap();
    let archived=fixture.record();
    fixture.rename(&fixture.request("C")).unwrap();
    assert!(fixture.record().is_retired());
    assert_eq!(fixture.record().retired_at,archived.retired_at);
    assert_eq!(fixture.record().runtime_generation,archived.runtime_generation);
    let replay=fixture.rename(&request).unwrap();assert!(replay.replayed);assert_eq!(replay.event,committed.event);
    assert_eq!(fixture.history().events.len(),2);
    assert_eq!(fixture.record().formal_agent_name.as_deref(),Some("C"));
}

#[test]
fn case_only_rename_is_an_event_and_invalid_names_have_a_distinct_code() {
    let fixture=Fixture::new();
    let committed=fixture.rename(&fixture.request("a")).unwrap();
    assert_eq!(committed.event.old_name,"A");
    assert_eq!(committed.event.new_name,"a");
    assert_eq!(fixture.record().formal_agent_name.as_deref(),Some("a"));
    assert_eq!(fixture.history().events.len(),1);
    assert_eq!(code(&fixture.rename(&fixture.request("a")).unwrap_err()),"agent_name_unchanged");
    for name in [""," leading","director","other@host"] {
        assert_eq!(code(&fixture.rename(&fixture.request(name)).unwrap_err()),"agent_name_invalid");
    }
    let mut request=fixture.request("Valid");request.request_id.clear();
    assert_eq!(code(&fixture.rename(&request).unwrap_err()),"invalid_request");
    assert_eq!(fixture.history().events.len(),1);
}

#[test]
fn passive_outbox_is_authenticated_per_recipient_and_offline_rows_do_not_starve_online() {
    use crate::agent_bus::{rename_notice,model::{AgentBusMessage,AgentBusEnvelopeKind,AgentMessageKind},delivery::AgentDeliveryMode};
    let fixture=Fixture::new();
    // More than a batch's worth of offline self notes must not hide the Director's.
    for index in 0..55 {fixture.rename(&fixture.request(&format!("N{index}"))).unwrap();}
    let notes=pending_for_at(&fixture.path,&["cutex.director".into()],100).unwrap();
    assert_eq!(notes.len(),55);
    assert!(notes.iter().all(|note|note.recipient_session_id=="cutex.director"));
    let note=&notes[0];
    let message=AgentBusMessage {
        id:rename_notice::message_id(&note.event_id,&note.recipient_session_id),kind:AgentBusEnvelopeKind::Message,
        from:rename_notice::SOURCE_ID.into(),to:"current-director-runtime".into(),from_cutex_session_id:None,
        to_cutex_session_id:Some(note.recipient_session_id.clone()),content:note.text.clone(),
        delivery_mode:AgentDeliveryMode::Passive,trigger_turn:false,created_at_epoch_secs:1,sender_kind:AgentMessageKind::Agent,
        display_source:None,submit_mode:None,control_type:Some(rename_notice::CONTROL_TYPE.into()),
        control_payload:Some(serde_json::json!({"eventId":note.event_id})),external_action_id:None,external_message_id:None,
    };
    let text=rename_notice::model_text_at(&fixture.path,&message).unwrap();
    assert!(text.contains(&note.text));
    delivered_at(&fixture.path,&note.event_id,&note.recipient_session_id).unwrap();
    delivered_at(&fixture.path,&note.event_id,&note.recipient_session_id).unwrap();
    assert_eq!(pending_for_at(&fixture.path,&["cutex.director".into()],100).unwrap().len(),54);
    assert_eq!(rename_notice::model_text_at(&fixture.path,&message).unwrap(),text,"ACK preserves durable provenance");
    for case in 0..4 {
        let mut forged=message.clone();
        match case {0=>forged.trigger_turn=true,1=>forged.content.push('!'),2=>forged.to_cutex_session_id=Some("cutex.foreign".into()),_=>forged.from_cutex_session_id=Some(fixture.id.clone())}
        assert!(rename_notice::model_text_at(&fixture.path,&forged).is_err());
    }
}

#[test]
fn a_b_c_and_back_keep_identity_display_override_and_direct_claims() {
    let fixture = Fixture::new();
    let original = fixture.record();
    for name in ["B", "C", "A"] {
        fixture.rename(&fixture.request(name)).unwrap();
    }
    let current = fixture.record();
    assert_eq!(current.cutex_session_id, original.cutex_session_id);
    assert_eq!(current.codex_session_id, original.codex_session_id);
    assert_eq!(current.runtime_generation, 7);
    assert_eq!(current.display_name_override, Some("A".into()));
    assert_eq!(current.formal_agent_name, Some("A".into()));
    assert_eq!(current.durable_revision(), 4);
    let history = fixture.history();
    assert_eq!(
        history
            .events
            .iter()
            .map(|event| (event.old_name.as_str(), event.new_name.as_str()))
            .collect::<Vec<_>>(),
        [("C", "A"), ("B", "C"), ("A", "B")]
    );
    assert!(history
        .events
        .iter()
        .all(|event| event.actor == "human-management"));
    for name in ["A", "B", "C"] {
        let (_, claims) = claims_at(&fixture.path, Some(&fixture.scope), name).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].cutex_session_id, fixture.id);
    }
    assert_eq!(pending_at(&fixture.path, 100).unwrap().len(), 6);
}

#[test]
fn replay_is_bound_to_original_target_and_does_not_duplicate_notes() {
    let fixture = Fixture::new();
    let request = fixture.request("B");
    let original = fixture.rename(&request).unwrap();
    fixture.rename(&fixture.request("C")).unwrap();
    let replay = fixture.rename(&request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.event, original.event);
    assert_eq!(fixture.record().formal_agent_name.as_deref(), Some("C"));
    assert_eq!(pending_at(&fixture.path, 100).unwrap().len(), 4);
    let mut conflicting = request.clone();
    conflicting.name = "Other".into();
    assert_eq!(
        code(&fixture.rename(&conflicting).unwrap_err()),
        "idempotency_conflict"
    );
    assert_eq!(
        code(
            &store::rename_formal_name_at(
                &fixture.path,
                "cutex.someone-else",
                &request,
                &fixture.scope,
                None,
                "human-management"
            )
            .unwrap_err()
        ),
        "idempotency_conflict"
    );
    let note = pending_at(&fixture.path, 1).unwrap().pop().unwrap();
    delivered_at(&fixture.path, &note.event_id, &note.recipient_session_id).unwrap();
    delivered_at(&fixture.path, &note.event_id, &note.recipient_session_id).unwrap();
    assert_eq!(pending_at(&fixture.path, 100).unwrap().len(), 3);
}

#[test]
fn stale_revision_active_case_collision_and_history_scope_are_checked_before_write() {
    let fixture = Fixture::new();
    fixture.insert("cutex.other", "Existing");
    let before = fixture.record();
    assert_eq!(
        code(&fixture.rename(&fixture.request("existing")).unwrap_err()),
        "agent_name_conflict"
    );
    let mut stale = fixture.request("B");
    stale.expected_revision += 1;
    assert_eq!(
        code(&fixture.rename(&stale).unwrap_err()),
        "stale_session_revision"
    );
    assert_eq!(fixture.record(), before);
    assert!(fixture.history().events.is_empty());
    fixture.rename(&fixture.request("B")).unwrap();
    assert!(available_at(&fixture.path, &fixture.scope, "a", Some(&fixture.id)).is_ok());
    assert_eq!(
        code(&available_at(&fixture.path, &fixture.scope, "a", Some("cutex.other")).unwrap_err()),
        "agent_name_reserved"
    );
    assert!(available_at(
        &fixture.path,
        &NameScope::for_project(Some("project-b")),
        "a",
        Some("cutex.other")
    )
    .is_ok());
}

#[test]
fn crash_before_audit_commit_rolls_back_name_revision_claim_and_outbox() {
    let fixture = Fixture::new();
    let before = fixture.record();
    with_connection(&database_path(&fixture.path),|connection| {
        connection.execute_batch("CREATE TRIGGER abort_rename BEFORE INSERT ON agent_name_events BEGIN SELECT RAISE(ABORT,'test interrupted commit'); END;")?; Ok(())
    }).unwrap();
    assert!(fixture.rename(&fixture.request("B")).is_err());
    assert_eq!(fixture.record(), before);
    assert!(fixture.history().events.is_empty());
    assert!(claims_at(&fixture.path, Some(&fixture.scope), "A")
        .unwrap()
        .1
        .is_empty());
    assert!(pending_at(&fixture.path, 100).unwrap().is_empty());
}

#[test]
fn concurrent_renames_have_one_cas_winner() {
    let fixture = Fixture::new();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["Blue", "Green"]
        .into_iter()
        .map(|name| {
            let request = fixture.request(name);
            let (path, id, scope, barrier) = (
                fixture.path.clone(),
                fixture.id.clone(),
                fixture.scope.clone(),
                barrier.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                store::rename_formal_name_at(&path, &id, &request, &scope, None, "human-management")
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        code(
            results
                .into_iter()
                .find(|result| result.is_err())
                .unwrap()
                .as_ref()
                .unwrap_err()
        ),
        "stale_session_revision"
    );
    assert_eq!(fixture.history().events.len(), 1);
}

#[test]
fn concurrent_rename_and_late_create_publication_keep_one_active_current_name() {
    let fixture=Fixture::new();
    let request=fixture.request("New");
    let created=format!("cutex.{}",uuid::Uuid::new_v4());
    let mut candidate=CutexSessionRecord::new(created.clone(),None,"host".into(),"/workspace".into(),None).unwrap();
    candidate.formal_agent_name=Some("New".into());candidate.agent_enabled=true;
    candidate.creation_action=Some("human-agent:slow-bootstrap".into());
    let barrier=Arc::new(std::sync::Barrier::new(2));
    let (path,id,scope,rename_barrier)=(fixture.path.clone(),fixture.id.clone(),fixture.scope.clone(),barrier.clone());
    let rename=std::thread::spawn(move|| {
        rename_barrier.wait();
        store::rename_formal_name_at(&path,&id,&request,&scope,None,"human-management")
    });
    let (path,scope,created_id)=(fixture.path.clone(),fixture.scope.clone(),created.clone());
    let create=std::thread::spawn(move|| {
        let _scope=crate::agent_names::mutation_scope::enter(scope);
        barrier.wait();
        store::with_locked_session_rows(&path,|rows| {rows.insert_session(&created_id,candidate);rows.commit()?;Ok(())})
    });
    let renamed=rename.join().unwrap();let published=create.join().unwrap();
    assert_ne!(renamed.is_ok(),published.is_ok(),"exactly one stale-precheck publication may claim New");
    if let Err(error)=renamed.as_ref() {assert_eq!(code(error),"agent_name_conflict");}
    if let Err(error)=published.as_ref() {assert_eq!(code(error),"agent_name_conflict");}
    let count=with_connection(&database_path(&fixture.path),|connection| {
        Ok(connection.query_row("SELECT COUNT(*) FROM sessions WHERE archive_state='active' AND formal_agent_name='New' COLLATE NOCASE",[],|row|row.get::<_,i64>(0))?)
    }).unwrap();
    assert_eq!(count,1);
    assert_eq!(fixture.history().events.len(),usize::from(renamed.is_ok()));
}

#[test]
fn concurrent_rename_and_restore_keep_one_active_current_name() {
    let fixture=Fixture::new();
    let restored=format!("cutex.{}",uuid::Uuid::new_v4());
    fixture.insert(&restored,"New");
    store::with_locked_session_rows(&fixture.path,|rows| {
        let mut record=rows.session(&restored).unwrap().clone();
        record.archive_state=crate::session::model::CutexSessionArchiveState::Retired;
        rows.insert_session(&restored,record);rows.commit()?;Ok(())
    }).unwrap();
    let request=fixture.request("New");
    let barrier=Arc::new(std::sync::Barrier::new(2));
    let (path,id,scope,rename_barrier)=(fixture.path.clone(),fixture.id.clone(),fixture.scope.clone(),barrier.clone());
    let rename=std::thread::spawn(move|| {
        rename_barrier.wait();store::rename_formal_name_at(&path,&id,&request,&scope,None,"human-management")
    });
    let (path,scope)=(fixture.path.clone(),fixture.scope.clone());
    let restore=std::thread::spawn(move|| {
        let _scope=crate::agent_names::mutation_scope::enter(scope);barrier.wait();
        store::with_locked_session_rows(&path,|rows| {
            let mut record=rows.session(&restored).unwrap().clone();
            record.archive_state=crate::session::model::CutexSessionArchiveState::Active;
            rows.insert_session(&restored,record);rows.commit()?;Ok(())
        })
    });
    let renamed=rename.join().unwrap();let restored=restore.join().unwrap();
    assert_ne!(renamed.is_ok(),restored.is_ok());
    if let Err(error)=renamed.as_ref() {assert_eq!(code(error),"agent_name_conflict");}
    if let Err(error)=restored.as_ref() {assert_eq!(code(error),"agent_name_conflict");}
    let count=with_connection(&database_path(&fixture.path),|connection| {
        Ok(connection.query_row("SELECT COUNT(*) FROM sessions WHERE archive_state='active' AND formal_agent_name='New' COLLATE NOCASE",[],|row|row.get::<_,i64>(0))?)
    }).unwrap();assert_eq!(count,1);
}

#[test]
fn session_deletion_preserves_minimal_ledger_claims_and_exact_replay() {
    let fixture = Fixture::new();
    let request = fixture.request("B");
    let event = fixture.rename(&request).unwrap().event;
    store::with_locked_session_rows(&fixture.path, |rows| {
        rows.remove_session(&fixture.id);
        rows.commit()?;
        Ok(())
    })
    .unwrap();
    assert_eq!(fixture.history().events, [event.clone()]);
    assert_eq!(fixture.rename(&request).unwrap().event, event);
    assert_eq!(
        code(&available_at(&fixture.path, &fixture.scope, "A", None).unwrap_err()),
        "agent_name_reserved"
    );
}

#[test]
fn director_rename_has_only_the_own_note() {
    let fixture = Fixture::new();
    let request = fixture.request("B");
    store::rename_formal_name_at(
        &fixture.path,
        &fixture.id,
        &request,
        &fixture.scope,
        Some(&fixture.id),
        "human-management",
    )
    .unwrap();
    let notes = pending_at(&fixture.path, 100).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].recipient_session_id, fixture.id);
}

#[test]
fn final_writer_guards_enforce_scoped_claims_on_create_and_restore() {
    let fixture = Fixture::new();
    fixture.rename(&fixture.request("B")).unwrap();
    let mut candidate = CutexSessionRecord::new(
        "cutex.new".into(),
        None,
        "host".into(),
        "/workspace".into(),
        None,
    )
    .unwrap();
    candidate.formal_agent_name = Some("A".into());
    candidate.agent_enabled = true;
    let _scope = crate::agent_names::mutation_scope::enter(fixture.scope.clone());
    let error = store::with_locked_session_rows(&fixture.path, |rows| {
        rows.insert_session(&candidate.cutex_session_id, candidate.clone());
        rows.commit()?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(code(&error), "agent_name_reserved");
    assert!(
        store::load_session_row(&fixture.path, &candidate.cutex_session_id)
            .unwrap()
            .is_none()
    );
    // Preserve an archived legacy record; only restoring it activates the
    // check against the scope's permanent old-name reservation.
    candidate.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
    store::with_locked_session_rows(&fixture.path, |rows| {
        rows.insert_session(&candidate.cutex_session_id, candidate.clone());
        rows.commit()?;
        Ok(())
    })
    .unwrap();
    let error = store::with_locked_session_rows(&fixture.path, |rows| {
        rows.session_mut(&candidate.cutex_session_id)
            .unwrap()
            .archive_state = crate::session::model::CutexSessionArchiveState::Active;
        rows.commit()?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(code(&error), "agent_name_reserved");
    assert!(
        store::load_session_row(&fixture.path, &candidate.cutex_session_id)
            .unwrap()
            .unwrap()
            .1
            .is_retired()
    );
}

#[test]
fn stale_runtime_configuration_cannot_overwrite_an_audited_owner_name() {
    let fixture = Fixture::new();
    fixture.rename(&fixture.request("B")).unwrap();
    let error = store::with_locked_session_rows(&fixture.path, |rows| {
        rows.session_mut(&fixture.id).unwrap().formal_agent_name = Some("A".into());
        rows.commit()?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(code(&error), "formal_name_owner_rename_required");
    assert_eq!(fixture.record().formal_agent_name.as_deref(), Some("B"));
    assert_eq!(fixture.history().events.len(), 1);
}

#[test]
fn history_is_paginated_and_never_has_the_profile_directory_2000_entry_cap() {
    let fixture = Fixture::new();
    for index in 0..2005 {
        fixture
            .rename(&fixture.request(&format!("Name-{index}")))
            .unwrap();
    }
    let mut cursor = None;
    let mut events = Vec::new();
    loop {
        let page = history_at(&fixture.path, &fixture.id, cursor, 100).unwrap();
        events.extend(page.events);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(events.len(), 2005);
    assert_eq!(events.last().unwrap().old_name, "A");
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence > pair[1].sequence));
}
