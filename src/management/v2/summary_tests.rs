use super::*;
use crate::management::v2::model::{
    CutexMessage, EventCorrelation, EventSource, NativeMessage, NativeMessageKind,
};
use crate::management::listeners::RevocationWatch;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-summary-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn envelope(session: &str, native: Option<Value>, cutex: Option<(&str, Value)>) -> EventEnvelope {
    EventEnvelope {
        contract_version: 2,
        event_id: "e".into(),
        cursor: "c".into(),
        stream_id: "s".into(),
        sequence: 1,
        received_at: "2026-10-08T01:02:03Z".into(),
        cutex_session_id: session.into(),
        host_id: "h".into(),
        source: if native.is_some() { EventSource::AppServer } else { EventSource::Cutex },
        sensitivity: "owner".into(),
        schema: None,
        correlation: EventCorrelation::default(),
        native: native.map(|message| NativeMessage {
            kind: NativeMessageKind::Notification,
            message,
        }),
        cutex: cutex.map(|(method, params)| CutexMessage {
            method: method.into(),
            params,
        }),
    }
}

#[test]
fn preview_collapses_whitespace_and_truncates() {
    assert_eq!(preview_text("  a \n\n b\tc ", 120).as_deref(), Some("a b c"));
    assert_eq!(preview_text(" \n ", 120), None);
    let long = "x".repeat(200);
    let cut = preview_text(&long, 120).unwrap();
    assert_eq!(cut.chars().count(), 120);
    assert!(cut.ends_with('…'));
}

#[test]
fn reduce_tracks_preview_turns_and_unread() {
    let mut projections = HashMap::new();
    let message = envelope(
        "cutex.a",
        Some(json!({"method":"item/completed","params":{"item":{"type":"agentMessage","text":"Done.\n\nAll tests pass."}}})),
        None,
    );
    assert!(reduce(&mut projections, &message).is_none());
    let ended = envelope(
        "cutex.a",
        Some(json!({"method":"turn/completed","params":{"turn":{"id":"t1","status":"completed"}}})),
        None,
    );
    let (name, data) = reduce(&mut projections, &ended).unwrap();
    assert_eq!(name, "cutex/summary/turnEnded");
    assert_eq!(
        data,
        json!({"id":"cutex.a","turnId":"t1","status":"completed","preview":"Done. All tests pass."})
    );
    reduce(&mut projections, &ended).unwrap();
    let projection = &projections["cutex.a"];
    assert_eq!(projection.unread_turns, 2);
    assert_eq!(projection.last_turn.as_ref().unwrap().ended_at, "2026-10-08T01:02:03.000Z");

    // An in-progress status is not a turn end.
    let running = envelope(
        "cutex.a",
        Some(json!({"method":"turn/completed","params":{"turn":{"id":"t2","status":"inProgress"}}})),
        None,
    );
    assert!(reduce(&mut projections, &running).is_none());

    // Owner input clears unread.
    let input = envelope("cutex.a", None, Some(("cutex/userInput/submitted", json!({}))));
    assert!(reduce(&mut projections, &input).is_none());
    assert_eq!(projections["cutex.a"].unread_turns, 0);

    // Host events are ignored.
    let host = envelope(
        super::super::services::HOST_EVENT_SESSION_ID,
        Some(json!({"method":"turn/completed","params":{"turn":{"id":"t","status":"failed"}}})),
        None,
    );
    assert!(reduce(&mut projections, &host).is_none());
}

#[test]
fn command_titles_strip_shell_wrappers_and_truncate() {
    assert_eq!(
        command_title(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoProfile -Command 'cargo test --locked'"#),
        "cargo test --locked"
    );
    assert_eq!(command_title("powershell.exe -c Get-ChildItem -Force"), "Get-ChildItem -Force");
    assert_eq!(command_title(r#"cmd.exe /d /c "dir /b""#), "dir /b");
    assert_eq!(command_title("/bin/bash -lc 'git status && git diff'"), "git status && git diff");
    assert_eq!(command_title("sh -c \"bash -lc 'ls -la'\""), "ls -la");
    assert_eq!(command_title("git   status\n  --short"), "git status --short");
    // Not a wrapper / wrapper without a command: kept.
    assert_eq!(command_title("bash script.sh"), "bash script.sh");
    assert_eq!(command_title("pwsh -NoProfile"), "pwsh -NoProfile");
    let long = command_title(&format!("bash -lc '{}'", "echo x; ".repeat(30)));
    assert_eq!(long.chars().count(), COMMAND_TITLE_CHARS);
    assert!(long.ends_with('…') && long.starts_with("echo x;"));
    assert_eq!(
        native_need(&json!({"id":1,"method":"item/commandExecution/requestApproval","params":{"command":"pwsh -Command 'npm run build'"}})),
        Some(("approval", "npm run build".to_string()))
    );
}

#[test]
fn native_requests_of_offline_or_errored_runtimes_are_not_needs() {
    let sessions = vec![
        session("cutex.live", true, Some("t")),
        session("cutex.off", false, None),
        json!({"cutexSessionId":"cutex.err","runtime":{"status":"error","runtimeGeneration":1}}),
    ];
    let request = |id: &str, generation: u64| (id.to_string(), generation, json!({"id":1,"method":"item/tool/requestUserInput","params":{}}), "2026-10-08T00:00:00+00:00".to_string());
    let answerable = answerable_native_requests(
        &sessions,
        vec![request("cutex.live", 1), request("cutex.live", 0), request("cutex.off", 1), request("cutex.err", 1), request("cutex.gone", 1)],
    );
    assert_eq!(answerable.len(), 1);
    assert_eq!(answerable[0].0, "cutex.live");
    let sources = NeedSources { native: answerable, ..Default::default() };
    let synthesis = synthesize(&sessions, &HashMap::new(), &BTreeMap::new(), &sources, &HashMap::new());
    assert_eq!(synthesis.needs_you_count, 1);
    let live = synthesis.agents.iter().find(|row| row["id"] == "cutex.live").unwrap();
    assert_eq!(live["needs"][0]["at"], "2026-10-08T00:00:00.000Z");
    let off = synthesis.agents.iter().find(|row| row["id"] == "cutex.off").unwrap();
    assert_eq!(off["status"], "offline");
    assert_eq!(off["needs"], json!([]));
    crate::management::v2::schema_export::assert_valid("summary", "agent", live);
}

#[test]
fn native_requests_and_asks_map_to_need_kinds() {
    assert_eq!(
        native_need(&json!({"id":1,"method":"item/commandExecution/requestApproval","params":{"command":"cargo test"}})),
        Some(("approval", "cargo test".to_string()))
    );
    assert_eq!(
        native_need(&json!({"id":2,"method":"item/tool/requestUserInput","params":{"questions":[{"question":"Which branch?"}]}})),
        Some(("question", "Which branch?".to_string()))
    );
    assert_eq!(
        native_need(&json!({"id":3,"method":"mcpServer/elicitation/request","params":{"message":"Pick"}})).unwrap().0,
        "question"
    );
    assert_eq!(ask_need_kind(&json!({"execution":"running","ownerQuestions":1})), Some("question"));
    assert_eq!(ask_need_kind(&json!({"execution":"pendingReview"})), Some("delegationReview"));
    assert_eq!(
        ask_need_kind(&json!({"execution":"interrupted","attempt":{"interruptReason":"outcomeUnknown"},"requester":{"kind":"owner"}})),
        Some("delegationReview")
    );
    // Default tables: an agent's outcome-unknown delegation is a record.
    assert_eq!(ask_need_kind(&json!({"execution":"interrupted","attempt":{"interruptReason":"outcomeUnknown"}})), None);
    assert_eq!(
        ask_need_kind(&json!({"execution":"answered","review":"awaitingAcceptance","requester":{"kind":"owner"}})),
        Some("acceptance")
    );
    assert_eq!(
        ask_need_kind(&json!({"execution":"answered","review":"awaitingAcceptance","requester":{"kind":"agent"}})),
        None
    );
    assert_eq!(ask_need_kind(&json!({"execution":"running"})), None);
    assert_eq!(ask_need_kind(&json!({"execution":"pendingReview","closed":true})), None);
}

fn session(id: &str, online: bool, active: Option<&str>) -> Value {
    json!({
        "cutexSessionId": id,
        "displayName": format!("Agent {id}"),
        "lifecycle": "active",
        "createdAt": "2026-10-07T00:00:00Z",
        "updatedAt": "2026-10-08T00:09:00Z",
        "runtime": {"status": if online {"online"} else {"offline"}, "activeTurnId": active, "lastEventAt": "2026-10-08T00:09:00Z", "runtimeGeneration": 1},
        "activity": {"lastOutput": {"displayText": "fallback   text"}, "lastConversationAt": "2026-10-08T00:05:00Z"},
    })
}

#[test]
fn synthesize_builds_rows_status_needs_and_counts() {
    let sessions = vec![
        session("cutex.idle", true, None),
        session("cutex.work", true, Some("t9")),
        session("cutex.off", false, None),
        session("cutex.need", true, Some("t1")),
        json!({"cutexSessionId":"cutex.gone","lifecycle":"retired","runtime":{}}),
    ];
    let meta = HashMap::from([(
        "cutex.work".to_string(),
        AgentMeta { kind: "agent", provider: "claude", project_id: Some("p1".into()) },
    )]);
    let projects = BTreeMap::from([
        ("p1".to_string(), json!({"id":"p1","name":"Proj","color":"blue"})),
        ("p2".to_string(), json!({"id":"p2","name":"Unused","color":null})),
    ]);
    let sources = NeedSources {
        native: vec![(
            "cutex.need".into(),
            json!({"id":7,"method":"item/fileChange/requestApproval","params":{"reason":"edit"}}),
            "2026-10-08T00:01:00Z".into(),
        )],
        approvals: vec![
            json!({"approvalId":"ap1","requesterCutexSessionId":"cutex.need","summary":"deploy","createdAt":"2026-10-08T00:00:30Z"}),
            json!({"approvalId":"ap2","requesterCutexSessionId":"cutex.elsewhere","summary":"x","createdAt":"2026-10-08T00:00:30Z"}),
        ],
        asks: vec![
            json!({"askId":"ask1","target":"cutex.need","title":"Review","execution":"pendingReview","createdAt":"2026-10-08T00:02:00Z"}),
            json!({"askId":"ask2","target":"cutex.idle","title":"Busy","execution":"running","createdAt":"2026-10-08T00:02:00Z"}),
        ],
        ..Default::default()
    };
    let projections = HashMap::from([(
        "cutex.idle".to_string(),
        AgentProjection {
            last_turn: Some(LastTurn { turn_id: "t0".into(), status: "completed".into(), ended_at: "x".into() }),
            unread_turns: 3,
            preview: Some("from events".into()),
            read_at: None,
        },
    )]);
    let synthesis = synthesize(&sessions, &meta, &projects, &sources, &projections);
    let by_id: HashMap<&str, &Value> =
        synthesis.agents.iter().map(|row| (row["id"].as_str().unwrap(), row)).collect();
    assert_eq!(synthesis.agents.len(), 4);
    assert_eq!(by_id["cutex.idle"]["status"], "idle");
    assert_eq!(by_id["cutex.idle"]["preview"], "from events");
    assert_eq!(by_id["cutex.idle"]["unreadTurns"], 3);
    assert_eq!(by_id["cutex.idle"]["lastTurn"]["turnId"], "t0");
    assert_eq!(by_id["cutex.work"]["status"], "working");
    assert_eq!(by_id["cutex.work"]["provider"], "claude");
    assert_eq!(by_id["cutex.work"]["projectId"], "p1");
    assert_eq!(by_id["cutex.work"]["preview"], "fallback text");
    assert_eq!(by_id["cutex.work"]["lastActivityAt"], "2026-10-08T00:05:00.000Z");
    assert_eq!(by_id["cutex.off"]["status"], "offline");
    assert_eq!(by_id["cutex.off"]["projectId"], Value::Null);
    let need = by_id["cutex.need"];
    assert_eq!(need["status"], "needsInput");
    let kinds: Vec<&str> = need["needs"].as_array().unwrap().iter().map(|n| n["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["ownerApproval", "approval", "delegationReview"]);
    assert_eq!(synthesis.projects, vec![json!({"id":"p1","name":"Proj","color":"blue"})]);
    assert_eq!(synthesis.approvals_count, 2);
    assert_eq!(synthesis.asks_count, 1);
    // cutex.need + the approval of an unlisted agent.
    assert_eq!(synthesis.needs_you_count, 2);
}

#[test]
fn last_activity_ignores_runtime_events_metadata_and_interrupted_turns() {
    let mut fresh = session("cutex.fresh", true, None);
    fresh["activity"] = json!({});
    // No conversation yet: the creation time, never lastEventAt / updatedAt.
    let row = agent_row(&fresh, None, &[], None).unwrap();
    assert_eq!(row["lastActivityAt"], "2026-10-07T00:00:00.000Z");
    let turn = |status: &str| AgentProjection {
        last_turn: Some(LastTurn { turn_id: "t1".into(), status: status.into(), ended_at: "2026-10-07T06:00:00Z".into() }),
        ..Default::default()
    };
    let row = agent_row(&fresh, None, &[], Some(&turn("completed"))).unwrap();
    assert_eq!(row["lastActivityAt"], "2026-10-07T06:00:00.000Z");
    // The interrupt of a turn a restart left in progress is not activity.
    let row = agent_row(&fresh, None, &[], Some(&turn("interrupted"))).unwrap();
    assert_eq!(row["lastActivityAt"], "2026-10-07T00:00:00.000Z");
}

#[test]
fn diff_throttles_activity_only_changes_and_reports_changes_and_removals() {
    let a = json!({"id":"a","status":"idle","lastActivityAt":"1"});
    let b = json!({"id":"b","status":"idle","lastActivityAt":"1"});
    let previous: BTreeMap<String, Value> =
        [&a, &b].iter().map(|row| (row["id"].as_str().unwrap().to_string(), (*row).clone())).collect();
    let a2 = json!({"id":"a","status":"idle","lastActivityAt":"2"});
    let next = vec![a2.clone(), json!({"id":"c","status":"working"})];

    // Throttled: the activity-only change of "a" is held back, its old row kept.
    let (events, emitted) = diff_rows(&previous, &next, &|_| false);
    let names: Vec<(&str, &Value)> = events.iter().map(|(n, d)| (n.as_str(), d)).collect();
    assert_eq!(names.len(), 2);
    assert_eq!(names[0].0, "cutex/summary/agentChanged");
    assert_eq!(names[0].1["agent"]["id"], "c");
    assert_eq!(names[1], ("cutex/summary/agentRemoved", &json!({"id":"b"})));
    assert_eq!(emitted["a"], a);

    // Due: the held-back change goes out with the current row.
    let (events, emitted) = diff_rows(&emitted, &next, &|id| id == "a");
    assert_eq!(events, vec![("cutex/summary/agentChanged".to_string(), json!({"agent": a2}))]);
    assert_eq!(emitted["a"], a2);

    // Unchanged rows emit nothing even when due; real changes ignore the throttle.
    let (events, _) = diff_rows(&emitted, &next, &|_| true);
    assert!(events.is_empty());
    let busy = vec![json!({"id":"a","status":"working","lastActivityAt":"3"}), json!({"id":"c","status":"working"})];
    let (events, _) = diff_rows(&emitted, &busy, &|_| false);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["agent"]["status"], "working");
}

#[test]
fn owner_read_survives_late_consumption_of_earlier_turns() {
    let mut projections = HashMap::new();
    let ended = |at: &str| {
        let mut event = envelope(
            "cutex.a",
            Some(json!({"method":"turn/completed","params":{"turn":{"id":"t","status":"completed"}}})),
            None,
        );
        event.received_at = at.into();
        event
    };
    reduce(&mut projections, &ended("2026-10-08T01:00:00Z"));
    assert_eq!(projections["cutex.a"].unread_turns, 1);
    // The ack is applied before a tick consumes a turn that ended before it.
    projections.get_mut("cutex.a").unwrap().mark_read("2026-10-08T01:00:05.000Z");
    reduce(&mut projections, &ended("2026-10-08T01:00:04.500Z"));
    assert_eq!(projections["cutex.a"].unread_turns, 0);
    // A later turn is unread again; an older read never moves the marker back.
    reduce(&mut projections, &ended("2026-10-08T01:00:06Z"));
    assert_eq!(projections["cutex.a"].unread_turns, 1);
    projections.get_mut("cutex.a").unwrap().mark_read("2026-10-08T00:00:00Z");
    assert_eq!(projections["cutex.a"].read_at.as_deref(), Some("2026-10-08T01:00:05.000Z"));
}

#[test]
fn hub_queues_reads_for_the_single_writer() {
    let dir = TempDir::new("reads");
    let hub = Hub::new(Journal::open(&dir.0.join("summary.sqlite3")).unwrap());
    hub.dirty.store(false, Ordering::SeqCst);
    hub.queue_read("cutex.a".into(), "2026-10-08T01:00:00Z".into());
    assert!(hub.dirty.load(Ordering::SeqCst));
    assert_eq!(hub.take_reads(), vec![("cutex.a".to_string(), "2026-10-08T01:00:00Z".to_string())]);
    assert!(hub.take_reads().is_empty());
}

#[test]
fn journal_cursor_resolution_and_expiry() {
    let dir = TempDir::new("journal");
    let path = dir.0.join("summary").join("summary.sqlite3");
    let mut journal = Journal::open(&path).unwrap();
    let stream_id = journal.stream_id();
    assert!(!stream_id.is_empty());
    // Empty journal: live = 0, "0" valid, anything else expired.
    assert_eq!(journal.resolve_cursor(None, None).unwrap(), Ok(0));
    assert_eq!(journal.resolve_cursor(Some("0"), None).unwrap(), Ok(0));
    assert!(journal.resolve_cursor(Some("5"), None).unwrap().is_err());
    let events: Vec<PendingSummaryEvent> =
        (0..3).map(|i| ("cutex/summary/asksChanged".to_string(), json!({"count": i}))).collect();
    let journaled = journal.commit(&HashMap::new(), None, &[], &events).unwrap();
    assert_eq!(journaled.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(journal.resolve_cursor(None, None).unwrap(), Ok(3));
    assert_eq!(journal.resolve_cursor(Some("1"), Some(&stream_id)).unwrap(), Ok(1));
    assert_eq!(journal.page_after(1, 10).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
    assert!(journal.resolve_cursor(Some("1"), Some("other-stream")).unwrap().is_err());
    assert!(journal.resolve_cursor(Some("nope"), None).unwrap().is_err());
    // Age out the first two events: cursor 0 (needs seq 1) is now expired.
    journal.db.execute("UPDATE events SET at = 0 WHERE seq <= 2", []).unwrap();
    journal.commit(&HashMap::new(), None, &[], &[]).unwrap();
    let expired = journal.resolve_cursor(Some("0"), None).unwrap().unwrap_err();
    assert_eq!(expired["earliestCursor"], "2");
    assert_eq!(expired["latestCursor"], "3");
    assert_eq!(journal.resolve_cursor(Some("2"), None).unwrap(), Ok(2));
    // Projections and rows persist across reopen; the stream id is stable.
    let projections = HashMap::from([("cutex.a".to_string(), AgentProjection { unread_turns: 4, ..Default::default() })]);
    let rows = BTreeMap::from([("cutex.a".to_string(), json!({"id":"cutex.a"}))]);
    journal.commit(&projections, Some(RowsUpdate { previous: &BTreeMap::new(), next: &rows }), &[("asksCount", "1".into())], &[]).unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    assert_eq!(journal.stream_id(), stream_id);
    assert_eq!(journal.load_projections().unwrap()["cutex.a"].unread_turns, 4);
    assert_eq!(journal.load_rows().unwrap(), rows);
    assert_eq!(journal.meta("asksCount").unwrap().as_deref(), Some("1"));
    // Sequence numbers never go back after pruning.
    assert_eq!(journal.assigned_seq().unwrap(), 3);
}

#[test]
fn hub_publishes_and_marks_slow_subscriber_lagged_and_stream_writes_frames() {
    let dir = TempDir::new("hub");
    let hub = Hub::new(Journal::open(&dir.0.join("summary.sqlite3")).unwrap());
    let (fast_tx, fast_rx) = mpsc::sync_channel(16);
    let (slow_tx, slow_rx) = mpsc::sync_channel(1);
    let fast_lagged = Arc::new(AtomicBool::new(false));
    let slow_lagged = Arc::new(AtomicBool::new(false));
    hub.subscribers.lock().unwrap().push((fast_tx, fast_lagged.clone()));
    hub.subscribers.lock().unwrap().push((slow_tx, slow_lagged.clone()));
    let events: Vec<PendingSummaryEvent> = vec![
        ("cutex/summary/agentRemoved".into(), json!({"id":"cutex.x"})),
        ("cutex/summary/approvalsChanged".into(), json!({"count":2})),
    ];
    hub.append(&HashMap::new(), None, &[], &events).unwrap();
    assert!(!fast_lagged.load(Ordering::SeqCst));
    assert!(slow_lagged.load(Ordering::SeqCst));
    assert_eq!(hub.subscribers.lock().unwrap().len(), 1);

    // Healthy stream: backlog + live events, then a clean end without a lagged frame.
    hub.subscribers.lock().unwrap().clear();
    let backlog = hub.lock().page_after(0, 10).unwrap();
    let mut out = Vec::new();
    stream_summary(&mut out, backlog, fast_rx, fast_lagged, RevocationWatch::new(None)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text,
        "id: 1\nevent: cutex/summary/agentRemoved\ndata: {\"id\":\"cutex.x\"}\n\n\
         id: 2\nevent: cutex/summary/approvalsChanged\ndata: {\"count\":2}\n\n"
    );

    // Lagged stream: the one delivered event, then the lagged error frame.
    let mut out = Vec::new();
    stream_summary(&mut out, Vec::new(), slow_rx, slow_lagged, RevocationWatch::new(None)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("id: 1\nevent: cutex/summary/agentRemoved\n"), "{text}");
    assert!(text.contains("\"code\":\"lagged\""), "{text}");
    assert!(text.contains("\"lastDeliveredCursor\":\"1\""), "{text}");
}

#[test]
fn owner_notify_records_become_needs_and_phone_ones_message_the_owner() {
    use crate::config::owner_notify::OwnerNotifySettings;
    use crate::owner_notify::store::{Level, NewNotification, OwnerNotifyState};
    let mut state = OwnerNotifyState::default();
    let now = chrono::Utc::now();
    let new = |level, needs_reply| NewNotification {
        agent_id: "cutex.a".into(),
        agent_name: None,
        text: "Schema   question".into(),
        level,
        needs_reply,
        delegation: None,
        job: None,
        turn_id: None,
    };
    let info = state.create(new(Level::Info, false), &OwnerNotifySettings::default(), now);
    let urgent = state.create(new(Level::Urgent, true), &OwnerNotifySettings::default(), now);
    let sources = NeedSources {
        owner_notifications: state.notifications.values().cloned().collect(),
        ..Default::default()
    };
    let needs = needs_by_agent(&sources);
    let mut kinds: Vec<(&str, &str)> = needs["cutex.a"]
        .iter()
        .map(|need| (need["kind"].as_str().unwrap(), need["id"].as_str().unwrap()))
        .collect();
    kinds.sort();
    // Default tables: an info message is a record (no need).
    assert_eq!(kinds, vec![("ownerReply", urgent.notification_id.as_str())]);
    assert_eq!(needs["cutex.a"][0]["title"], "Schema question");
    // Info goes to the desktop only: no messageToOwner; urgent includes the phone.
    let mut projections = HashMap::new();
    let created = |record: &crate::owner_notify::store::OwnerNotification| {
        envelope("cutex.a", None, Some((crate::owner_notify::CREATED_EVENT, json!({"notification": record}))))
    };
    assert!(reduce(&mut projections, &created(&info)).is_none());
    let (name, data) = reduce(&mut projections, &created(&urgent)).unwrap();
    assert_eq!(name, "cutex/summary/messageToOwner");
    assert_eq!(data["id"], "cutex.a");
    assert_eq!(data["notificationId"], urgent.notification_id.as_str());
    assert_eq!(data["preview"], "Schema question");
    assert_eq!(data["level"], "urgent");
    assert_eq!(data["needsReply"], true);
}

#[test]
fn system_notices_are_records_not_needs_or_phone_messages() {
    use crate::config::owner_notify::OwnerNotifySettings;
    use crate::owner_notify::store::{Level, NewNotification, OwnerNotifyState, Origin};
    let mut state = OwnerNotifyState::default();
    let now = chrono::Utc::now();
    let notice = state.create_with_origin(
        NewNotification {
            agent_id: "cutex.live".into(),
            agent_name: None,
            text: "消息没能送达（tsa_1）：event_too_large".into(),
            level: Level::Attention,
            needs_reply: false,
            delegation: None,
            job: None,
            turn_id: None,
        },
        Origin::System,
        &OwnerNotifySettings::default(),
        now,
    );
    assert!(notice.channels.is_empty());
    // A record written before notices went quiet still lists desktop + phone.
    let mut legacy = notice.clone();
    legacy.notification_id = "ntf_legacy".into();
    legacy.level = Level::Attention;
    legacy.channels = vec!["desktop".into(), "phone".into()];
    // (and, being older than notification levels, has no decided actions)
    legacy.actions = None;
    legacy.notification_level = None;
    legacy.notification_event = None;
    let sessions = vec![session("cutex.live", true, Some("t"))];
    let sources = NeedSources { owner_notifications: vec![notice.clone(), legacy.clone()], ..Default::default() };
    assert!(needs_by_agent(&sources).is_empty());
    let synthesis = synthesize(&sessions, &HashMap::new(), &BTreeMap::new(), &sources, &HashMap::new());
    assert_eq!(synthesis.needs_you_count, 0);
    assert_eq!(synthesis.agents[0]["needs"], json!([]));
    let mut projections = HashMap::new();
    for record in [&notice, &legacy] {
        let created = envelope("cutex.live", None, Some((crate::owner_notify::CREATED_EVENT, json!({"notification": record}))));
        assert!(reduce(&mut projections, &created).is_none(), "no messageToOwner for a cutex notice");
    }
}

/// A daemon context for the summary thread: registry + offline runtimes only.
fn bench_context() -> ManagementRequestContext {
    ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unexpected runtime action"),
        adopt_saved_native: |_, _| panic!("unexpected native adoption"),
        review_agent_archive: |_, _| panic!("unexpected archive review"),
        execute_agent_archive: |_, _| panic!("unexpected archive"),
        durable_agent_candidates: |_| panic!("unexpected agent candidates"),
        import_durable_agent: |_, _| panic!("unexpected agent import"),
        load_registry: || Ok(Default::default()),
        load_runtime_status: |_| Ok(None),
        forward_native_request: |_, _| panic!("unexpected native request"),
        respond_native_server_request: |_, _, _| panic!("unexpected native response"),
        handle_user_input: |_| panic!("unexpected user input"),
        flush_user_input_queue: |_, _, _| panic!("unexpected queue flush"),
        steer_user_input_queue: |_, _, _| panic!("unexpected queue steer"),
        interrupt_active_turn: |_, _| panic!("unexpected interrupt"),
        load_bootstrap_state: |_, _| panic!("unexpected bootstrap"),
        load_latest_plan: |_| panic!("unexpected plan"),
        load_latest_token_usage: |_| panic!("unexpected usage"),
        revert_session_history: |_, _, _| panic!("unexpected revert"),
        delete_session: |_, _| panic!("unexpected session delete"),
        fork_session: |_, _, _| panic!("unexpected fork"),
        side_chat: |_, _, _| panic!("unexpected side chat"),
        open_offline_history: |_| panic!("unexpected history"),
        mutate_session: |_, _, _| panic!("unexpected session mutation"),
        online_session: |_, _, _| panic!("unexpected online request"),
        bind_project_authority: |_| panic!("unexpected project bind"),
        import_legacy_director_ownership: |_| panic!("unexpected ownership import"),
        reconcile_agent_reservation: |_| panic!("unexpected reservation"),
        list_management_projects: |_| panic!("unexpected project list"),
        read_management_project: |_, _| panic!("unexpected project read"),
        update_management_project_presentation: |_, _| panic!("unexpected project update"),
        execute_management_operator_action: |_, _| panic!("unexpected operator action"),
        execute_management_project_mutation: |_, _| panic!("unexpected project mutation"),
        create_human_agent: |_, _| panic!("unexpected agent create"),
        create_human_session: |_, _| panic!("unexpected session create"),
    }
}

/// Synthetic store for the benchmark: `sessions` agents (1 in 10 archived)
/// and `receipts` explicit-launch receipts. Never a real home.
fn bench_store(sessions: usize, receipts: usize) -> crate::session::model::CutexSessionStore {
    use crate::agent_bus::model::AgentRegistrationClass;
    use crate::session::model::{CutexSessionArchiveState, CutexSessionRecord, CutexSessionStore};
    let host = crate::platform::host::current_host_name();
    let mut store = CutexSessionStore::default();
    for index in 0..sessions {
        let name = format!("agent-{index:03}");
        let mut record = CutexSessionRecord::new_at(
            format!("cutex.{name}"),
            Some(format!("019e{index:04x}-7c1d-7a42-9a51-{index:012x}")),
            host.clone(),
            format!("/srv/projects/Project-{index:03}/workspace"),
            Some("fixture-profile".into()),
            "2026-09-01T00:00:00Z".into(),
        )
        .unwrap();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.formal_agent_name = Some(name.clone());
        record.thread_name = Some(format!("{name} thread with a realistic title"));
        record.note = Some("synthetic benchmark record; ".repeat(8));
        record.agent_groups = vec!["alpha".into(), "beta".into()];
        record.runtime_generation = index as u64 % 9;
        if index % 10 == 9 {
            record.archive_state = CutexSessionArchiveState::Retired;
        }
        store.sessions.insert(record.cutex_session_id.clone(), record);
    }
    for index in 0..receipts {
        let session = format!("cutex.agent-{:03}", index % sessions.max(1));
        let frames: Vec<Value> = (0..4)
            .map(|frame| json!({"text": format!("working {index}-{frame}"), "fg": "#a0a0a0", "bold": frame % 2 == 0}))
            .collect();
        let receipt = serde_json::from_value(json!({"kind":"runtime","receipt":{
            "action_id":format!("action-{index:05}"),"stage":"ready","claim_id":format!("claim-{index:05}"),
            "runtime_agent_id":format!("stock.agent.{index}"),"expected_generation":index as u64,"binding":null,
            "publication":null,"error":null,"updated_at":"2026-10-08T00:00:00Z",
            "review":{"subject":{"cutex_session_id":session,"formal_name":"worker","durable_sha256":"a".repeat(64),
                "authority_sha256":"a".repeat(64),"current_project_id":null,"revision":1,"runtime_generation":0},
                "contract":{"version":4,"native_id":"native-1","native_home":"/private/home",
                    "bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)},
                "configuration":{
                    "selected_projection":{"version":2,"route":"chatgpt_file",
                        "auth":{"path":"/private/auth.json","parent_device":1,"parent_inode":2,"owner":3,"account":"a".repeat(64),"api_file":null},
                        "settings":{"windows":{"status_animation":{"frames":[frames],"repeat":true,"width":24}}},
                        "catalog":null,"requires_job":false},
                    "profile_name":"alpha","profile_id":"private","inherited":false,"profile_sha256":"c".repeat(64),
                    "account_sha256":"d".repeat(64),"model":"fixture","reasoning":null,"model_provider":"private",
                    "provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses",
                        "requires_openai_auth":false,"supports_websockets":false},
                    "sandbox":"danger-full-access","approval":"never"},
                "restart":false}
        }}))
        .unwrap();
        store.explicit_launch_receipts.insert(format!("action-{index:05}"), receipt);
    }
    store
}

/// Idle cost of the hot paths on a seeded private store (default 500
/// sessions / 2000 receipts; `CUTEX_PERF_BENCH_SESSIONS` /
/// `CUTEX_PERF_BENCH_RECEIPTS` override):
/// `cargo test --release --lib summary_tests::idle_paths_bench -- --ignored --nocapture`.
/// Lines marked `perf-counter` use counters added with the PERF fixes; the
/// rest also runs on the commit before them for the comparison.
#[test]
#[ignore = "synthetic performance comparison; run explicitly with --release"]
fn idle_paths_bench() {
    use std::time::Instant;
    let env = |key: &str, default: usize| {
        std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    };
    let (sessions, receipts) = (env("CUTEX_PERF_BENCH_SESSIONS", 500), env("CUTEX_PERF_BENCH_RECEIPTS", 2000));
    let _home = crate::profiles::lifecycle::tests::TestHome::new();
    crate::session::store::save_cutex_session_store(&bench_store(sessions, receipts)).unwrap();
    let median = |runs: usize, run: &mut dyn FnMut()| {
        let mut times: Vec<f64> = (0..runs)
            .map(|_| {
                let started = Instant::now();
                run();
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        times[runs / 2]
    };
    let copies = crate::session::store::whole_store_copies_on_this_thread; // perf-counter
    let serializations = crate::management::v2::session::projection_serializations_on_this_thread; // perf-counter

    let copy_ms = median(20, &mut || {
        assert_eq!(crate::session::store::load_cutex_session_store().unwrap().sessions.len(), sessions);
    });
    let lookup_ms = median(20, &mut || {
        assert!(crate::session::store::with_cutex_session_store(|s| s.sessions.contains_key("cutex.agent-001")).unwrap());
    });
    let copies_before = copies(); // perf-counter
    let retired_ms = median(20, &mut || {
        crate::management::v2::session::retired_session_list_resource().unwrap();
    });
    eprintln!("perf-bench retired_list_copies={}", copies() - copies_before); // perf-counter

    let repository = super::super::repository::management_v2_repository().unwrap();
    let registry = crate::im::registry::ImRegistry::default();
    let list = || {
        crate::management::v2::session::session_list_resource_with_hidden(&registry, |_| Ok(None), repository, false)
            .unwrap()
    };
    assert_eq!(list()["sessions"].as_array().unwrap().len(), sessions - sessions / 10);
    let serializations_before = serializations(); // perf-counter
    let list_ms = median(20, &mut || {
        list();
    });
    eprintln!("perf-bench session_list_projection_serializations={} (20 unchanged lists)", serializations() - serializations_before); // perf-counter

    let dir = TempDir::new("bench");
    let hub = Hub::new(Journal::open(&dir.0.join("summary.sqlite3")).unwrap());
    let mut state = TickState::default();
    tick(&hub, bench_context(), &mut state).unwrap(); // initial silent pass
    tick(&hub, bench_context(), &mut state).unwrap();
    let loads_before = JOURNAL_STATE_LOADS.load(Ordering::Relaxed); // perf-counter
    let copies_before = copies(); // perf-counter
    let idle_tick_ms = median(20, &mut || {
        tick(&hub, bench_context(), &mut state).unwrap();
    });
    let full_tick_ms = median(10, &mut || {
        hub.dirty.store(true, Ordering::SeqCst);
        tick(&hub, bench_context(), &mut state).unwrap();
    });
    eprintln!("perf-bench summary_journal_state_loads={} summary_copies={} (30 ticks)", JOURNAL_STATE_LOADS.load(Ordering::Relaxed) - loads_before, copies() - copies_before); // perf-counter
    eprintln!(
        "perf-bench sessions={sessions} receipts={receipts} whole_store_copy_ms={copy_ms:.3} shared_lookup_ms={lookup_ms:.3} \
         retired_list_ms={retired_ms:.3} session_list_ms={list_ms:.3} summary_idle_tick_ms={idle_tick_ms:.3} summary_full_tick_ms={full_tick_ms:.3}"
    );
}

/// PERF-04: the summary thread reads the journal state once and then keeps
/// it; after every commit (only changed projections / rows written) the
/// kept state equals what a fresh read of the journal returns.
#[test]
fn tick_keeps_the_journal_state_it_commits() {
    let _home = crate::profiles::lifecycle::tests::TestHome::new();
    crate::session::store::save_cutex_session_store(&bench_store(6, 0)).unwrap();
    let dir = TempDir::new("tick-state");
    let hub = Hub::new(Journal::open(&dir.0.join("summary.sqlite3")).unwrap());
    let mut state = TickState::default();
    let assert_matches_journal = |state: &TickState| {
        let kept = state.persisted.as_ref().expect("state kept after a good tick");
        let fresh = Persisted::load(&hub.lock()).unwrap();
        assert_eq!(kept.projections, fresh.projections);
        assert_eq!(kept.rows, fresh.rows);
        assert_eq!(kept.checkpoint, fresh.checkpoint);
        assert_eq!(kept.initialized, fresh.initialized);
        assert_eq!((kept.approvals_count, kept.asks_count), (fresh.approvals_count, fresh.asks_count));
    };
    tick(&hub, bench_context(), &mut state).unwrap();
    assert_matches_journal(&state);
    assert_eq!(state.persisted.as_ref().unwrap().rows.len(), 6);
    hub.queue_read("cutex.agent-001".into(), "2026-10-15T00:00:00Z".into());
    tick(&hub, bench_context(), &mut state).unwrap();
    assert_matches_journal(&state);
    assert_eq!(
        state.persisted.as_ref().unwrap().projections["cutex.agent-001"].read_at.as_deref(),
        Some("2026-10-15T00:00:00Z")
    );
    // An agent leaves: its row is cleared, the others stay.
    let mut store = crate::session::store::load_cutex_session_store().unwrap();
    store.sessions.remove("cutex.agent-002");
    crate::session::store::save_cutex_session_store(&store).unwrap();
    hub.dirty.store(true, Ordering::SeqCst);
    tick(&hub, bench_context(), &mut state).unwrap();
    assert_matches_journal(&state);
    assert!(!state.persisted.as_ref().unwrap().rows.contains_key("cutex.agent-002"));
    assert_eq!(state.persisted.as_ref().unwrap().rows.len(), 5);
    let removed = hub.lock().page_after(0, 100).unwrap();
    assert!(removed.iter().any(|event| event.name == "cutex/summary/agentRemoved"
        && event.data["id"] == "cutex.agent-002"));
}
