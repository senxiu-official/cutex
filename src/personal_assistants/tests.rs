use super::{
    host,
    inbox::{Draft, Response},
    *,
};
use serde_json::json;
mod limits_and_recovery;
mod recurring_agenda;

struct Fixture {
    root: std::path::PathBuf,
    store: Store,
    assistant: Assistant,
    session: String,
    project: String,
}
fn isolated(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(id(label));
    let store = Store::open(&root).unwrap();
    let project = id("empty-test-project");
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    let cfg = Config {
        name: "Test assistant".into(),
        profile: "fixture-profile".into(),
        home_instance_id: crate::management::instance::instance_id(),
        project_ids: [project.clone()].into(),
    };
    let assistant = store.begin("owner", "create", cfg, |_| Ok(())).unwrap();
    assert!(assistant.session_id.is_none());
    let assistant = store.bind(&assistant.id, &session).unwrap();
    Fixture {
        root,
        store,
        assistant,
        session,
        project,
    }
}
fn draft(f: &Fixture, text: &str) -> Draft {
    Draft {
        project_id: f.project.clone(),
        outcome: text.into(),
        constraints: "within this project".into(),
        source_ref: "test-owner-input-1".into(),
        due_at: None,
    }
}
fn director(epoch: u64) -> Seat {
    Seat {
        session_id: Some("cutex.private-test-director".into()),
        epoch,
    }
}
fn must_fail<T: std::fmt::Debug>(result: Result<T>, code: &str) {
    assert_eq!(result.unwrap_err().downcast_ref::<Fault>().unwrap().0, code);
}

#[test]
fn empty_registry_creation_replay_conflict_overlap_and_per_assistant_grants() {
    let f = isolated("registry");
    let store = &f.store;
    let created = store
        .begin("owner", "create", f.assistant.config.clone(), |_| {
            panic!("replay re-ran provisioning validation")
        })
        .unwrap();
    assert_eq!(created.id, f.assistant.id);
    let mut cfg = created.config.clone();
    cfg.name = "Second test assistant".into();
    must_fail(
        store.begin("owner", "create", cfg.clone(), |_| Ok(())),
        "action_conflict",
    );
    let other = store.begin("owner", "second", cfg, |_| Ok(())).unwrap();
    let other = store.bind(&other.id, "cutex.private-second").unwrap();
    assert_eq!(
        store.overlap(&other).unwrap()[0]["assistantId"],
        f.assistant.id
    );
    let grants = Grants {
        full_owner: false,
        operations: [Operation::Publishing].into(),
        ..Default::default()
    };
    let update = Update::Grants {
        grants: grants.clone(),
    };
    let changed = store
        .configure("owner", &f.assistant.id, "grant", 2, &update, |_| Ok(()))
        .unwrap();
    assert_eq!(changed.grants, grants);
    assert_eq!(store.get(&other.id).unwrap().grants, Grants::default());
    must_fail(
        store.configure(
            "owner",
            &changed.id,
            "conflicting",
            2,
            &Update::Enabled { enabled: false },
            |_| Ok(()),
        ),
        "revision_conflict",
    );
    let reopened = Store::open(&f.root).unwrap();
    assert_eq!(reopened.get(&changed.id).unwrap(), changed);
    let full = Grants {
        full_owner: true,
        ..grants
    };
    must_fail(
        reopened.configure(
            "owner",
            &changed.id,
            "grant-all",
            3,
            &Update::Grants {
                grants: full.clone(),
            },
            |_| Ok(()),
        ),
        "full_owner_native_confirmation_required",
    );
    // Model a grant already stored by r295. Revocation remains available.
    reopened.connection().unwrap().execute(
        "UPDATE pa_assistants SET body=json_set(body,'$.grants.fullOwner',json('true')) WHERE id=?1",
        [&changed.id]).unwrap();
    let revoked = reopened
        .configure(
            "owner",
            &changed.id,
            "revoke",
            3,
            &Update::Grants {
                grants: Grants::default(),
            },
            |_| Ok(()),
        )
        .unwrap();
    assert!(!revoked.grants.full_owner);
    let audit = reopened.audit(&changed.id, i64::MAX, 20).unwrap();
    assert_eq!(audit[0]["detail"]["before"]["grants"]["fullOwner"], true);
    assert_eq!(audit[0]["detail"]["after"]["grants"]["fullOwner"], false);
}

#[test]
fn director_confirmation_fences_identity_and_epoch_and_preserves_admission_order() {
    let f = isolated("inbox");
    let store = &f.store;
    let seat = director(4);
    let first = store
        .prepare(&f.session, "draft1", &draft(&f, "First draft"), &seat)
        .unwrap();
    let second = store
        .prepare(&f.session, "draft2", &draft(&f, "Second draft"), &seat)
        .unwrap();
    let target = seat.session_id.as_deref().unwrap();
    assert!(store
        .requests(None, Some(target), 0, 20)
        .unwrap()
        .is_empty());
    must_fail(
        store.confirm(
            "owner",
            &f.assistant.id,
            &first.id,
            "stale",
            1,
            f.assistant.revision,
            &seat,
            &director(5),
        ),
        "director_changed",
    );
    for r in [&second, &first] {
        let observed = store
            .confirm(
                "owner",
                &f.assistant.id,
                &r.id,
                &r.id,
                1,
                f.assistant.revision,
                &seat,
                &seat,
            )
            .unwrap();
        assert_eq!(observed.state, RequestState::Queued);
    }
    let page = store.requests(None, Some(target), 0, 20).unwrap();
    assert_eq!(
        page.iter().map(|(_, r)| &r.id).collect::<Vec<_>>(),
        vec![&second.id, &first.id]
    );
    must_fail(
        store.respond(
            target,
            &first.id,
            "wrong-epoch",
            2,
            Response::Accept,
            "",
            &director(5),
        ),
        "director_changed",
    );
    let accepted = store
        .respond(target, &first.id, "accept", 2, Response::Accept, "", &seat)
        .unwrap();
    let completed = store
        .respond(
            target,
            &first.id,
            "report",
            accepted.revision,
            Response::Report,
            "Result in test project",
            &seat,
        )
        .unwrap();
    assert_eq!(completed.state, RequestState::Completed);
    assert_eq!(
        store
            .respond(
                target,
                &first.id,
                "report",
                accepted.revision,
                Response::Report,
                "Result in test project",
                &seat
            )
            .unwrap()
            .revision,
        completed.revision
    );
    let mut notices = Vec::new();
    store
        .deliver_notices(|key, to, body| {
            assert!(!body.contains("First draft"));
            notices.push((key.to_owned(), to.to_owned()));
            Ok(())
        })
        .unwrap();
    assert_eq!(notices.len(), 4);
    store
        .deliver_notices(|_, _, _| panic!("already delivered"))
        .unwrap();
}

#[test]
fn owner_can_reconfirm_an_accepted_request_for_a_new_director_without_changing_its_identity() {
    let f = isolated("new-director");
    let old = director(1);
    let new = Seat {
        session_id: Some(format!("cutex.{}", uuid::Uuid::new_v4())),
        epoch: 2,
    };
    let request = f
        .store
        .prepare(&f.session, "prepare", &draft(&f, "Follow up"), &old)
        .unwrap();
    let confirm = |key: &str, revision: u64, reviewed: &Seat, current: &Seat| {
        f.store.confirm(
            "owner",
            &f.assistant.id,
            &request.id,
            key,
            revision,
            f.assistant.revision,
            reviewed,
            current,
        )
    };
    confirm("first", 1, &old, &old).unwrap();
    let old_id = old.session_id.as_deref().unwrap();
    let new_id = new.session_id.as_deref().unwrap();
    f.store
        .respond(
            old_id,
            &request.id,
            "accept",
            2,
            Response::Accept,
            "Waiting for a result",
            &old,
        )
        .unwrap();
    must_fail(confirm("stale", 3, &old, &new), "director_changed");
    must_fail(confirm("duplicate", 3, &old, &old), "invalid_transition");
    assert!(f
        .store
        .requests(None, Some(new_id), 0, 5)
        .unwrap()
        .is_empty());
    let transfer = confirm("new-seat", 3, &new, &new).unwrap();
    assert_eq!(transfer.id, request.id);
    assert_eq!(transfer.state, RequestState::Queued);
    assert_eq!(transfer.detail.as_deref(), Some("Waiting for a result"));
    assert!(f
        .store
        .requests(None, Some(old_id), 0, 5)
        .unwrap()
        .is_empty());
    assert_eq!(f.store.requests(None, Some(new_id), 0, 5).unwrap().len(), 1);
    assert_eq!(
        confirm("new-seat", 3, &new, &new).unwrap().revision,
        transfer.revision
    );
    assert_eq!(
        f.store
            .respond(new_id, &request.id, "resume", 4, Response::Accept, "", &new)
            .unwrap()
            .state,
        RequestState::Accepted
    );
}

#[test]
fn revoked_scope_does_not_reauthorize_old_request_and_clarification_needs_new_review() {
    let f = isolated("scope");
    let seat = director(1);
    let target = seat.session_id.as_deref().unwrap();
    let req = f
        .store
        .prepare(&f.session, "draft", &draft(&f, "Outcome"), &seat)
        .unwrap();
    f.store
        .confirm(
            "owner",
            &f.assistant.id,
            &req.id,
            "confirm",
            1,
            f.assistant.revision,
            &seat,
            &seat,
        )
        .unwrap();
    let q = f
        .store
        .respond(
            target,
            &req.id,
            "clarify",
            2,
            Response::Clarify,
            "Which month?",
            &seat,
        )
        .unwrap();
    let changed = f
        .store
        .revise(
            &f.session,
            &req.id,
            "revision",
            q.revision,
            &draft(&f, "Use June"),
            &seat,
        )
        .unwrap();
    assert_eq!(changed.state, RequestState::AwaitingOwner);
    assert!(f
        .store
        .requests(None, Some(target), 0, 5)
        .unwrap()
        .is_empty());
    f.store
        .configure(
            "owner",
            &f.assistant.id,
            "remove-scope",
            2,
            &Update::Settings {
                name: "Assistant".into(),
                project_ids: Default::default(),
            },
            |_| Ok(()),
        )
        .unwrap();
    must_fail(
        f.store.confirm(
            "owner",
            &f.assistant.id,
            &req.id,
            "bad",
            changed.revision,
            f.assistant.revision,
            &seat,
            &seat,
        ),
        "project_outside_scope",
    );
    must_fail(
        tools::call(
            &f.store,
            &f.session,
            "pa_request_read",
            &json!({"requestId":req.id}),
        ),
        "project_outside_scope",
    );
}

#[test]
fn memory_and_tool_calls_are_scoped_to_authenticated_session() {
    let f = isolated("memory");
    tools::call(&f.store,&f.session,"pa_memory_write",&json!({"actionId":"note","key":"preference","expectedRevision":0,"content":"June","sourceRef":"test-input"})).unwrap();
    assert_eq!(
        tools::call(
            &f.store,
            &f.session,
            "pa_memory_search",
            &json!({"query":"Jun"})
        )
        .unwrap()["items"][0]["key"],
        "preference"
    );
    must_fail(
        tools::call(&f.store, "cutex.ordinary", "pa_self", &json!({})),
        "assistant_required",
    );
    must_fail(
        tools::call(&f.store, &f.session, "pa_self", &json!({"owner":true})),
        "invalid_arguments",
    );
    must_fail(
        tools::call(
            &f.store,
            &f.session,
            "pa_memory_write",
            &json!({"actionId":"overwrite","key":"preference","expectedRevision":0,"content":"July","sourceRef":"test-input"}),
        ),
        "revision_conflict",
    );
    f.store
        .configure(
            "owner",
            &f.assistant.id,
            "disable",
            2,
            &Update::Enabled { enabled: false },
            |_| Ok(()),
        )
        .unwrap();
    must_fail(
        tools::call(&f.store, &f.session, "pa_self", &json!({})),
        "assistant_disabled",
    );
    assert!(!tools::definitions()
        .iter()
        .any(|t| t["name"] == "pa_owner_confirm"));
}

#[test]
fn empty_eva_fixture_project_has_no_director_or_members() {
    use crate::agent_management::{
        AgentManagementProvider, ProjectId, ProjectPaletteColor, ProjectPresentationInput,
    };
    use crate::management::control_plane::*;
    let provider = AgentManagementProvider::open_default().unwrap();
    let project = id("pa-test-empty");
    let request = HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: crate::agent_management::AgentActionId::new(id("test")).unwrap(),
        project_id: ProjectId::new(&project).unwrap(),
        expected_authority_epoch: None,
        expected_project_revision: None,
        operation: HumanManagementProjectMutationKind::CreateProject {
            presentation: ProjectPresentationInput {
                display_name: "Empty assistant fixture".into(),
                badge_label: "T".into(),
                color: ProjectPaletteColor::Blue,
                icon: None,
            },
            workspace_root: None,
            create_workspace_root: false,
            director_cutex_session_id: None,
        },
    };
    provider
        .execute_project_mutation_for_management(
            &HumanManagementPrincipal::authenticated(),
            &request,
        )
        .unwrap();
    let dir = provider.store().directory_snapshot().unwrap();
    assert_eq!(
        host::seat(&dir, &project).unwrap(),
        Seat {
            session_id: None,
            epoch: 0
        }
    );
    assert!(crate::memory::project_member_session_ids(&dir, &request.project_id).is_empty());
    let store = Store::open(&std::env::temp_dir().join(id("pa-test-registry"))).unwrap();
    assert!(store.list("", 10).unwrap().is_empty());
    let a = store
        .begin(
            "owner",
            "first",
            Config {
                name: "Private assistant".into(),
                profile: "private".into(),
                home_instance_id: crate::management::instance::instance_id(),
                project_ids: [project.clone()].into(),
            },
            |c| host::validate_scope(c, &dir),
        )
        .unwrap();
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    store.bind(&a.id, &session).unwrap();
    assert_eq!(
        tools::call(
            &store,
            &session,
            "pa_project_read",
            &json!({"projectId":project})
        )
        .unwrap()["director"]["sessionId"],
        Value::Null
    );
}
use serde_json::Value;
#[test]
fn concurrent_duplicate_creation_and_configure_return_one_binding_and_one_winner() {
    let f = isolated("concurrency");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for i in 0..2 {
        let barrier = barrier.clone();
        let store = f.store.clone();
        let id = f.assistant.id.clone();
        let cfg = f.assistant.config.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let duplicate = store
                .begin("owner", "concurrent-create", cfg, |_| Ok(()))
                .unwrap();
            let update = store.configure(
                "owner",
                &id,
                &format!("concurrent-{i}"),
                2,
                &Update::Enabled { enabled: i == 0 },
                |_| Ok(()),
            );
            (duplicate.id, update.is_ok())
        }));
    }
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results[0].0, results[1].0);
    assert_eq!(results.iter().filter(|(_, ok)| *ok).count(), 1);
    assert_eq!(f.store.list("", 10).unwrap().len(), 2);
    assert_eq!(f.store.get(&f.assistant.id).unwrap().revision, 3);
}

#[test]
fn in_flight_accepted_work_can_report_after_scope_revocation_without_notifying_source() {
    let f = isolated("accepted-work");
    let seat = director(2);
    let target = seat.session_id.as_deref().unwrap();
    let req = f
        .store
        .prepare(
            &f.session,
            "prepare",
            &draft(&f, "A fixed project outcome"),
            &seat,
        )
        .unwrap();
    f.store
        .confirm(
            "owner",
            &f.assistant.id,
            &req.id,
            "approve",
            1,
            f.assistant.revision,
            &seat,
            &seat,
        )
        .unwrap();
    f.store
        .respond(target, &req.id, "accept", 2, Response::Accept, "", &seat)
        .unwrap();
    f.store.deliver_notices(|_, _, _| Ok(())).unwrap();
    f.store
        .configure(
            "owner",
            &f.assistant.id,
            "revoke",
            2,
            &Update::Enabled { enabled: false },
            |_| Ok(()),
        )
        .unwrap();
    let r = f
        .store
        .respond(
            target,
            &req.id,
            "done",
            3,
            Response::Report,
            "Completed in its project",
            &seat,
        )
        .unwrap();
    assert_eq!(r.state, RequestState::Completed);
    f.store
        .deliver_notices(|_, _, _| panic!("a disabled source must not be woken"))
        .unwrap();
}

#[test]
fn overlap_returns_the_first_hundred_project_matches_and_one_lookahead() {
    let f = isolated("overlap-page");
    let mut cfg = f.assistant.config.clone();
    cfg.project_ids = (0..130).map(|n| format!("p-{n:03}")).collect();
    let source = f
        .store
        .begin("owner", "source", cfg.clone(), |_| Ok(()))
        .unwrap();
    let peer = f.store.begin("owner", "peer", cfg, |_| Ok(())).unwrap();
    let rows = f.store.overlap(&source).unwrap();
    assert_eq!(rows.len(), 101);
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row["assistantId"], peer.id);
        assert_eq!(row["projectId"], format!("p-{n:03}"));
    }
}

#[test]
fn agenda_pages_equal_time_reminders_by_message_id_in_delivery_order() {
    use crate::agent_bus::alarms::{self, SetArgs};
    use chrono::{Duration, Local, SecondsFormat, Timelike};
    let f = isolated("agenda-ties");
    let day = (Local::now() + Duration::days(2)).with_hour(12).unwrap();
    let mut expected = Vec::new();
    for (name, seconds) in [("a", 4), ("b", 2), ("c", 0), ("d", 0), ("e", 0), ("f", 1)] {
        let time = day + Duration::seconds(seconds);
        let receipt = alarms::set(
            &f.session,
            SetArgs {
                alarm_id: name.into(),
                to: f.session.clone(),
                message: name.into(),
                at: Some(time.to_rfc3339()),
                delay_seconds: None,
            },
        )
        .unwrap();
        let key = format!(
            "{}|{}",
            time.to_utc().to_rfc3339_opts(SecondsFormat::Nanos, true),
            receipt["message_id"].as_str().unwrap()
        );
        expected.push((key, name));
    }
    expected.sort();
    let mut args = json!({"date":day.date_naive().to_string(),"limit":2});
    for (index, chunk) in expected.chunks(2).enumerate() {
        let page = tools::call(&f.store, &f.session, "pa_agenda_read", &args).unwrap();
        let ids: Vec<_> = page["reminders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["alarmId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [chunk[0].1, chunk[1].1]);
        if index == 2 {
            assert!(page["nextReminderCursor"].is_null());
        } else {
            assert_eq!(page["nextReminderCursor"], chunk[1].0);
            args["reminderCursor"] = page["nextReminderCursor"].clone();
        }
    }
}

#[test]
fn agenda_uses_the_home_local_day_and_only_this_assistants_reminders() {
    use chrono::{Duration, Local, Timelike};
    let f = isolated("agenda");
    let day = (Local::now() + Duration::days(2)).with_hour(12).unwrap();
    let at = day.to_rfc3339();
    let args = json!({"alarmId":"personal","at":at,"message":"Test follow-up"});
    tools::call(&f.store, &f.session, "pa_reminder_set", &args).unwrap();
    let other_assistant = isolated("other-agenda");
    tools::call(
        &other_assistant.store,
        &other_assistant.session,
        "pa_reminder_set",
        &args,
    )
    .unwrap();
    let later = json!({"alarmId":"later","at":(day + Duration::seconds(1)).to_rfc3339(),"message":"Second follow-up"});
    tools::call(&f.store, &f.session, "pa_reminder_set", &later).unwrap();
    let agenda = tools::call(
        &f.store,
        &f.session,
        "pa_agenda_read",
        &json!({"date":day.date_naive().to_string(),"limit":1}),
    )
    .unwrap();
    assert_eq!(agenda["reminders"][0]["message"], "Test follow-up");
    assert_eq!(agenda["reminders"].as_array().unwrap().len(), 1);
    let second = tools::call(&f.store, &f.session, "pa_agenda_read",
        &json!({"date":day.date_naive().to_string(),"limit":1,"reminderCursor":agenda["nextReminderCursor"]})).unwrap();
    assert_eq!(second["reminders"][0]["alarmId"], "later");
    assert!(second["nextReminderCursor"].is_null());
    let other = tools::call(
        &f.store,
        &f.session,
        "pa_agenda_read",
        &json!({"date":Local::now().date_naive().to_string()}),
    )
    .unwrap();
    assert_eq!(other["reminders"], json!([]));
    tools::call(
        &f.store,
        &f.session,
        "pa_reminder_cancel",
        &json!({"alarmId":"personal"}),
    )
    .unwrap();
    tools::call(
        &f.store,
        &f.session,
        "pa_reminder_cancel",
        &json!({"alarmId":"later"}),
    )
    .unwrap();
    let empty = tools::call(
        &f.store,
        &f.session,
        "pa_agenda_read",
        &json!({"date":day.date_naive().to_string()}),
    )
    .unwrap();
    assert_eq!(empty["reminders"], json!([]));
}
