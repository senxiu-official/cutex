use super::*;
use rusqlite::{params, Connection};
use std::sync::Barrier;

fn race<T: Send>(work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let start = Barrier::new(2);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|i| {
                let (start, work) = (&start, &work);
                scope.spawn(move || {
                    start.wait();
                    work(i)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    })
}

#[test]
fn concurrent_bind_and_confirm_have_one_winner_and_one_delivery() {
    let f = isolated("bind-confirm");
    let row = f
        .store
        .begin("owner", "new", f.assistant.config.clone(), |_| Ok(()))
        .unwrap();
    let sessions = [id("session"), id("session")];
    let outcomes = race(|i| f.store.bind(&row.id, &sessions[i]));
    let winner = outcomes.iter().position(Result::is_ok).unwrap();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes[1 - winner]
            .as_ref()
            .unwrap_err()
            .downcast_ref::<Fault>()
            .unwrap()
            .0,
        "action_conflict"
    );
    for result in race(|_| f.store.bind(&row.id, &sessions[winner])) {
        assert_eq!(
            result.unwrap().session_id.as_deref(),
            Some(sessions[winner].as_str())
        );
    }
    let unbound = ["left", "right"].map(|action| {
        f.store
            .begin("owner", action, f.assistant.config.clone(), |_| Ok(()))
            .unwrap()
    });
    let common = id("session");
    let competing = race(|i| f.store.bind(&unbound[i].id, &common));
    assert_eq!(competing.iter().filter(|r| r.is_ok()).count(), 1);
    for error in competing.into_iter().filter_map(Result::err) {
        assert_eq!(error_status(&error), (409, "binding_conflict"));
    }
    let seat = director(1);
    for same_action in [false, true] {
        let r = f
            .store
            .prepare(
                &f.session,
                &format!("prepare-{same_action}"),
                &draft(&f, "work"),
                &seat,
            )
            .unwrap();
        let confirmed = race(|i| {
            let action = format!("confirm-{same_action}-{}", if same_action { 0 } else { i });
            f.store.confirm(
                "owner",
                &f.assistant.id,
                &r.id,
                &action,
                r.revision,
                f.assistant.revision,
                &seat,
                &seat,
            )
        });
        assert_eq!(
            confirmed.iter().filter(|r| r.is_ok()).count(),
            if same_action { 2 } else { 1 }
        );
        for outcome in confirmed {
            match outcome {
                Ok(r) => assert_eq!((r.state, r.revision), (RequestState::Queued, 2)),
                Err(e) => assert_eq!(e.downcast_ref::<Fault>().unwrap().0, "revision_conflict"),
            }
        }
    }
    let inbox = f
        .store
        .requests(None, seat.session_id.as_deref(), 0, 10)
        .unwrap();
    assert_eq!(inbox.iter().map(|r| r.0).collect::<Vec<_>>(), [1, 2]);
    let mut delivered = Vec::new();
    f.store
        .deliver_notices(|id, _, _| {
            delivered.push(id.to_owned());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        delivered,
        inbox
            .iter()
            .map(|r| format!("{}-2", r.1.id))
            .collect::<Vec<_>>()
    );
}

#[test]
fn memory_limit_is_atomic_and_existing_keys_remain_editable() {
    let f = isolated("memory-cap");
    f.store
        .connection()
        .unwrap()
        .execute(
            "WITH RECURSIVE keys(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM keys WHERE n < ?2)
         INSERT INTO pa_memory(assistant,key,revision,content,source)
         SELECT ?1,'note-'||n,1,'old','test' FROM keys",
            params![f.assistant.id, inbox::MEMORY_KEYS - 1],
        )
        .unwrap();
    let write = |i| {
        f.store.memory_write(
            &f.session,
            &format!("last-{i}"),
            &format!("new-{i}"),
            0,
            "value",
            "test",
        )
    };
    let results = race(write);
    let winner = results.iter().position(Result::is_ok).unwrap();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        error_status(results[1 - winner].as_ref().unwrap_err()),
        (409, "memory_full")
    );
    assert_eq!(write(winner).unwrap()["revision"], 1);
    assert_eq!(
        f.store
            .memory_write(&f.session, "update", "note-1", 1, "new", "test")
            .unwrap()["revision"],
        2
    );
    let other = f
        .store
        .begin("owner", "second", f.assistant.config.clone(), |_| Ok(()))
        .unwrap();
    let other = f.store.bind(&other.id, "another-session").unwrap();
    f.store
        .memory_write(
            other.session_id.as_deref().unwrap(),
            "other",
            "note-1",
            0,
            "value",
            "test",
        )
        .unwrap();
}

#[test]
fn waiting_and_clarification_share_slots_and_existing_work_can_leave() {
    let f = isolated("open-cap");
    let s = &f.store;
    let seat = director(1);
    let who = seat.session_id.as_deref().unwrap();
    let queued = s
        .prepare(&f.session, "first", &draft(&f, "queued"), &seat)
        .unwrap();
    let queued = s
        .confirm(
            "owner",
            &f.assistant.id,
            &queued.id,
            "admit",
            1,
            2,
            &seat,
            &seat,
        )
        .unwrap();
    let saved = s
        .prepare(&f.session, "saved", &draft(&f, "saved"), &seat)
        .unwrap();
    s.connection()
        .unwrap()
        .execute(
            "WITH RECURSIVE copies(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM copies WHERE n < ?2)
         INSERT INTO pa_requests(id,assistant,director,project,state,body)
         SELECT 'open-'||n,assistant,director,project,state,json_set(body,'$.id','open-'||n)
         FROM pa_requests,copies WHERE id=?1",
            params![saved.id, inbox::OPEN_REQUESTS - 2],
        )
        .unwrap();
    let add = |i| s.prepare(&f.session, &format!("last-{i}"), &draft(&f, "last"), &seat);
    let outcomes = race(add);
    let winner = outcomes.iter().position(Result::is_ok).unwrap();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        error_status(outcomes[1 - winner].as_ref().unwrap_err()),
        (409, "open_request_limit")
    );
    assert_eq!(
        add(winner).unwrap().id,
        outcomes[winner].as_ref().unwrap().id
    );
    let revised = s
        .revise(&f.session, &saved.id, "edit", 1, &draft(&f, "edit"), &seat)
        .unwrap();
    must_fail(
        s.respond(
            who,
            &queued.id,
            "clarify",
            2,
            Response::Clarify,
            "details?",
            &seat,
        ),
        "open_request_limit",
    );
    must_fail(
        s.revise(
            &f.session,
            &queued.id,
            "retarget",
            2,
            &draft(&f, "retarget"),
            &director(2),
        ),
        "open_request_limit",
    );
    s.cancel(
        "owner",
        &f.assistant.id,
        &revised.id,
        "close",
        revised.revision,
    )
    .unwrap();
    let clarification = s
        .respond(
            who,
            &queued.id,
            "clarify",
            2,
            Response::Clarify,
            "details?",
            &seat,
        )
        .unwrap();
    must_fail(
        s.prepare(&f.session, "another", &draft(&f, "new"), &seat),
        "open_request_limit",
    );
    let edited = s
        .revise(
            &f.session,
            &queued.id,
            "answer",
            clarification.revision,
            &draft(&f, "answer"),
            &seat,
        )
        .unwrap();
    s.confirm(
        "owner",
        &f.assistant.id,
        &edited.id,
        "approve",
        edited.revision,
        2,
        &seat,
        &seat,
    )
    .unwrap();
    s.prepare(&f.session, "another", &draft(&f, "new"), &seat)
        .unwrap();
    let other = s
        .begin("owner", "second", f.assistant.config.clone(), |_| Ok(()))
        .unwrap();
    s.bind(&other.id, "other-session").unwrap();
    s.prepare("other-session", "another", &draft(&f, "other"), &seat)
        .unwrap();
}

#[test]
fn legacy_history_bindings_and_cancellations_reach_the_last_key_page() {
    let mut row = isolated("paged-template").assistant;
    let root = std::env::temp_dir().join(id("paged-legacy"));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let mut legacy = Connection::open(root.join(store::DB)).unwrap();
    legacy.execute_batch(include_str!("../schema.sql")).unwrap();
    let tx = legacy.transaction().unwrap();
    let mut expected = Vec::new();
    row.enabled = false;
    for n in 0..105 {
        let session = format!("cutex.00000000-0000-4000-8000-{n:012}");
        row.id = format!("pa-{n:032x}");
        row.session_id = Some(session.clone());
        tx.execute(
            "INSERT INTO pa_assistants(id,session,body) VALUES(?1,?2,?3)",
            params![row.id, session, serde_json::to_string(&row).unwrap()],
        )
        .unwrap();
        expected.push(session);
    }
    tx.execute(
        "WITH RECURSIVE changes(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM changes WHERE n<?2)
         INSERT INTO pa_audit(assistant,actor,action,verb,at,detail)
         SELECT ?1,'owner','edit-'||n,'configure','then','{}' FROM changes",
        params![row.id, store::HISTORY_LIMIT + 1],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO pa_actions SELECT actor,action,'fixture','{}' FROM pa_audit",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(legacy);

    let s = Store::open(&root).unwrap();
    let mut bound = Vec::new();
    Store::visit_bound_sessions(&root, |session| bound.push(session)).unwrap();
    assert_eq!(bound, expected);
    let conn = s.connection().unwrap();
    let count = |sql| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT count(*) FROM pa_audit"), store::HISTORY_LIMIT);
    assert_eq!(
        count("SELECT count(*) FROM pa_actions"),
        store::HISTORY_LIMIT
    );
    assert_eq!(
        count("SELECT count(*) FROM pa_reminder_cancellations"),
        expected.len() as i64
    );
    s.retry_reminder_cancellations().unwrap();
    assert_eq!(count("SELECT count(*) FROM pa_reminder_cancellations"), 0);
}

#[test]
fn v1_upgrade_and_live_pruning_preserve_creation_replays_and_bound_history() {
    use crate::personal_assistants::store::{DB, HISTORY_LIMIT};
    let f = isolated("source");
    let seat = director(7);
    let initial = f
        .store
        .prepare(&f.session, "draft-1", &draft(&f, "one"), &seat)
        .unwrap();
    f.store
        .cancel("owner", &f.assistant.id, &initial.id, "cancel-1", 1)
        .unwrap();
    f.store
        .memory_write(&f.session, "note", "key", 0, "original", "source")
        .unwrap();
    let later = f
        .store
        .prepare(&f.session, "draft-2", &draft(&f, "two"), &seat)
        .unwrap();
    f.store
        .cancel("owner", &f.assistant.id, &later.id, "cancel-2", 1)
        .unwrap();

    let root = std::env::temp_dir().join(id("legacy"));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let legacy = Connection::open(root.join(DB)).unwrap();
    legacy.execute_batch(include_str!("../schema.sql")).unwrap();
    legacy
        .execute(
            "ATTACH DATABASE ?1 AS src",
            [f.root.join(DB).to_str().unwrap()],
        )
        .unwrap();
    legacy.execute_batch(
        "INSERT INTO pa_assistants SELECT * FROM src.pa_assistants;
         INSERT INTO pa_requests SELECT seq,id,assistant,director,project,state,body,admission_seq FROM src.pa_requests;
         INSERT INTO pa_actions SELECT * FROM src.pa_actions;
         INSERT INTO pa_audit SELECT seq,assistant,actor,action,verb,at,detail FROM src.pa_audit;
         INSERT INTO pa_memory SELECT * FROM src.pa_memory;
         DETACH DATABASE src;",
    ).unwrap();
    legacy.execute(
        "WITH RECURSIVE changes(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM changes WHERE n < ?2)
         INSERT INTO pa_audit(assistant,actor,action,verb,at,detail)
         SELECT ?1,'fixture','edit-'||n,'memoryWrite','then','{}' FROM changes",
        params![f.assistant.id,HISTORY_LIMIT-1],
    ).unwrap();
    legacy
        .execute(
            "INSERT INTO pa_actions(actor,action,digest,reply)
         SELECT actor,action,'fixture','{}' FROM pa_audit WHERE actor='fixture'",
            [],
        )
        .unwrap();
    drop(legacy);

    let s = Store::open(&root).unwrap();
    let conn = s.connection().unwrap();
    let counts = || {
        conn.query_row(
            "SELECT (SELECT count(*) FROM pa_audit),(SELECT count(*) FROM pa_actions),
            (SELECT count(*) FROM pa_requests WHERE prepare_action IS NOT NULL)",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap()
    };
    let total = HISTORY_LIMIT + 4; // Four owner/system audit entries have their own budget.
    assert_eq!(counts(), (total, total, 1));
    s.memory_write(&f.session, "new-note", "key", 1, "updated", "source")
        .unwrap();
    assert_eq!(counts(), (total, total, 2));
    let compact: i64 = conn
        .query_row(
            "SELECT count(*) FROM pa_requests WHERE prepare_action IS NOT NULL
             AND length(CAST(prepare_origin AS BLOB))<=2048
             AND json_type(prepare_origin,'$.outcome') IS NULL
             AND json_type(prepare_origin,'$.constraints') IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(compact, 2);
    for (action, text, r) in [("draft-1", "one", initial), ("draft-2", "two", later)] {
        let replay = s
            .prepare(&f.session, action, &draft(&f, text), &director(99))
            .unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(&r).unwrap()
        );
        assert_eq!(s.request(&r.id).unwrap().state, RequestState::Cancelled);
        must_fail(
            s.prepare(&f.session, action, &draft(&f, "different"), &seat),
            "action_conflict",
        );
    }
    must_fail(
        s.memory_write(&f.session, "note", "key", 0, "original", "source"),
        "revision_conflict",
    );
    let create = s
        .begin("owner", "create", f.assistant.config.clone(), |_| {
            panic!("creation reran")
        })
        .unwrap();
    assert_eq!(
        (create.id.as_str(), create.revision),
        (f.assistant.id.as_str(), 1)
    );
    assert_eq!(s.bind(&create.id, &f.session).unwrap().revision, 2);
    assert_eq!(counts(), (total, total, 2));
}

#[test]
fn v2_owner_grant_audit_survives_six_hundred_memory_writes() {
    let f = isolated("grant-source");
    let granted = Update::Grants {
        grants: Grants {
            operations: [Operation::Publishing].into(),
            ..Default::default()
        },
    };
    let result = f
        .store
        .configure(
            "owner",
            &f.assistant.id,
            "grant-publishing",
            2,
            &granted,
            |_| Ok(()),
        )
        .unwrap();
    let root = std::env::temp_dir().join(id("grant-v2"));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let legacy = Connection::open(root.join(store::DB)).unwrap();
    legacy.execute_batch(include_str!("../schema.sql")).unwrap();
    legacy
        .execute_batch(include_str!("../schema-v2.sql"))
        .unwrap();
    legacy
        .execute(
            "ATTACH DATABASE ?1 AS src",
            [f.root.join(store::DB).to_str().unwrap()],
        )
        .unwrap();
    legacy
        .execute_batch(
            "INSERT INTO pa_assistants SELECT * FROM src.pa_assistants;
         INSERT INTO pa_actions SELECT * FROM src.pa_actions;
         INSERT INTO pa_audit SELECT seq,assistant,actor,action,verb,at,detail FROM src.pa_audit;
         DETACH DATABASE src;",
        )
        .unwrap();
    drop(legacy);
    let s = Store::open(&root).unwrap();
    for revision in 0..600 {
        s.memory_write(
            &f.session,
            &format!("edit-{revision}"),
            "same-key",
            revision,
            "updated",
            "fixture",
        )
        .unwrap();
    }
    let s = Store::open(&root).unwrap();
    let mut before = i64::MAX;
    let mut audit = Vec::new();
    loop {
        let page = s.audit(&f.assistant.id, before, 100).unwrap();
        let Some(last) = page.last() else { break };
        before = last["seq"].as_i64().unwrap();
        audit.extend(page);
    }
    assert_eq!(
        audit
            .iter()
            .filter(|row| row["verb"] == "memoryWrite")
            .count(),
        store::HISTORY_LIMIT as usize
    );
    let grant = audit
        .iter()
        .find(|row| row["actionId"] == "grant-publishing")
        .unwrap();
    assert_eq!(grant["actor"], "owner");
    assert_eq!(grant["detail"]["before"]["grants"]["operations"], json!([]));
    assert_eq!(
        grant["detail"]["after"]["grants"]["operations"],
        json!(["publishing"])
    );
    assert_eq!(
        s.configure(
            "owner",
            &f.assistant.id,
            "grant-publishing",
            2,
            &granted,
            |_| panic!("grant replay reran validation")
        )
        .unwrap(),
        result
    );
    assert_eq!(audit.len(), store::HISTORY_LIMIT as usize + 3);
}

#[test]
fn owner_churn_has_a_bounded_window_and_preserves_assistant_receipts() {
    let f = isolated("owner-churn");
    let s = &f.store;
    let note = || {
        s.memory_write(
            &f.session,
            "single-edit",
            "same-key",
            0,
            "updated",
            "fixture",
        )
    };
    let receipt = note().unwrap();
    let mut rev = f.assistant.revision;
    for n in 0..store::HISTORY_LIMIT {
        rev = s
            .configure(
                "owner",
                &f.assistant.id,
                &format!("settings-{n}"),
                rev,
                &Update::Settings {
                    name: format!("Fixture {n}"),
                    project_ids: f.assistant.config.project_ids.clone(),
                },
                |_| Ok(()),
            )
            .unwrap()
            .revision;
    }
    let conn = s.connection().unwrap();
    for (control, expected) in [(false, 1), (true, store::HISTORY_LIMIT)] {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM pa_audit WHERE assistant=?1 AND owner_action=?2",
                params![f.assistant.id, control],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, expected);
    }
    let all: i64 = conn
        .query_row("SELECT count(*) FROM pa_actions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(all, store::HISTORY_LIMIT + 3);
    assert_eq!(note().unwrap(), receipt);
}

#[test]
fn deferred_notices_leave_the_next_page_deliverable_and_keep_their_ids() {
    let f = isolated("notices");
    let conn = f.store.connection().unwrap();
    for n in 0..34 {
        conn.execute(
            "INSERT INTO pa_notices(id,recipient,message) VALUES(?1,?2,'update')",
            params![
                format!("notice-{n}"),
                if n == 1 || n == 33 {
                    "online"
                } else {
                    "unavailable"
                }
            ],
        )
        .unwrap();
    }
    let mut delivered = Vec::new();
    let mut send = |id: &str, to: &str, _: &str| {
        anyhow::ensure!(to == "online", "fixture recipient unavailable");
        delivered.push(id.to_owned());
        Ok(())
    };
    f.store.deliver_notices_at(100, &mut send).unwrap();
    f.store.deliver_notices_at(100, &mut send).unwrap();
    assert_eq!(delivered, ["notice-1", "notice-33"]);
    let pending: i64 = conn
        .query_row(
            "SELECT count(*) FROM pa_notices WHERE attempts=1 AND retry_at>0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 32);
    conn.execute("UPDATE pa_notices SET retry_at=0 WHERE id='notice-0'", [])
        .unwrap();
    let reopened = Store::open(&f.root).unwrap();
    reopened
        .deliver_notices_at(100, |id, _, _| {
            assert_eq!(id, "notice-0");
            Ok(())
        })
        .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM pa_notices WHERE id='notice-0'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn failed_notice_is_removed_only_after_eight_attempts_and_confirmed_recipient_absence() {
    use crate::config::instance::{scoped_test_selection, Selection};
    use crate::session::model::{CutexSessionRecord, CutexSessionStore};
    let _home = crate::config::paths::test_home_env_lock();
    let _selection = scoped_test_selection(Selection::Custom(
        std::env::temp_dir().join(id("notice-catalog")),
    ));
    let f = isolated("notice-retry");
    let offline = format!("cutex.{}", uuid::Uuid::new_v4());
    let absent = format!("cutex.{}", uuid::Uuid::new_v4());
    assert!(!host::notice_recipient_gone(
        &absent,
        host::MISSING_NOTICE_ATTEMPTS
    ));
    let mut catalog = CutexSessionStore::default();
    catalog.sessions.insert(
        offline.clone(),
        CutexSessionRecord::new(offline.clone(), None, "local".into(), ".".into(), None).unwrap(),
    );
    crate::session::store::save_cutex_session_store(&catalog).unwrap();
    let conn = f.store.connection().unwrap();
    for (notice, recipient) in [("missing", &absent), ("offline", &offline)] {
        conn.execute(
            "INSERT INTO pa_notices(id,recipient,message) VALUES(?1,?2,'fixture')",
            params![notice, recipient],
        )
        .unwrap();
    }
    let pending = || {
        conn.query_row("SELECT count(*) FROM pa_notices", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let limit = i64::try_from(host::MISSING_NOTICE_ATTEMPTS).unwrap();
    for n in 1..limit {
        f.store
            .deliver_notices_at(4000 * n, |_, _, _| {
                anyhow::bail!("fixture delivery deferred")
            })
            .unwrap();
        assert_eq!(pending(), 2);
    }
    Store::open(&f.root)
        .unwrap()
        .deliver_notices_at(4000 * limit, |_, _, _| {
            anyhow::bail!("fixture delivery deferred")
        })
        .unwrap();
    assert_eq!(pending(), 1);
    f.store
        .deliver_notices_at(4000 * (limit + 1), |id, to, _| {
            assert_eq!((id, to), ("offline", offline.as_str()));
            Ok(())
        })
        .unwrap();
    assert_eq!(pending(), 0);
}

#[test]
fn disabling_cleans_self_reminders_after_an_interruption_and_before_reenable() {
    use crate::agent_bus::alarms;
    use crate::config::instance::{scoped_test_selection, Selection};
    let _home = crate::config::paths::test_home_env_lock();
    let _selection = scoped_test_selection(Selection::Custom(
        std::env::temp_dir().join(id("alarm-home")),
    ));
    let f = isolated("reminder-cleanup");
    let other = format!("cutex.{}", uuid::Uuid::new_v4());
    let at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let remind = |owner: &str, to: &str, name: &str| {
        alarms::set(
            owner,
            alarms::SetArgs {
                alarm_id: name.into(),
                to: to.into(),
                message: "fixture".into(),
                at: Some(at.clone()),
                delay_seconds: None,
            },
        )
        .unwrap();
    };
    let path = alarms::store_path().unwrap();
    let state = |name: &str| {
        alarms::read_all_at(&path)
            .unwrap()
            .into_iter()
            .find(|row| row.alarm_id == name)
            .unwrap()
            .state
    };
    let change = |action, rev, enabled| {
        f.store.configure(
            "owner",
            &f.assistant.id,
            action,
            rev,
            &Update::Enabled { enabled },
            |_| Ok(()),
        )
    };
    remind(&f.session, &f.session, "owned");
    remind(&other, &f.session, "incoming");
    remind(&f.session, &other, "outgoing");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    assert!(!change("disable", 2, false).unwrap().enabled);
    assert_eq!(state("owned"), "scheduled");
    assert!(change("enable", 3, true).is_err());
    assert!(!f.store.get(&f.assistant.id).unwrap().enabled);
    fs2::FileExt::unlock(&lock).unwrap();
    Store::open(&f.root)
        .unwrap()
        .retry_reminder_cancellations()
        .unwrap();
    assert_eq!(state("owned"), "cancelled");
    assert_eq!(state("incoming"), "scheduled");
    assert_eq!(state("outgoing"), "scheduled");
    assert!(change("enable", 3, true).unwrap().enabled);
    remind(&f.session, &f.session, "after-enable");
    change("disable", 2, false).unwrap();
    assert_eq!(state("after-enable"), "scheduled");
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    change("second-disable", 4, false).unwrap();
    fs2::FileExt::unlock(&lock).unwrap();
    change("second-enable", 5, true).unwrap();
    assert_eq!(state("after-enable"), "cancelled");
}
