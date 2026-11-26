use super::{
    runtime::{self, Paused, Runtime},
    *,
};
use chrono::{DateTime, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::json;

const TARGET: &str = "cutex.00000000-0000-0000-0000-000000000001";
const OTHER: &str = "cutex.00000000-0000-0000-0000-000000000002";
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("schedule-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}
fn args(action: &str) -> CreateArgs {
    CreateArgs {
        action_id: action.into(),
        target_session_id: TARGET.into(),
        message: "weekly health report".into(),
        rule: Rule::Interval { hours: 1 },
        enabled: true,
    }
}
fn create(store: &Store, action: &str, now: DateTime<Utc>) -> Schedule {
    store.create(args(action), now, &Utc, |_| Ok(())).unwrap()
}

#[test]
fn rules_use_host_wall_clock_and_utc_elapsed_hours() {
    let zone = FixedOffset::east_opt(11 * 3600).unwrap();
    let now = at("2026-11-15T22:59:00Z"); // Monday09:59 Sydney offset
    assert_eq!(
        Rule::Weekly {
            weekday: 1,
            time: "10:00".into()
        }
        .next_after(&zone, now, now)
        .unwrap(),
        at("2026-11-15T23:00:00Z")
    );
    let leap = at("2028-02-28T23:30:00Z");
    assert_eq!(
        Rule::Daily {
            time: "00:15".into()
        }
        .next_after(&Utc, leap, leap)
        .unwrap(),
        at("2028-02-29T00:15:00Z")
    );
    let anchor = at("2026-01-01T00:00:00Z");
    assert_eq!(
        Rule::Interval { hours: 7 }
            .next_after(&zone, anchor, anchor - chrono::Duration::hours(1))
            .unwrap(),
        anchor + chrono::Duration::hours(7)
    );
    assert_eq!(
        Rule::Interval { hours: 7 }
            .next_after(&zone, anchor, at("2026-01-02T02:00:00Z"))
            .unwrap(),
        at("2026-01-02T04:00:00Z")
    );
    assert!(Rule::Weekly {
        weekday: 0,
        time: "10:00".into()
    }
    .validate()
    .is_err());
    for time in ["2:30", "24:00", "10:60", "10:00:00"] {
        assert!(Rule::Daily { time: time.into() }.validate().is_err());
    }
    assert!(Rule::Interval { hours: 0 }.validate().is_err());
}

#[derive(Clone, Copy)]
struct TransitionZone;
impl TimeZone for TransitionZone {
    type Offset = FixedOffset;
    fn from_offset(_: &Self::Offset) -> Self {
        Self
    }
    fn offset_from_local_date(&self, date: &NaiveDate) -> LocalResult<Self::Offset> {
        self.offset_from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
    }
    fn offset_from_local_datetime(&self, dt: &NaiveDateTime) -> LocalResult<Self::Offset> {
        let gap_start = NaiveDate::from_ymd_opt(2026, 10, 4)
            .unwrap()
            .and_hms_opt(2, 0, 0)
            .unwrap();
        let gap_end = gap_start + chrono::Duration::hours(1);
        if *dt >= gap_start && *dt < gap_end {
            return LocalResult::None;
        }
        let overlap = NaiveDate::from_ymd_opt(2026, 4, 5)
            .unwrap()
            .and_hms_opt(2, 0, 0)
            .unwrap();
        let summer = FixedOffset::east_opt(11 * 3600).unwrap();
        let winter = FixedOffset::east_opt(10 * 3600).unwrap();
        if *dt >= overlap && *dt < overlap + chrono::Duration::hours(1) {
            LocalResult::Ambiguous(summer, winter)
        } else if *dt >= gap_end || *dt < overlap {
            LocalResult::Single(summer)
        } else {
            LocalResult::Single(winter)
        }
    }
    fn offset_from_utc_date(&self, date: &NaiveDate) -> Self::Offset {
        self.offset_from_utc_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
    }
    fn offset_from_utc_datetime(&self, dt: &NaiveDateTime) -> Self::Offset {
        let fall = NaiveDate::from_ymd_opt(2026, 4, 4)
            .unwrap()
            .and_hms_opt(16, 0, 0)
            .unwrap();
        let spring = NaiveDate::from_ymd_opt(2026, 10, 3)
            .unwrap()
            .and_hms_opt(16, 0, 0)
            .unwrap();
        FixedOffset::east_opt(if *dt >= fall && *dt < spring {
            10 * 3600
        } else {
            11 * 3600
        })
        .unwrap()
    }
}
#[test]
fn dst_gap_shifts_forward_and_overlap_never_fires_twice() {
    let rule = Rule::Daily {
        time: "02:30".into(),
    };
    let gap = at("2026-10-03T15:00:00Z");
    assert_eq!(
        rule.next_after(&TransitionZone, gap, gap).unwrap(),
        at("2026-10-03T16:00:00Z")
    ); //03:00 local
    let fall = at("2026-04-04T14:00:00Z");
    let first = rule.next_after(&TransitionZone, fall, fall).unwrap();
    assert_eq!(first, at("2026-04-04T15:30:00Z"));
    assert_eq!(
        rule.next_after(&TransitionZone, fall, first).unwrap(),
        at("2026-04-05T16:30:00Z")
    );
}
#[test]
fn assistant_templates_record_local_dst_slots_once_and_keep_the_confirmed_revision() {
    use crate::personal_assistants::{
        recurring::{CoordinationTask, Template},
        ProjectRef, Seat,
    };
    for (before, due, wall) in [
        (
            "2026-10-03T15:00:00Z",
            "2026-10-03T16:00:00Z",
            "2026-10-04T03:00:00+11:00",
        ),
        (
            "2026-04-04T14:00:00Z",
            "2026-04-04T15:30:00Z",
            "2026-04-05T02:30:00+11:00",
        ),
    ] {
        let root = Fixture::new();
        let store = Store::open(&root.0).unwrap();
        let mut a = args("dst-template");
        a.rule = Rule::Daily {
            time: "02:30".into(),
        };
        let template = Template {
            assistant_id: "assistant".into(),
            assistant_revision: 2,
            template_revision: 1,
            project_ref: ProjectRef {
                instance_id: "fixture".into(),
                project_id: "project".into(),
            },
            confirmed_seat: Seat {
                session_id: Some(TARGET.into()),
                epoch: 4,
            },
            task: CoordinationTask::ProjectStatus,
        };
        let row = store
            .create_linked(
                a,
                Some(template.clone()),
                at(before),
                &TransitionZone,
                |_| Ok(()),
            )
            .unwrap();
        let run = store
            .claim(at(due), &TransitionZone, 8)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(run.slot_local.as_deref(), Some(wall));
        assert_eq!(run.assistant_template, Some(template));
        assert!(run.coalesced_until.is_none());
        assert!(store
            .finish(
                &run,
                "handedOff",
                None,
                Some("fixture-request".into()),
                at(due)
            )
            .unwrap());
        assert!(store
            .claim(at(due) + chrono::Duration::hours(1), &TransitionZone, 8)
            .unwrap()
            .is_empty());
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../management/v2/schema/export/schedules.json"
        ))
        .unwrap();
        jsonschema::validator_for(&schema)
            .unwrap()
            .validate(&json!({"schedule":store.get(&row.id).unwrap()}))
            .unwrap();
    }
}

#[test]
fn crud_is_cas_guarded_and_action_replays_survive_restart() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "create", now);
    drop(store);
    let store = Store::open(&root.0).unwrap();
    assert_eq!(
        store
            .create(args("create"), now, &Utc, |_| panic!(
                "replay must not revalidate target"
            ))
            .unwrap(),
        row
    );
    let mut conflict = args("create");
    conflict.message = "different".into();
    assert!(store.create(conflict, now, &Utc, |_| Ok(())).is_err());
    let update = UpdateArgs {
        action_id: "edit".into(),
        expected_revision: row.revision,
        target_session_id: TARGET.into(),
        message: "changed".into(),
        rule: row.rule.clone(),
        enabled: false,
    };
    let changed = store
        .update(&row.id, update.clone(), now, &Utc, |_| Ok(()))
        .unwrap();
    assert_eq!(changed.revision, 2);
    assert!(changed.next_run_at.is_none());
    assert_eq!(
        store
            .update(&row.id, update, now, &Utc, |_| panic!("edit replay"))
            .unwrap(),
        changed
    );
    let stale = DeleteArgs {
        action_id: "stale".into(),
        expected_revision: 1,
    };
    assert!(store.delete(&row.id, stale, now).is_err());
    let deleted = store
        .delete(
            &row.id,
            DeleteArgs {
                action_id: "delete".into(),
                expected_revision: 2,
            },
            now,
        )
        .unwrap();
    assert!(deleted.deleted_at.is_some());
    assert!(store.get(&row.id).is_err());
    assert_eq!(
        store
            .delete(
                &row.id,
                DeleteArgs {
                    action_id: "delete".into(),
                    expected_revision: 2
                },
                now
            )
            .unwrap(),
        deleted
    );
    assert!(store.list(None, None, 50).unwrap().data.is_empty());
}
#[test]
fn missed_runs_catch_up_once_without_a_backlog_after_restart() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "catch-up", now);
    let after = now + chrono::Duration::days(30);
    let claimed = store.claim(after, &Utc, 8).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].scheduled_at, now + chrono::Duration::hours(1));
    assert!(store.get(&row.id).unwrap().next_run_at.unwrap() > after);
    drop(store);
    let store = Store::open(&root.0).unwrap();
    assert_eq!(store.recover_dispatching(after).unwrap(), 1);
    let retry = store.claim(after, &Utc, 8).unwrap();
    assert_eq!(retry[0].id, claimed[0].id);
    store
        .finish(
            &retry[0],
            "handedOff",
            None,
            Some("alarm-once".into()),
            after,
        )
        .unwrap();
    assert!(store.claim(after, &Utc, 8).unwrap().is_empty());
    assert_eq!(store.runs(&row.id, None, 100).unwrap().data.len(), 1);
}
#[derive(Default)]
struct Fake {
    paused: bool,
    online: bool,
    pause_during_start: bool,
    fail_after_handoff: bool,
    events: Vec<&'static str>,
    delivered: std::collections::BTreeMap<String, String>,
}
impl Runtime for Fake {
    fn check(&mut self, _: &Run) -> anyhow::Result<()> {
        if self.paused {
            Err(Paused.into())
        } else {
            Ok(())
        }
    }
    fn online(&mut self, _: &Run) -> anyhow::Result<()> {
        self.events.push("online");
        if self.pause_during_start {
            self.paused = true;
            return Err(Paused.into());
        }
        self.online = true;
        Ok(())
    }
    fn handoff(&mut self, run: &Run) -> anyhow::Result<String> {
        assert!(self.online);
        self.events.push("handoff");
        let id = self
            .delivered
            .entry(run.id.clone())
            .or_insert_with(|| format!("alarm-{}", run.id))
            .clone();
        if std::mem::take(&mut self.fail_after_handoff) {
            anyhow::bail!("lost receipt after commit");
        }
        Ok(id)
    }
}
#[test]
fn offline_target_is_woken_before_normal_message_handoff() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "offline", now);
    let mut runtime = Fake::default();
    runtime::tick(&store, now + chrono::Duration::hours(1), &Utc, &mut runtime).unwrap();
    assert_eq!(runtime.events, vec!["online", "handoff"]);
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "handedOff"
    );
}
#[test]
fn paused_target_is_held_and_missed_slots_coalesce_then_resume_once() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "pause", now);
    let mut runtime = Fake {
        paused: true,
        ..Default::default()
    };
    runtime::tick(&store, now + chrono::Duration::hours(1), &Utc, &mut runtime).unwrap();
    assert!(runtime.events.is_empty());
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "held"
    );
    runtime::tick(&store, now + chrono::Duration::days(7), &Utc, &mut runtime).unwrap();
    assert_eq!(store.runs(&row.id, None, 100).unwrap().data.len(), 1);
    runtime.paused = false;
    runtime::tick(
        &store,
        now + chrono::Duration::days(7) + chrono::Duration::minutes(2),
        &Utc,
        &mut runtime,
    )
    .unwrap();
    assert_eq!(runtime.delivered.len(), 1);
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "handedOff"
    );
}
#[test]
fn pause_racing_online_never_delivers_and_a_lost_handoff_receipt_reuses_occurrence() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "race", now);
    let mut runtime = Fake {
        pause_during_start: true,
        ..Default::default()
    };
    let due = now + chrono::Duration::hours(1);
    runtime::tick(&store, due, &Utc, &mut runtime).unwrap();
    assert!(runtime.delivered.is_empty());
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "held"
    );
    runtime.paused = false;
    runtime.pause_during_start = false;
    runtime.fail_after_handoff = true;
    runtime::tick(
        &store,
        due + chrono::Duration::minutes(1),
        &Utc,
        &mut runtime,
    )
    .unwrap();
    assert_eq!(runtime.delivered.len(), 1);
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "retrying"
    );
    runtime::tick(
        &store,
        due + chrono::Duration::minutes(2),
        &Utc,
        &mut runtime,
    )
    .unwrap();
    assert_eq!(runtime.delivered.len(), 1);
    assert_eq!(store.runs(&row.id, None, 100).unwrap().data.len(), 1);
}
#[test]
fn disable_cancels_unreserved_runs_and_reserved_run_lease_is_cas_guarded() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "disable", now);
    let due = now + chrono::Duration::hours(1);
    let first = store.claim(due, &Utc, 8).unwrap().remove(0);
    assert!(store.claim(due, &Utc, 8).unwrap().is_empty());
    store
        .finish(&first, "held", Some("paused".into()), None, due)
        .unwrap();
    let update = UpdateArgs {
        action_id: "disable-it".into(),
        expected_revision: 1,
        target_session_id: TARGET.into(),
        message: row.message,
        rule: row.rule,
        enabled: false,
    };
    store
        .update(&row.id, update, due, &Utc, |_| Ok(()))
        .unwrap();
    assert_eq!(
        store.runs(&row.id, None, 1).unwrap().data[0].outcome,
        "cancelled"
    );
    assert!(!store.finish(&first, "handedOff", None, None, due).unwrap());
}
#[test]
fn lists_are_bounded_and_agent_scope_cannot_read_another_target() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    for i in 0..5 {
        let mut a = args(&format!("list-{i}"));
        if i == 4 {
            a.target_session_id = OTHER.into();
        }
        store.create(a, now, &Utc, |_| Ok(())).unwrap();
    }
    let first = store.list(Some(TARGET), None, 2).unwrap();
    assert_eq!(first.data.len(), 2);
    let second = store
        .list(Some(TARGET), first.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(second.data.len(), 2);
    assert!(second.next_cursor.is_none());
    assert!(first
        .data
        .iter()
        .chain(second.data.iter())
        .all(|r| r.target_session_id == TARGET));
    assert_eq!(store.list(Some(OTHER), None, 1000).unwrap().data.len(), 1);
    assert!(runtime::agent_list(TARGET, &json!({"targetSessionId":OTHER})).is_err());
}
#[test]
fn owner_only_routes_and_schema_match_real_resources() {
    use crate::management::v2::schedules_api::owner_allowed;
    assert!(owner_allowed(true, true, true, "POST"));
    assert!(owner_allowed(true, true, false, "GET"));
    for (auth, owner, full) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        assert!(!owner_allowed(auth, owner, full, "POST"));
    }
    assert_eq!(
        crate::management::v2::access::classify("POST", "/v2/schedules"),
        Some(crate::management::v2::access::Class::OwnerOnly)
    );
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "schema", now);
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../management/v2/schema/export/schedules.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    validator.validate(&json!({"schedule":row})).unwrap();
    validator
        .validate(&serde_json::to_value(store.list(None, None, 50).unwrap()).unwrap())
        .unwrap();
}
#[test]
fn schema_version_and_kernel_lock_prevent_parallel_workers() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let lock = store.worker_lock().unwrap();
    assert!(store.worker_lock().is_err());
    drop(lock);
    assert!(store.worker_lock().is_ok());
    let conn = rusqlite::Connection::open(root.0.join("schedules-v1.sqlite3")).unwrap();
    conn.pragma_update(None, "user_version", 999).unwrap();
    drop(conn);
    assert!(Store::open(&root.0).is_err());
}

#[test]
fn retargeted_agent_list_never_discloses_the_previous_targets_audit_message() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "retarget", now);
    let due = now + chrono::Duration::hours(1);
    let run = store.claim(due, &Utc, 1).unwrap().remove(0);
    store
        .finish(
            &run,
            "handedOff",
            None,
            Some("alarm-old-target".into()),
            due,
        )
        .unwrap();
    store
        .update(
            &row.id,
            UpdateArgs {
                action_id: "retarget-edit".into(),
                expected_revision: 1,
                target_session_id: OTHER.into(),
                message: "new target message".into(),
                rule: row.rule,
                enabled: true,
            },
            due,
            &Utc,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        store.list(Some(OTHER), None, 50).unwrap().data[0]
            .last_run
            .as_ref()
            .unwrap()
            .target_session_id,
        TARGET
    );
    let own = runtime::own_list(&store, OTHER, None, 50).unwrap();
    assert_eq!(own.data[0].target_session_id, OTHER);
    assert!(own.data[0].last_run.is_none());
    assert!(runtime::own_list(&store, TARGET, None, 50)
        .unwrap()
        .data
        .is_empty());
    assert_eq!(
        store.runs(&row.id, None, 50).unwrap().data[0].target_session_id,
        TARGET
    );
}

#[test]
fn concurrent_claims_share_one_durable_occurrence() {
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "concurrent", now);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .claim(now + chrono::Duration::hours(1), &Utc, 8)
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    let all: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(all.len(), 1);
    assert_eq!(store.runs(&row.id, None, 100).unwrap().data.len(), 1);
}
#[test]
fn missing_or_archived_target_is_audited_without_online_or_handoff() {
    struct Missing;
    impl Runtime for Missing {
        fn check(&mut self, _: &Run) -> anyhow::Result<()> {
            Err(runtime::Unavailable("target is archived".into()).into())
        }
        fn online(&mut self, _: &Run) -> anyhow::Result<()> {
            panic!("must not wake an archived target")
        }
        fn handoff(&mut self, _: &Run) -> anyhow::Result<String> {
            panic!("must not deliver to replacement target")
        }
    }
    let root = Fixture::new();
    let store = Store::open(&root.0).unwrap();
    let now = at("2026-01-01T00:00:00Z");
    let row = create(&store, "archived", now);
    runtime::tick(&store, now + chrono::Duration::hours(1), &Utc, &mut Missing).unwrap();
    assert_eq!(
        store.get(&row.id).unwrap().last_run.unwrap().outcome,
        "refused"
    );
}

#[test]
fn held_and_retrying_backoff_is_durable_and_caps_at_ten_minutes() {
    for outcome in ["held", "retrying"] {
        let root = Fixture::new();
        let mut store = Store::open(&root.0).unwrap();
        let now = at("2026-01-01T00:00:00Z");
        let row = create(&store, outcome, now);
        let mut due = now + chrono::Duration::hours(1);
        for (i, seconds) in [30, 60, 120, 240, 480, 600, 600].into_iter().enumerate() {
            let run = store.claim(due, &Utc, 1).unwrap().remove(0);
            assert_eq!(run.attempts, i as u32 + 1);
            store
                .finish(&run, outcome, Some("temporary hold".into()), None, due)
                .unwrap();
            store = Store::open(&root.0).unwrap();
            let saved = store.get(&row.id).unwrap().last_run.unwrap();
            assert_eq!(
                saved.next_attempt_at,
                due + chrono::Duration::seconds(seconds)
            );
            due = saved.next_attempt_at;
            assert!(store
                .claim(due - chrono::Duration::milliseconds(1), &Utc, 1)
                .unwrap()
                .is_empty());
        }
        assert_eq!(store.runs(&row.id, None, 100).unwrap().data.len(), 1);
    }
}
