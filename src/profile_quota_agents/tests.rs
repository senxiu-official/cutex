use super::*;
use serde_json::json;
fn account(id: &str, kind: &str) -> crate::profiles::model::StoredAccount {
    serde_json::from_value(json!({"id":id,"name":id,"cli_kind":kind})).unwrap()
}
fn gauge() -> crate::profiles::quota::Gauge {
    crate::profiles::quota::Gauge {
        id: "claude-0".into(),
        label: "Claude".into(),
        display_label: None,
        kind: "window",
        remaining_percent: Some(80.0),
        balance: None,
        reset_at: Some(2000),
        window_label: Some("5h".into()),
        observed_at: Some(1000),
        stale: false,
        error: None,
        used_percent: Some(20.0),
        source: None,
    }
}
#[test]
fn stale_failed_future_and_unobserved_reset_never_look_healthy() {
    let fresh = gauge();
    assert_eq!(window(&fresh, &[], "a", 1001).remaining_percent, Some(80.0));
    for g in [
        crate::profiles::quota::Gauge {
            stale: true,
            ..fresh.clone()
        },
        crate::profiles::quota::Gauge {
            error: Some("read failed".into()),
            ..fresh.clone()
        },
        crate::profiles::quota::Gauge {
            observed_at: Some(1100),
            ..fresh.clone()
        },
        crate::profiles::quota::Gauge {
            reset_at: Some(1001),
            ..fresh.clone()
        },
    ] {
        let row = window(&g, &[], "a", 1001);
        assert_eq!(row.state, "unknown");
        assert_eq!(row.remaining_percent, None);
        assert_eq!(row.forecast.state, "unknown");
    }
    assert_eq!(window(&fresh, &[], "a", 1901).state, "unknown");
}
#[test]
fn shared_claude_account_has_one_pool_and_unknown_native_identity_stays_separate() {
    let mut a = account("a", "claude");
    let mut b = account("b", "claude");
    assert_eq!(pool_id(&a), pool_id(&b));
    b.claude_account = Some("other".into());
    assert_ne!(pool_id(&a), pool_id(&b));
    a = account("a", "codex");
    b = account("b", "codex");
    assert_ne!(pool_id(&a), pool_id(&b));
    a.private_chatgpt_auth_file = true;
    b.private_chatgpt_auth_file = true;
    assert_eq!(pool_id(&a), pool_id(&b));
}
#[test]
fn p8_consumer_seam_never_exposes_hidden_members() {
    let rows = attribution::visible_rows(
        json!({"rows":[{"cutexSessionId":"hidden","agentName":"secret"},{"cutexSessionId":"visible","agentName":"reviewer","share":30.0,"percent":null}]}),
        |id| id == "visible",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "visible");
    assert!(!serde_json::to_string(&rows).unwrap().contains("secret"));
}
fn member() -> Member {
    Member {
        id: "member".into(),
        name: Some("reviewer".into()),
        profile: "a".into(),
        model: Some("opus".into()),
        reasoning: Some("high".into()),
        busy: true,
        queued: 0,
        started_at: Some(1000),
    }
}
fn view_for_alert() -> View {
    View {
        id: "a".into(),
        name: "A".into(),
        provider: "anthropic".into(),
        pool_id: "pool-shared".into(),
        auto_pick: true,
        state: "known".into(),
        windows: vec![window(
            &crate::profiles::quota::Gauge {
                remaining_percent: Some(9.0),
                ..gauge()
            },
            &[],
            "a",
            1001,
        )],
        busy_member_count: 1,
        queued_member_count: 0,
        members: vec![member()],
        members_truncated: false,
        top_consumers: None,
    }
}
#[test]
fn alerts_dedupe_across_shared_profiles_and_restart_and_retry_the_same_message() {
    let view = view_for_alert();
    let first = alerts::note(&view, view.windows.first(), "project", "director", 1001).unwrap();
    let mut alias = view.clone();
    alias.id = "b".into();
    alias.name = "B".into();
    assert_eq!(
        first.id,
        alerts::note(&alias, alias.windows.first(), "project", "director", 1001)
            .unwrap()
            .id
    );
    let root = std::env::temp_dir().join(format!("profile-quota-outbox-{}", uuid::Uuid::new_v4()));
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        outbox.enqueue(&first, 1001).unwrap();
        outbox.enqueue(&first, 1002).unwrap();
        let mut attempts = 0;
        outbox
            .drain(1002, |n| {
                assert_eq!(n.id, first.id);
                attempts += 1;
                anyhow::bail!("lost receipt")
            })
            .unwrap();
        assert_eq!(attempts, 1);
    }
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        assert_eq!(outbox.pending().unwrap().len(), 1);
        outbox
            .drain(1003, |n| {
                assert_eq!(n.id, first.id);
                Ok(())
            })
            .unwrap();
        outbox.enqueue(&first, 1004).unwrap();
        assert!(outbox.pending().unwrap().is_empty());
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn unknown_stale_future_unobserved_and_no_work_never_alert() {
    let mut view = view_for_alert();
    for state in ["unknown", "stale", "error"] {
        view.windows[0].state = state.into();
        for observed in [None, Some(1000), Some(1002)] {
            view.windows[0].observed_at = observed;
            assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
            assert!(alerts::profile_note(&view, "p", "d", 4600).is_none());
        }
    }
    view = view_for_alert();
    for observed in [None, Some(0), Some(1002)] {
        view.windows[0].observed_at = observed;
        assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    }
    view = view_for_alert();
    view.windows[0].reset_at = Some(1001);
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    view.windows.clear();
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    view = view_for_alert();
    view.members.clear();
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
}

#[test]
fn multiple_windows_share_one_note_and_each_cycle_is_deduplicated() {
    let root = std::env::temp_dir().join(format!("quota-group-{}", uuid::Uuid::new_v4()));
    let mut view = view_for_alert();
    view.windows[0].label = Some("5h".into());
    let mut weekly = view.windows[0].clone();
    weekly.id = "weekly".into();
    weekly.label = Some("7d".into());
    weekly.reset_at = Some(90000);
    let mut fable = weekly.clone();
    fable.id = "fable".into();
    fable.label = Some("Fable 7d".into());
    view.windows.extend([weekly, fable]);
    let first = alerts::profile_note(&view, "p", "d", 1001).unwrap();
    assert_eq!(first.text.matches("[额度提醒]").count(), 1);
    for label in ["5h", "7d", "Fable 7d"] {
        assert!(first.text.contains(label));
    }
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        outbox.enqueue(&first, 1001).unwrap();
        assert_eq!(outbox.pending().unwrap().len(), 1);
        outbox.drain(1001, |_| Ok(())).unwrap();
    }
    let outbox = alerts::Outbox::open(&root).unwrap();
    view.windows.reverse(); // Stable IDs and dedupe even after API ordering changes.
    outbox
        .enqueue(&alerts::profile_note(&view, "p", "d", 1002).unwrap(), 1002)
        .unwrap();
    assert!(outbox.pending().unwrap().is_empty());
    let short = view
        .windows
        .iter_mut()
        .find(|w| w.id != "weekly" && w.id != "fable")
        .unwrap();
    short.remaining_percent = Some(80.0);
    short.reset_at = Some(20000);
    outbox
        .enqueue(&alerts::profile_note(&view, "p", "d", 1003).unwrap(), 1003)
        .unwrap();
    assert!(outbox.pending().unwrap().is_empty()); // New healthy 5h cycle does not repeat weekly warnings.
    let short = view
        .windows
        .iter_mut()
        .find(|w| w.id != "weekly" && w.id != "fable")
        .unwrap();
    short.remaining_percent = Some(8.0);
    outbox
        .enqueue(&alerts::profile_note(&view, "p", "d", 1004).unwrap(), 1004)
        .unwrap();
    assert_eq!(outbox.pending().unwrap().len(), 1); // A genuinely new 5h low cycle can alert once.
    let text = &outbox.pending().unwrap()[0].text;
    assert!(text.contains("5h"));
    assert!(!text.contains("7d"));
    assert!(!text.contains("Fable")); // Already-reported windows are not repeated in the new note.
    drop(outbox);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn only_low_exhausted_or_imminent_real_forecasts_alert() {
    let mut view = view_for_alert();
    view.windows[0].remaining_percent = Some(10.0);
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    view.windows[0].forecast = Forecast {
        state: "runsOut".into(),
        runs_out_at: Some(2801),
        percent_per_hour: Some(10.0),
    };
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_some());
    view.windows[0].forecast.runs_out_at = Some(2802);
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    view.windows[0].forecast.state = "unknown".into();
    view.windows[0].forecast.runs_out_at = Some(1100);
    assert!(alerts::profile_note(&view, "p", "d", 1001).is_none());
    view.windows[0].forecast.state = "exhausted".into();
    view.windows[0].remaining_percent = None;
    assert!(alerts::profile_note(&view, "p", "d", 1001)
        .unwrap()
        .text
        .contains("已用完"));
}

#[test]
fn restart_discards_legacy_pending_unknown_but_keeps_sent_receipts() {
    let root = std::env::temp_dir().join(format!("quota-old-{}", uuid::Uuid::new_v4()));
    {
        let _outbox = alerts::Outbox::open(&root).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(root.join("quota-notes-v1.sqlite3")).unwrap();
        conn.execute_batch("INSERT INTO quota_notes VALUES('pending','d','额度状态未知，最近读数不可用',NULL,1000,'old:unknown',2000,3000); INSERT INTO quota_notes VALUES('sent','d','old diagnostic',1000,1000,'sent:unknown',2000,3000);").unwrap();
    }
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        assert!(outbox.pending().unwrap().is_empty());
    }
    let conn = rusqlite::Connection::open(root.join("quota-notes-v1.sqlite3")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM quota_notes WHERE id='sent'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(conn);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reset_jitter_and_known_unknown_threshold_flaps_deliver_one_note_even_at_bucket_edges() {
    let root = std::env::temp_dir().join(format!("quota-jitter-{}", uuid::Uuid::new_v4()));
    let outbox = alerts::Outbox::open(&root).unwrap();
    let view = view_for_alert();
    for reset in [1794983738, 1794983739, 1794983737, 1794983999, 1794984000] {
        let mut w = view.windows[0].clone();
        w.reset_at = Some(reset);
        let known = alerts::note(&view, Some(&w), "p", "d", 1001).unwrap();
        outbox.enqueue(&known, 1001).unwrap();
        w.state = "unknown".into();
        w.observed_at = None;
        w.remaining_percent = None;
        assert!(alerts::note(&view, Some(&w), "p", "d", 1001).is_none());
        w.state = "known".into();
        w.observed_at = Some(1000);
        w.remaining_percent = Some(11.0);
        assert!(alerts::note(&view, Some(&w), "p", "d", 1001).is_none());
        w.remaining_percent = Some(9.0);
        outbox
            .enqueue(
                &alerts::note(&view, Some(&w), "p", "d", 1001).unwrap(),
                1003,
            )
            .unwrap();
    }
    assert_eq!(outbox.pending().unwrap().len(), 1);
    let mut sends = 0;
    outbox
        .drain(1004, |_| {
            sends += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(sends, 1);
    drop(outbox);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pruning_removes_only_old_sent_bookkeeping_after_the_active_cycle() {
    let root = std::env::temp_dir().join(format!("quota-prune-{}", uuid::Uuid::new_v4()));
    let outbox = alerts::Outbox::open(&root).unwrap();
    let view = view_for_alert();
    let mut old = alerts::note(&view, view.windows.first(), "p", "d", 1001).unwrap();
    old.retain_until = 100;
    outbox.enqueue(&old, 10).unwrap();
    outbox.drain(20, |_| Ok(())).unwrap();
    let mut active = old.clone();
    active.id = "active".into();
    active.scope = "active".into();
    for c in &mut active.cycles {
        c.scope = "active-window".into();
    }
    active.retain_until = 9 * 86400;
    outbox.enqueue(&active, 10).unwrap();
    outbox.drain(20, |_| Ok(())).unwrap();
    let mut pending = old.clone();
    pending.id = "pending".into();
    pending.scope = "pending".into();
    for c in &mut pending.cycles {
        c.scope = "pending-window".into();
    }
    outbox.enqueue(&pending, 10).unwrap();
    assert_eq!(outbox.prune(8 * 86400).unwrap(), 1);
    outbox.enqueue(&active, 8 * 86400).unwrap();
    assert_eq!(outbox.pending().unwrap().len(), 1);
    assert_eq!(outbox.prune(10 * 86400).unwrap(), 1);
    assert_eq!(outbox.pending().unwrap()[0].id, "pending");
    drop(outbox);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn v1_outbox_migration_preserves_undelivered_text() {
    let root = std::env::temp_dir().join(format!("quota-v1-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("quota-notes-v1.sqlite3");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE quota_notes(id TEXT PRIMARY KEY,recipient TEXT NOT NULL,text TEXT NOT NULL,sent_at INTEGER,created_at INTEGER NOT NULL); CREATE INDEX quota_notes_pending ON quota_notes(created_at,id) WHERE sent_at IS NULL; INSERT INTO quota_notes VALUES('old','director','keep',NULL,1000);PRAGMA user_version=1;").unwrap();
    }
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        assert_eq!(outbox.pending().unwrap()[0].text, "keep");
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn old_v2_warning_receipts_suppress_grouped_duplicates_and_old_readers_still_work() {
    let root = std::env::temp_dir().join(format!("quota-v2-{}", uuid::Uuid::new_v4()));
    let view = view_for_alert();
    let note = alerts::profile_note(&view, "p", "d", 1001).unwrap();
    {
        let _outbox = alerts::Outbox::open(&root).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(root.join("quota-notes-v1.sqlite3")).unwrap();
        conn.execute_batch("DROP TABLE quota_note_windows;")
            .unwrap();
        conn.execute("INSERT INTO quota_notes VALUES('old-warning','d','real warning',1000,1000,?1,2000,3000)",[&note.cycles[0].scope]).unwrap();
    }
    {
        let outbox = alerts::Outbox::open(&root).unwrap();
        outbox.enqueue(&note, 1001).unwrap();
        assert!(outbox.pending().unwrap().is_empty());
        let mut other = view.clone();
        other.windows[0].id = "different-window".into();
        outbox
            .enqueue(&alerts::profile_note(&other, "p", "d", 1001).unwrap(), 1001)
            .unwrap();
        assert_eq!(outbox.pending().unwrap().len(), 1);
    }
    {
        let conn = rusqlite::Connection::open(root.join("quota-notes-v1.sqlite3")).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        // Simulate predecessor pruning without knowing about the new table.
        conn.execute("DELETE FROM quota_notes WHERE id!='old-warning'", [])
            .unwrap();
    }
    {
        let _outbox = alerts::Outbox::open(&root).unwrap();
    }
    let conn = rusqlite::Connection::open(root.join("quota-notes-v1.sqlite3")).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM quota_note_windows", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(conn);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn idle_pass_does_not_read_broken_profile_or_session_files() {
    use crate::config::instance::{self, Selection};
    let _guard = crate::config::paths::test_home_env_lock();
    let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(format!("quota-idle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".cutex")).unwrap();
    std::fs::write(root.join(".cutex-test-private-home"), b"").unwrap();
    struct Reset {
        previous: Selection,
        root: std::path::PathBuf,
    }
    impl Drop for Reset {
        fn drop(&mut self) {
            instance::set_test_selection(self.previous.clone());
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    let _reset = Reset {
        previous: instance::selected().unwrap(),
        root: root.clone(),
    };
    instance::set_test_selection(Selection::Custom(root.join(".cutex")));
    let profiles = crate::config::paths::accounts_path().unwrap();
    std::fs::write(profiles, b"broken profile state").unwrap();
    let sessions = crate::session::store::cutex_sessions_path().unwrap();
    if let Some(parent) = sessions.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(sessions, b"broken session state").unwrap();
    alerts::pass(|_| panic!("no pending note on idle fixture")).unwrap();
}
#[test]
fn load_ignores_idle_offline_and_paused_but_includes_launch_reservations() {
    assert!(!load::working(true, false, false, 0, false));
    assert!(!load::working(false, true, false, 4, false));
    assert!(!load::working(true, true, true, 4, false));
    assert!(load::working(true, false, false, 2, false));
    assert!(load::working(true, true, false, 0, false));
    assert!(load::working(false, false, false, 0, true));
}
#[test]
fn weighted_load_adds_unobserved_starts_without_double_counting_observed_work() {
    let mut prior = member();
    prior.started_at = Some(0);
    let mut new = member();
    new.id = "new".into();
    new.started_at = Some(1200);
    assert_eq!(
        load::rate(Some(20.0), 1100, Some("5h"), &[prior.clone()]),
        20.0
    );
    assert_eq!(
        load::rate(Some(20.0), 1100, Some("5h"), &[prior.clone(), new]),
        32.0
    );
    let mut small = prior.clone();
    small.model = Some("mini".into());
    small.reasoning = Some("low".into());
    assert!(
        load::rate(None, 1100, Some("5h"), &[small]) < load::rate(None, 1100, Some("5h"), &[prior])
    );
}
#[test]
fn cached_quota_projection_does_not_connect_to_the_configured_endpoint() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut a = account(&uuid::Uuid::new_v4().to_string(), "codex");
    a.options.quota.enabled = true;
    a.options.quota.url = format!("http://{}/usage", listener.local_addr().unwrap());
    a.options.quota.auth = crate::profiles::quota::Auth::None;
    let mut optional = a.options.quota.clone();
    optional.optional = true;
    optional.window_label = Some("week".into());
    a.options.quotas.push(optional);
    let gauges = crate::profiles::quota::cached_gauges(&a).unwrap();
    assert_eq!(gauges.len(), 1);
    assert!(gauges[0].stale);
    assert_eq!(gauges[0].remaining_percent, None);
    assert!(matches!(listener.accept(),Err(e)if e.kind()==std::io::ErrorKind::WouldBlock));
}
#[test]
fn current_role_is_rechecked_after_a_director_rotation() {
    use crate::agent_management::{AgentManagementSnapshot, ProjectAuthority, ProjectId};
    let a = "cutex.00000000-0000-0000-0000-000000000001";
    let b = "cutex.00000000-0000-0000-0000-000000000002";
    let mut roster = AgentManagementSnapshot::empty();
    assert!(!manager(&roster, a));
    let authority:ProjectAuthority=serde_json::from_value(json!({"project_id":"project","authorized_director_session":a,"authority_epoch":1,"updated_at":"2026-11-17T00:00:00Z"})).unwrap();
    roster
        .projects
        .insert(ProjectId::new("project").unwrap(), authority);
    assert!(manager(&roster, a));
    assert!(!manager(&roster, b));
    let authority:ProjectAuthority=serde_json::from_value(json!({"project_id":"project","authorized_director_session":b,"authority_epoch":2,"updated_at":"2026-11-17T00:00:00Z"})).unwrap();
    roster
        .projects
        .insert(ProjectId::new("project").unwrap(), authority);
    assert!(!manager(&roster, a));
    assert!(manager(&roster, b));
}
#[test]
fn forecast_needs_a_measured_span_and_warns_before_the_reset() {
    let mut g = gauge();
    g.observed_at = Some(10000);
    g.reset_at = Some(20000);
    g.remaining_percent = Some(8.0);
    let point = crate::management::v2::usage::QuotaSnapshot {
        ts: 6400,
        profile_id: "a".into(),
        profile: "A".into(),
        gauge_id: g.id.clone(),
        label: None,
        kind: Some("window".into()),
        remaining_percent: Some(32.0),
        balance: None,
        reset_at: g.reset_at,
    };
    let f = forecast(&g, &[point], "a", 10000);
    assert_eq!(f.state, "runsOut");
    assert_eq!(f.runs_out_at, Some(11200));
    assert_eq!(forecast(&g, &[], "a", 10000).state, "unknown");
    let mut v = view_for_alert();
    v.windows[0].remaining_percent = Some(50.0);
    v.windows[0].forecast = f;
    v.windows[0].observed_at = Some(10000);
    v.windows[0].reset_at = Some(20000);
    assert!(alerts::note(&v, v.windows.first(), "p", "d", 10000).is_some());
    assert!(alerts::note(&v, v.windows.first(), "p", "d", 9000).is_none());
    let before = alerts::note(&v, v.windows.first(), "p", "d", 10000)
        .unwrap()
        .id;
    v.windows[0].reset_at = Some(50000);
    assert_ne!(
        before,
        alerts::note(&v, v.windows.first(), "p", "d", 10000)
            .unwrap()
            .id
    );
    v.state = "notApplicable".into();
    assert!(alerts::note(&v, None, "p", "d", 10000).is_none());
}

#[test]
fn model_specific_weekly_labels_keep_the_weekly_horizon() {
    assert_eq!(
        crate::management::v2::profile_auto_pick::window_hours(Some("Opus 7d")),
        Some(168.0)
    );
    let mut g = gauge();
    g.window_label = Some("Opus 7d".into());
    g.observed_at = Some(10000);
    g.reset_at = Some(200000);
    g.remaining_percent = Some(8.0);
    let point = crate::management::v2::usage::QuotaSnapshot {
        ts: 6400,
        profile_id: "a".into(),
        profile: "A".into(),
        gauge_id: g.id.clone(),
        label: None,
        kind: Some("window".into()),
        remaining_percent: Some(32.0),
        balance: None,
        reset_at: g.reset_at,
    };
    assert_eq!(forecast(&g, &[point], "a", 10000).state, "unknown");
}
