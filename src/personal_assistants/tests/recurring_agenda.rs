use super::*;
use crate::personal_assistants::recurring::{self, CoordinationTask, Template};
use crate::schedules::{self, CreateArgs, DeleteArgs, Rule, Run};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};

#[test]
fn owner_project_history_spans_assistants_and_filters_director_epoch_before_paging() {
    use owner_views::RequestFilter;
    let f = isolated("owner-history");
    let second = f
        .store
        .begin("owner", "second", f.assistant.config.clone(), |_| Ok(()))
        .unwrap();
    let second = f.store.bind(&second.id, "cutex.fixture-second").unwrap();
    let wanted = director(2);
    let first = f
        .store
        .prepare(&f.session, "one", &draft(&f, "First"), &wanted)
        .unwrap();
    let second_request = f
        .store
        .prepare(
            second.session_id.as_deref().unwrap(),
            "two",
            &draft(&f, "Second"),
            &wanted,
        )
        .unwrap();
    f.store
        .prepare(
            &f.session,
            "next-seat",
            &draft(&f, "Later seat"),
            &director(3),
        )
        .unwrap();
    let page = |cursor| {
        f.store
            .owner_requests(
                RequestFilter {
                    project: Some(f.assistant.config.project_ref(&f.project)),
                    director: wanted.session_id.clone(),
                    epoch: Some(2),
                    state: Some(RequestState::AwaitingOwner),
                    ..Default::default()
                },
                cursor,
                1,
            )
            .unwrap()
    };
    let p1 = page(i64::MAX);
    assert_eq!(p1["items"][0]["id"], second_request.id);
    let p2 = page(p1["nextCursor"].as_i64().unwrap());
    assert_eq!(p2["items"][0]["id"], first.id);
    assert!(p2["nextCursor"].is_null());
    assert_eq!(
        f.store.owner_notes(&second.id, "", "", 10).unwrap()["items"],
        json!([])
    );
}

fn setup(
    label: &str,
) -> (
    Fixture,
    schedules::Store,
    schedules::Schedule,
    DateTime<Utc>,
) {
    let f = isolated(label);
    let store = schedules::Store::open(f.root.join("schedules")).unwrap();
    let start = Utc.with_ymd_and_hms(2026, 11, 2, 0, 0, 0).unwrap();
    let template = Template {
        assistant_id: f.assistant.id.clone(),
        assistant_revision: f.assistant.revision,
        project_ref: f.assistant.config.project_ref(&f.project),
        template_revision: 1,
        confirmed_seat: director(4),
        task: CoordinationTask::ProjectStatus,
    };
    let schedule = store
        .create_linked(
            CreateArgs {
                action_id: "template".into(),
                target_session_id: f.session.clone(),
                message: "coordination".into(),
                rule: Rule::Interval { hours: 1 },
                enabled: true,
            },
            Some(template),
            start,
            &Utc,
            |_| Ok(()),
        )
        .unwrap();
    (f, store, schedule, start)
}
struct InboxRuntime<'a> {
    pa: &'a Store,
    seat: Seat,
    stopped: bool,
}
impl schedules::runtime::Runtime for InboxRuntime<'_> {
    fn check(&mut self, _: &Run) -> Result<()> {
        panic!("template used ordinary dispatch")
    }
    fn online(&mut self, _: &Run) -> Result<()> {
        panic!("template woke an agent")
    }
    fn handoff(&mut self, _: &Run) -> Result<String> {
        panic!("template sent an alarm")
    }
    fn submit_template(&mut self, store: &schedules::Store, run: &Run) -> Result<String> {
        let gate = if self.stopped {
            Err(schedules::runtime::OwnerStopped.into())
        } else {
            Ok(())
        };
        recurring::admit_run(self.pa, store, run, gate, |_| Ok(self.seat.clone()))
    }
}

#[test]
fn concurrent_admission_returns_one_request_and_one_notification() {
    let (f, store, _, start) = setup("race-occurrence");
    let run = store
        .claim(start + Duration::hours(1), &Utc, 1)
        .unwrap()
        .pop()
        .unwrap();
    let start_both = std::sync::Barrier::new(2);
    let admit = || {
        start_both.wait();
        recurring::admit_run(&f.store, &store, &run, Ok(()), |_| Ok(director(4))).unwrap()
    };
    std::thread::scope(|threads| {
        let first = threads.spawn(admit);
        let second = threads.spawn(admit);
        assert_eq!(first.join().unwrap(), second.join().unwrap());
    });
    let conn = f.store.connection().unwrap();
    let count = |query| conn.query_row(query, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT count(*) FROM pa_requests"), 1);
    assert_eq!(count("SELECT count(*) FROM pa_notices"), 1);
    assert_eq!(
        count("SELECT count(*) FROM pa_audit WHERE verb='scheduledRequest'"),
        1
    );
}

#[test]
fn rename_and_unrelated_projects_preserve_approval_but_regrant_and_seat_changes_notify_once() {
    use crate::owner_notify::store::{Level, NewNotification, OwnerNotifyStore, TemplateReview};
    let (f, schedules, original, start) = setup("scope-notices");
    let mut runtime = InboxRuntime {
        pa: &f.store,
        seat: director(4),
        stopped: false,
    };
    let configure = |action: &str, projects: BTreeSet<String>| {
        f.store
            .configure(
                "owner",
                &f.assistant.id,
                action,
                f.store.get(&f.assistant.id).unwrap().revision,
                &Update::Settings {
                    name: action.into(),
                    project_ids: projects,
                },
                |_| Ok(()),
            )
            .unwrap()
    };
    let both = [f.project.clone(), "another".to_string()].into();
    configure("rename-and-add", both);
    let tick = |hour, rt: &mut InboxRuntime<'_>| {
        schedules::runtime::tick(&schedules, start + Duration::hours(hour), &Utc, rt).unwrap()
    };
    tick(1, &mut runtime);
    let req = schedules
        .get(&original.id)
        .unwrap()
        .last_run
        .unwrap()
        .request_id
        .unwrap();
    configure("rename-and-drop-unrelated", [f.project.clone()].into());
    let accepted = f
        .store
        .respond(
            director(4).session_id.as_deref().unwrap(),
            &req,
            "accept",
            1,
            Response::Accept,
            "",
            &director(4),
        )
        .unwrap();
    assert_eq!(accepted.state, RequestState::Accepted);
    configure("remove", BTreeSet::new());
    let now = configure("grant-again", [f.project.clone()].into());
    tick(2, &mut runtime);
    tick(3, &mut runtime);
    let history = schedules
        .template_history(&f.assistant.id, &original.id, i64::MAX, 20)
        .unwrap();
    let warnings: Vec<_> = history
        .iter()
        .filter(|e| e["verb"] == "reconfirmationRequired")
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["reason"], "confirmation_stale");
    let notices = OwnerNotifyStore::at(f.root.join("owner-notifications"));
    let (settings, evaluator) = crate::notification_levels::Evaluator::current_with_owner();
    let deliver = |review: &TemplateReview, session: &str| {
        assert_eq!(review.assistant_id, f.assistant.id);
        notices.template_review_once(
            review,
            NewNotification {
                agent_id: session.into(),
                agent_name: None,
                text: "请重新确认模板".into(),
                level: Level::Info,
                needs_reply: false,
                delegation: None,
                job: None,
                turn_id: None,
            },
            &settings,
            &evaluator,
            start,
        )
    };
    assert!(schedules
        .deliver_template_reviews(|r, s| {
            assert!(deliver(r, s)?);
            anyhow::bail!("notification committed before schedule acknowledgement")
        })
        .is_err());
    let reopened = schedules::Store::open(f.root.join("schedules")).unwrap();
    reopened
        .deliver_template_reviews(|r, s| {
            assert!(!deliver(r, s)?);
            Ok(())
        })
        .unwrap();
    tick(4, &mut runtime);
    reopened
        .deliver_template_reviews(|_, _| panic!("duplicate notice"))
        .unwrap();
    let mut t = original.assistant_template.clone().unwrap();
    t.assistant_revision = now.revision;
    t.template_revision = 2;
    reopened
        .update_linked(
            &original.id,
            schedules::UpdateArgs {
                action_id: "renew".into(),
                expected_revision: 1,
                target_session_id: f.session.clone(),
                message: original.message.clone(),
                rule: original.rule.clone(),
                enabled: true,
            },
            Some(t),
            start + Duration::hours(4),
            &Utc,
            |_| Ok(()),
        )
        .unwrap();
    runtime.seat = director(5);
    tick(5, &mut runtime);
    reopened
        .deliver_template_reviews(|r, s| {
            assert!(deliver(r, s)?);
            Ok(())
        })
        .unwrap();
    let records = notices.snapshot().unwrap().notifications;
    assert_eq!(records.len(), 2);
    assert!(records.values().all(|r| r.notification_event.as_deref()
        == Some("system.assistantTemplateNeedsReview")
        && r.notification_level == Some(crate::notification_levels::Level::NeedsYou)));
    assert!(records.values().any(|r| r
        .template_review
        .as_ref()
        .is_some_and(|info| info.template_revision == 2 && info.reason == "director_changed")));
}

#[test]
fn history_pruning_is_bounded_per_template_and_keeps_open_manual_and_latest_receipts() {
    use rusqlite::{params, Connection};
    let (f, schedules, original, start) = setup("retention");
    let sample = f
        .store
        .prepare(
            &f.session,
            "manual",
            &draft(&f, "keep manual"),
            &director(4),
        )
        .unwrap();
    let mut ca = f.store.connection().unwrap();
    let seed_requests = ca.transaction().unwrap();
    let mut body = json!(sample);
    body["occurrence"] = json!({"scheduleId":original.id,"templateRevision":1,"scheduledAt":start,"occurrenceKey":"fixture"});
    body["state"] = json!("completed");
    for i in 0..170 {
        let key = format!("closed-{i}");
        body["id"] = json!(key);
        seed_requests.execute("INSERT INTO pa_requests(id,assistant,project,state,body) VALUES(?1,?2,?3,'completed',?4)",
            params![key,f.assistant.id,f.project,body.to_string()]).unwrap();
    }
    for (key, schedule, state) in [
        ("other", "other-schedule", "completed"),
        ("old-open", original.id.as_str(), "accepted"),
    ] {
        body["id"] = json!(key);
        body["state"] = json!(state);
        body["occurrence"]["scheduleId"] = json!(schedule);
        seed_requests
            .execute(
                "INSERT INTO pa_requests(id,assistant,project,state,body) VALUES(?1,?2,?3,?4,?5)",
                params![key, f.assistant.id, f.project, state, body.to_string()],
            )
            .unwrap();
    }
    seed_requests.commit().unwrap();
    let mut cs = Connection::open(f.root.join("schedules/schedules-v1.sqlite3")).unwrap();
    let seed_events = cs.transaction().unwrap();
    for i in 0..169 {
        seed_events
            .execute(
                "INSERT INTO schedule_template_events(schedule_id,body) VALUES(?1,?2)",
                params![
                    original.id,
                    json!({"verb":"confirm","at":start,"n":i}).to_string()
                ],
            )
            .unwrap();
    }
    seed_events.commit().unwrap();
    let counts = || {
        (ca.query_row("SELECT count(*) FROM pa_requests WHERE id LIKE 'closed-%'",[],|r|r.get::<_,i64>(0)).unwrap(),
        cs.query_row("SELECT count(*) FROM schedule_template_events WHERE json_extract(body,'$.verb')<>'historyPruned'",[],|r|r.get::<_,i64>(0)).unwrap())
    };
    for (removed, remaining) in [(32, 138), (10, 128), (0, 128)] {
        schedules
            .maintain_templates("", start, |a, s| {
                assert_eq!(f.store.prune_closed_schedule(a, s)?, removed);
                Ok(())
            })
            .unwrap();
        assert_eq!(counts(), (remaining, remaining));
    }
    for key in [&sample.id, "old-open", "other", "closed-169"] {
        f.store.request(key).unwrap();
    }
    assert!(f.store.request("closed-0").is_err());
    let audit = f.store.audit(&f.assistant.id, i64::MAX, 20).unwrap();
    assert_eq!(
        audit
            .iter()
            .filter(|r| r["verb"] == "pruneScheduledRequests")
            .count(),
        2
    );
    let events = schedules
        .template_history(&f.assistant.id, &original.id, i64::MAX, 10)
        .unwrap();
    assert_eq!(events[0]["verb"], "historyPruned");
    assert_eq!(events[0]["deletedTotal"], 42);
}

#[test]
fn restart_and_cancel_after_admission_recover_the_same_occurrence_receipt() {
    let (f, schedules, schedule, start) = setup("lost-occurrence-receipt");
    let resumed = start + Duration::hours(4);
    let run = schedules.claim(resumed, &Utc, 1).unwrap().pop().unwrap();
    assert_eq!(run.scheduled_at, start + Duration::hours(1));
    assert_eq!(run.coalesced_until, Some(resumed));
    let receipt =
        recurring::admit_run(&f.store, &schedules, &run, Ok(()), |_| Ok(director(4))).unwrap();
    let request = f.store.request(&receipt).unwrap();
    assert_eq!(request.state, RequestState::Queued);
    assert_eq!(request.occurrence.unwrap().occurrence_key, run.id);
    drop(schedules);
    let schedules = schedules::Store::open(f.root.join("schedules")).unwrap();
    assert_eq!(schedules.recover_dispatching(resumed).unwrap(), 1);
    schedules
        .stop(
            &schedule.id,
            DeleteArgs {
                action_id: "cancel".into(),
                expected_revision: 1,
            },
            resumed,
            Some(&f.assistant.id),
            true,
        )
        .unwrap();
    let reopened = Store::open(&f.root).unwrap();
    let mut runtime = InboxRuntime {
        pa: &reopened,
        seat: director(5),
        stopped: true,
    };
    assert_eq!(
        schedules::runtime::tick(&schedules, resumed, &Utc, &mut runtime).unwrap(),
        1
    );
    assert!(!schedules
        .finish(&run, "refused", Some("old lease".into()), None, resumed)
        .unwrap());
    let runs = schedules.runs(&schedule.id, None, 10).unwrap().data;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "handedOff");
    assert_eq!(runs[0].request_id.as_ref(), Some(&receipt));
    assert!(runs[0].message_id.is_none());
    assert_eq!(
        reopened
            .requests(Some(&f.assistant.id), None, 0, 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM pa_notices", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        schedules
            .template_history(&f.assistant.id, &schedule.id, i64::MAX, 10)
            .unwrap()
            .len(),
        2
    );
    assert!(schedules
        .claim(resumed + Duration::days(5), &Utc, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn stopped_target_holds_then_seat_change_and_revocation_refuse_without_admission() {
    let (f, schedules, schedule, start) = setup("recurring-revoke");
    let due = start + Duration::hours(1);
    let mut runtime = InboxRuntime {
        pa: &f.store,
        seat: director(4),
        stopped: true,
    };
    schedules::runtime::tick(&schedules, due, &Utc, &mut runtime).unwrap();
    let run = schedules.get(&schedule.id).unwrap().last_run.unwrap();
    assert_eq!(run.outcome, "held");
    runtime.stopped = false;
    runtime.seat = director(5);
    schedules::runtime::tick(&schedules, due + Duration::hours(2), &Utc, &mut runtime).unwrap();
    let refused = schedules.get(&schedule.id).unwrap().last_run.unwrap();
    assert_eq!(refused.id, run.id);
    assert_eq!(refused.outcome, "refused");
    assert_eq!(refused.last_error.as_deref(), Some("director_changed"));
    assert!(refused.coalesced_until.is_some());
    runtime.seat = director(4);
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
    schedules::runtime::tick(&schedules, due + Duration::hours(3), &Utc, &mut runtime).unwrap();
    let revoked = schedules.get(&schedule.id).unwrap().last_run.unwrap();
    assert_ne!(revoked.id, run.id);
    assert_eq!(revoked.outcome, "refused");
    assert_eq!(revoked.last_error.as_deref(), Some("assistant_disabled"));
    assert!(f
        .store
        .requests(Some(&f.assistant.id), None, 0, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn ordinary_schedule_edits_cannot_change_a_confirmed_template() {
    let (f, store, schedule, start) = setup("template-edit");
    let edit = schedules::UpdateArgs {
        action_id: "ordinary-edit".into(),
        expected_revision: 1,
        target_session_id: f.session,
        message: "Edited reminder".into(),
        rule: Rule::Interval { hours: 2 },
        enabled: true,
    };
    let err = store
        .update(&schedule.id, edit, start, &Utc, |_| Ok(()))
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<schedules::Error>(),
        Some(schedules::Error::Conflict(_))
    ));
    assert_eq!(store.get(&schedule.id).unwrap().revision, 1);
    assert_eq!(
        store
            .template_history(&f.assistant.id, &schedule.id, i64::MAX, 5)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn cancelling_or_pausing_a_leased_template_prevents_a_new_request() {
    for cancel in [true, false] {
        let (f, schedules, schedule, start) = setup("cancel-leased");
        let due = start + Duration::hours(1);
        let run = schedules.claim(due, &Utc, 1).unwrap().pop().unwrap();
        schedules
            .stop(
                &schedule.id,
                DeleteArgs {
                    action_id: "stop".into(),
                    expected_revision: 1,
                },
                due,
                Some(&f.assistant.id),
                cancel,
            )
            .unwrap();
        assert!(
            recurring::admit_run(&f.store, &schedules, &run, Ok(()), |_| panic!(
                "superseded template checked the seat"
            ))
            .unwrap_err()
            .is::<schedules::runtime::CancelledTemplate>()
        );
        schedules.recover_dispatching(due).unwrap();
        let mut runtime = InboxRuntime {
            pa: &f.store,
            seat: director(4),
            stopped: false,
        };
        schedules::runtime::tick(&schedules, due, &Utc, &mut runtime).unwrap();
        assert_eq!(
            schedules.runs(&schedule.id, None, 10).unwrap().data[0].outcome,
            "cancelled"
        );
        assert!(f
            .store
            .requests(Some(&f.assistant.id), None, 0, 10)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn successive_occurrences_enqueue_in_order_and_the_director_can_decline() {
    let (f, schedules, schedule, start) = setup("successive-slots");
    let mut runtime = InboxRuntime {
        pa: &f.store,
        seat: director(4),
        stopped: false,
    };
    for hour in 1..=2 {
        let due = start + Duration::hours(hour);
        assert_eq!(
            schedules::runtime::tick(&schedules, due, &Utc, &mut runtime).unwrap(),
            1
        );
        assert_eq!(
            schedules::runtime::tick(&schedules, due, &Utc, &mut runtime).unwrap(),
            0
        );
    }
    let items = f
        .store
        .requests(None, director(4).session_id.as_deref(), 0, 10)
        .unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0].0 < items[1].0);
    assert_ne!(items[0].1.id, items[1].1.id);
    let first = &items[0].1;
    assert_eq!(first.occurrence.as_ref().unwrap().schedule_id, schedule.id);
    let decline = f
        .store
        .respond(
            director(4).session_id.as_deref().unwrap(),
            &first.id,
            "decline",
            first.revision,
            Response::Decline,
            "No status change",
            &director(4),
        )
        .unwrap();
    assert_eq!(decline.state, RequestState::Declined);
}

#[test]
fn legacy_local_scope_gets_qualified_without_rewriting_historical_creation_receipts() {
    use rusqlite::{params, Connection};
    let f = isolated("old-qualified");
    let mut original = serde_json::to_value(&f.assistant).unwrap();
    original
        .as_object_mut()
        .unwrap()
        .remove("coordinationProjectRef");
    original["config"]
        .as_object_mut()
        .unwrap()
        .remove("projectRefs");
    let root = std::env::temp_dir().join(id("genuine-v3"));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let old = Connection::open(root.join(store::DB)).unwrap();
    old.execute_batch(include_str!("../schema.sql")).unwrap();
    old.execute_batch(include_str!("../schema-v2.sql")).unwrap();
    old.execute_batch(include_str!("../schema-v3.sql")).unwrap();
    old.execute(
        "INSERT INTO pa_assistants(id,session,body) VALUES(?1,?2,?3)",
        params![f.assistant.id, f.session, original.to_string()],
    )
    .unwrap();
    let request = f
        .store
        .prepare(
            &f.session,
            "old-draft",
            &draft(&f, "Old request"),
            &director(1),
        )
        .unwrap();
    let mut old_request = json!(request);
    old_request.as_object_mut().unwrap().remove("projectRef");
    old.execute("INSERT INTO pa_requests(id,assistant,project,state,body) VALUES(?1,?2,?3,'awaitingOwner',?4)",
        params![request.id,f.assistant.id,f.project,old_request.to_string()]).unwrap();
    let prepare_digest =
        crate::approvals::request_digest(&json!(["prepare", draft(&f, "Old request")]));
    old.execute(
        "INSERT INTO pa_actions(actor,action,digest,reply) VALUES(?1,'old-draft',?2,?3)",
        params![f.session, prepare_digest, old_request.to_string()],
    )
    .unwrap();
    let digest = crate::approvals::request_digest(&json!(["create", original["config"]]));
    old.execute(
        "INSERT INTO pa_actions(actor,action,digest,reply) VALUES('owner','legacy',?1,?2)",
        params![digest, original.to_string()],
    )
    .unwrap();
    drop(old);
    let reopened = Store::open(&root).unwrap();
    let reference = f.assistant.config.project_ref(&f.project);
    assert_eq!(
        reopened.request(&request.id).unwrap().project_ref,
        Some(reference.clone())
    );
    assert_eq!(
        json!(reopened.get(&f.assistant.id).unwrap())["config"]["projectRefs"],
        json!([reference])
    );
    let replay = reopened
        .begin("owner", "legacy", f.assistant.config.clone(), |_| {
            panic!("replayed creation")
        })
        .unwrap();
    assert_eq!(replay.id, f.assistant.id);
    assert_eq!(
        replay.coordination_project_ref,
        Some(
            f.assistant
                .config
                .project_ref(&f.assistant.coordination_project_id)
        )
    );
    let replay = reopened
        .prepare(
            &f.session,
            "old-draft",
            &draft(&f, "Old request"),
            &director(44),
        )
        .unwrap();
    assert_eq!(replay.id, request.id);
    assert_eq!(replay.project_ref, Some(reference));
    let mut cfg = json!(f.assistant.config);
    cfg["projectRefs"][0]["instanceId"] = json!("other-host");
    assert!(serde_json::from_value::<Config>(cfg).is_err());
}

#[test]
fn agenda_pages_due_requests_and_overlapping_calendar_events_and_detects_replacement() {
    let f = isolated("merged-agenda");
    let midnight = Local
        .with_ymd_and_hms(2026, 11, 18, 0, 0, 0)
        .earliest()
        .unwrap()
        .to_utc();
    let mut task = draft(&f, "Due tomorrow");
    task.due_at = Some(midnight + Duration::days(1));
    f.store
        .prepare(&f.session, "tomorrow", &task, &director(2))
        .unwrap();
    task.due_at = Some(midnight + Duration::hours(12));
    let due = f
        .store
        .prepare(&f.session, "today", &task, &director(2))
        .unwrap();
    let args = json!({"date":"2026-11-18","limit":1});
    let read = |args: &serde_json::Value| {
        tools::call(&f.store, &f.session, "pa_agenda_read", args).unwrap()
    };
    let empty = read(&args);
    assert_eq!(empty["calendar"]["status"], "disconnected");
    assert_eq!(
        empty["projects"][0]["projectRef"],
        json!(f.assistant.config.project_ref(&f.project))
    );
    assert_eq!(empty["dueRequests"][0]["id"], due.id);
    let events =
        [("overnight", -1, 1), ("morning", 1, 2), ("later", 25, 26)].map(|(id, from, to)| {
            calendar::Event {
                id: id.into(),
                title: id.into(),
                starts_at: midnight + Duration::hours(from),
                ends_at: midnight + Duration::hours(to),
            }
        });
    let snap = calendar::Snapshot {
        source: "Owner calendar".into(),
        observed_at: midnight,
        events: events.into(),
    };
    f.store
        .replace_calendar(&f.assistant.id, "import", 0, Some(&snap))
        .unwrap();
    let first = read(&args);
    assert_eq!(first["calendar"]["items"][0]["id"], "overnight");
    let mut next = args.clone();
    next["calendarCursor"] = first["calendar"]["nextCursor"].clone();
    assert_eq!(read(&next)["calendar"]["items"][0]["id"], "morning");
    let other = f
        .store
        .begin(
            "owner",
            "other-calendar",
            f.assistant.config.clone(),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        f.store.calendar_info(&other.id).unwrap()["status"],
        "disconnected"
    );
    let empty = calendar::Snapshot {
        events: Vec::new(),
        ..snap
    };
    f.store
        .replace_calendar(&f.assistant.id, "replace", 1, Some(&empty))
        .unwrap();
    assert_eq!(read(&args)["calendar"]["status"], "ready");
    assert_eq!(read(&args)["calendar"]["items"], json!([]));
    must_fail(
        tools::call(&f.store, &f.session, "pa_agenda_read", &next),
        "calendar_changed",
    );
    f.store
        .replace_calendar(&f.assistant.id, "unlink", 2, None)
        .unwrap();
    assert_eq!(read(&args)["calendar"]["status"], "disconnected");
}
