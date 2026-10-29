use super::*;
use crate::management::listeners::RevocationWatch;
use crate::management::v2::model::{CutexMessage, EventCorrelation, EventSource, NativeMessage};

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-project-events-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn journal(&self) -> Journal {
        Journal::open(&self.0.join("journal.sqlite3")).unwrap()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeFacts {
    projects: BTreeMap<String, String>,
    threads: HashMap<String, String>,
    asks: HashMap<String, String>,
}
impl FakeFacts {
    fn new() -> Self {
        let mut facts = Self::default();
        facts.projects.insert("cutex.a".into(), "proj-a".into());
        facts.projects.insert("cutex.b".into(), "proj-b".into());
        facts.threads.insert("cutex.a".into(), "root-a".into());
        facts
    }
}
impl Facts for FakeFacts {
    fn project_of_session(&self, session: &str) -> Option<String> {
        self.projects.get(session).cloned()
    }
    fn own_thread(&self, session: &str) -> Option<String> {
        self.threads.get(session).cloned()
    }
    fn agent_name(&self, session: &str) -> Option<String> {
        self.projects.contains_key(session).then(|| format!("name-{session}"))
    }
    fn ask_project(&self, ask_id: &str) -> Option<String> {
        self.asks.get(ask_id).cloned()
    }
    fn ask_title(&self, ask_id: &str) -> Option<String> {
        self.asks.contains_key(ask_id).then(|| format!("title of {ask_id}"))
    }
}

fn envelope(session: &str, at: &str, native: Option<Value>, cutex: Option<(&str, Value)>) -> EventEnvelope {
    EventEnvelope {
        contract_version: 2,
        event_id: format!("e-{}", uuid::Uuid::new_v4()),
        cursor: format!("c2:{}", uuid::Uuid::new_v4()),
        stream_id: "s".into(),
        sequence: 1,
        received_at: at.into(),
        cutex_session_id: session.into(),
        host_id: "h".into(),
        source: if native.is_some() { EventSource::AppServer } else { EventSource::Cutex },
        sensitivity: "owner".into(),
        schema: None,
        correlation: EventCorrelation::default(),
        native: native.map(|message| NativeMessage { kind: NativeMessageKind::Notification, message }),
        cutex: cutex.map(|(method, params)| CutexMessage { method: method.into(), params }),
    }
}

fn native(session: &str, at: &str, method: &str, params: Value) -> EventEnvelope {
    envelope(session, at, Some(json!({ "method": method, "params": params })), None)
}

fn cutex(session: &str, method: &str, params: Value) -> EventEnvelope {
    envelope(session, "2026-10-23T01:00:00Z", None, Some((method, params)))
}

const AT: &str = "2026-10-23T01:00:00Z";

#[test]
fn codex_and_claude_turns_become_one_row_each_and_sub_agent_turns_do_not() {
    let facts = FakeFacts::new();
    let mut pending = Pending::default();
    let started = native("cutex.a", "2026-10-23T01:00:00Z", "turn/started", json!({"threadId": "root-a", "turn": {"id": "t1", "status": "inProgress"}}));
    assert!(normalize(&started, &facts, &mut pending).is_empty());
    let completed = native(
        "cutex.a",
        "2026-10-23T01:00:42.500Z",
        "turn/completed",
        json!({"threadId": "root-a", "turn": {"id": "t1", "status": "completed", "error": null, "items": [
            {"type": "userMessage", "content": [{"type": "text", "text": "Run the\n tests"}]},
            {"type": "commandExecution", "command": "cargo test"},
            {"type": "fileChange"},
            {"type": "reasoning"},
            {"type": "agentMessage", "text": "All green."}
        ]}}),
    );
    let rows = normalize(&completed, &facts, &mut pending);
    assert_eq!(rows.len(), 1);
    let Normalized { dedupe, row } = &rows[0];
    assert_eq!(dedupe, "turn:cutex.a:t1");
    assert_eq!(row.kind, "turn.completed");
    assert_eq!(row.project_id, "proj-a");
    assert_eq!(row.agent_name.as_deref(), Some("name-cutex.a"));
    assert_eq!(row.title.as_deref(), Some("Run the tests"));
    assert_eq!(row.detail.as_deref(), Some("All green."));
    assert_eq!(row.tool_calls, Some(2));
    assert_eq!(row.duration_ms, Some(42_500));
    assert_eq!(row.at, "2026-10-23T01:00:42.500Z");
    assert_eq!(row.parent.turn_id.as_deref(), Some("t1"));
    assert_eq!(row.source.method.as_deref(), Some("turn/completed"));
    assert!(pending.turn_starts.is_empty(), "the start is consumed");

    // Claude: durationMs on the turn; a failure shows its error.
    let mut claude_facts = FakeFacts::new();
    claude_facts.threads.clear();
    let failed = native("cutex.b", AT, "turn/completed", json!({"threadId": "x", "turn": {"id": "t9", "status": "failed", "durationMs": 1200, "error": {"message": "model overloaded"}, "items": []}}));
    let rows = normalize(&failed, &claude_facts, &mut pending);
    assert_eq!(rows[0].row.kind, "turn.failed");
    assert_eq!(rows[0].row.duration_ms, Some(1200));
    assert_eq!(rows[0].row.detail.as_deref(), Some("model overloaded"));

    // A sub-agent thread's turn under the parent session: no turn row.
    let child = native("cutex.a", AT, "turn/completed", json!({"threadId": "child-1", "turn": {"id": "t2", "status": "completed", "items": []}}));
    assert!(normalize(&child, &facts, &mut pending).is_empty());
    // Agents without a project and in-progress turns: nothing.
    let stranger = native("cutex.z", AT, "turn/completed", json!({"turn": {"id": "t3", "status": "completed"}}));
    assert!(normalize(&stranger, &facts, &mut pending).is_empty());
    let running = native("cutex.a", AT, "turn/completed", json!({"threadId": "root-a", "turn": {"id": "t4", "status": "inProgress"}}));
    assert!(normalize(&running, &facts, &mut pending).is_empty());
}

#[test]
fn sub_agents_start_and_finish_once_per_child() {
    let facts = FakeFacts::new();
    let mut pending = Pending::default();
    let activity = |kind: &str| {
        native("cutex.a", AT, "item/completed", json!({"threadId": "root-a", "turnId": "t1", "item": {
            "type": "subAgentActivity", "kind": kind, "agentThreadId": "claude:s1:task:abc", "agentPath": "/claude/abc"
        }}))
    };
    let started = normalize(&activity("started"), &facts, &mut pending);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].row.kind, "subagent.started");
    assert_eq!(started[0].row.title.as_deref(), Some("/claude/abc"));
    assert_eq!(started[0].row.parent.task_id.as_deref(), Some("abc"));
    assert_eq!(started[0].row.parent.turn_id.as_deref(), Some("t1"));
    assert!(normalize(&activity("interacted"), &facts, &mut pending).is_empty());
    let finished = normalize(&activity("interrupted"), &facts, &mut pending);
    assert_eq!(finished[0].row.kind, "subagent.finished");
    assert_eq!(finished[0].row.status.as_deref(), Some("interrupted"));
    assert_ne!(started[0].dedupe, finished[0].dedupe);
    // Codex v1 spawn: one start per receiver, the same key as an activity start.
    let spawn = native("cutex.a", AT, "item/completed", json!({"threadId": "root-a", "item": {
        "type": "collabAgentToolCall", "tool": "spawnAgent", "senderThreadId": "root-a", "receiverThreadIds": ["claude:s1:task:abc", "c2"]
    }}));
    let rows = normalize(&spawn, &facts, &mut pending);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].dedupe, started[0].dedupe);
}

#[test]
fn delegations_are_one_row_per_change_in_the_ask_project() {
    let mut facts = FakeFacts::new();
    let mut pending = Pending::default();
    let ask = json!({"askId": "ask-1", "requester": {"kind": "agent", "cutexSessionId": "cutex.a"}, "targetCutexSessionId": "cutex.b",
        "targetName": "worker", "projectId": "proj-x", "title": "Fix the build", "execution": "queued"});
    // The notice reaches both sessions: same dedupe key.
    let on_a = normalize(&cutex("cutex.a", "cutex/ask/created", json!({"ask": ask})), &facts, &mut pending);
    let on_b = normalize(&cutex("cutex.b", "cutex/ask/created", json!({"ask": ask})), &facts, &mut pending);
    assert_eq!(on_a[0].dedupe, on_b[0].dedupe);
    let row = &on_a[0].row;
    assert_eq!((row.kind.as_str(), row.project_id.as_str()), ("ask.created", "proj-x"));
    assert_eq!(row.cutex_session_id.as_deref(), Some("cutex.a"));
    assert_eq!(row.title.as_deref(), Some("Fix the build"));
    assert_eq!(row.detail.as_deref(), Some("→ worker"));
    assert_eq!(row.refs.ask_id.as_deref(), Some("ask-1"));
    // Later events use the ask's project, not the session's.
    let answered = normalize(
        &cutex("cutex.b", "cutex/ask/answered", json!({"askId": "ask-1", "answerId": "ans-1", "status": "final", "preview": "Done"})),
        &facts,
        &mut pending,
    );
    assert_eq!((answered[0].row.kind.as_str(), answered[0].row.project_id.as_str()), ("ask.answered", "proj-x"));
    assert_eq!(answered[0].row.detail.as_deref(), Some("Done"));
    // Every ask row carries the delegation's title (design 022 row table).
    assert_eq!(answered[0].row.title.as_deref(), Some("Fix the build"));
    let failed = normalize(&cutex("cutex.b", "cutex/ask/stateChanged", json!({"askId": "ask-1", "execution": "failed", "revision": 3})), &facts, &mut pending);
    assert_eq!((failed[0].row.kind.as_str(), failed[0].row.title.as_deref()), ("ask.failed", Some("Fix the build")));
    assert!(normalize(&cutex("cutex.b", "cutex/ask/stateChanged", json!({"askId": "ask-1", "execution": "running", "revision": 2})), &facts, &mut pending).is_empty());
    let closed = normalize(&cutex("cutex.a", "cutex/ask/closed", json!({"askId": "ask-1", "outcome": "accepted"})), &facts, &mut pending);
    assert_eq!((closed[0].row.kind.as_str(), closed[0].row.status.as_deref()), ("ask.closed", Some("accepted")));
    assert_eq!(closed[0].row.title.as_deref(), Some("Fix the build"));
    assert!(pending.ask_projects.is_empty() && pending.ask_titles.is_empty());
    // An ask created before the projector saw it: the ask store's project.
    facts.asks.insert("ask-old".into(), "proj-y".into());
    let late = normalize(&cutex("cutex.a", "cutex/ask/closed", json!({"askId": "ask-old", "outcome": "completed"})), &facts, &mut pending);
    assert_eq!((late[0].row.project_id.as_str(), late[0].row.title.as_deref()), ("proj-y", Some("title of ask-old")));
}

#[test]
fn host_records_carry_their_own_project() {
    let facts = FakeFacts::new();
    let mut pending = Pending::default();
    let host = super::super::services::HOST_EVENT_SESSION_ID;
    let approval = |reason: &str| {
        cutex(host, "cutex/approvals/changed", json!({"reason": reason, "approval": {
            "approvalId": "ap-1", "kind": "command", "requesterCutexSessionId": "cutex.a", "requesterName": "Eva",
            "projectId": "proj-z", "summary": "rm -rf build", "state": "pending"
        }}))
    };
    let requested = normalize(&approval("requested"), &facts, &mut pending);
    assert_eq!((requested[0].row.kind.as_str(), requested[0].row.project_id.as_str()), ("approval.requested", "proj-z"));
    let decided = normalize(&approval("denied"), &facts, &mut pending);
    assert_eq!((decided[0].row.kind.as_str(), decided[0].row.status.as_deref()), ("approval.decided", Some("denied")));
    assert!(normalize(&approval("consumed"), &facts, &mut pending).is_empty());

    let artifact = |scope: Value, new_version: bool| {
        cutex(host, "cutex/artifact/published", json!({"artifactId": "art-1", "version": 2, "scope": scope, "name": "report.md",
            "kind": "document", "newVersion": new_version, "producer": {"kind": "agent", "sessionId": "cutex.b", "turnId": "t7"}}))
    };
    let scoped = normalize(&artifact(json!({"kind": "project", "id": "proj-q"}), true), &facts, &mut pending);
    assert_eq!((scoped[0].row.project_id.as_str(), scoped[0].row.title.as_deref()), ("proj-q", Some("report.md")));
    assert_eq!(scoped[0].row.parent.turn_id.as_deref(), Some("t7"));
    let by_producer = normalize(&artifact(json!({"kind": "session", "id": "cutex.b"}), true), &facts, &mut pending);
    assert_eq!(by_producer[0].row.project_id, "proj-b");
    assert!(normalize(&artifact(json!({"kind": "project", "id": "proj-q"}), false), &facts, &mut pending).is_empty());

    let workflow = |state: &str| {
        cutex(host, "cutex/workflow/run/changed", json!({"runId": "run-1", "state": state, "name": "nightly", "authorCutexSessionId": "cutex.a"}))
    };
    assert!(normalize(&workflow("running"), &facts, &mut pending).is_empty());
    let done = normalize(&workflow("completed"), &facts, &mut pending);
    assert_eq!((done[0].row.kind.as_str(), done[0].row.project_id.as_str()), ("workflow.run", "proj-a"));

    let notify = normalize(
        &cutex("cutex.a", crate::owner_notify::CREATED_EVENT, json!({"notification": {"notificationId": "n-1", "agentId": "cutex.a", "text": "Need a key", "level": "needsYou"}})),
        &facts,
        &mut pending,
    );
    assert_eq!((notify[0].row.kind.as_str(), notify[0].row.title.as_deref()), ("ownerNotify.created", Some("Need a key")));

    let job = normalize(
        &cutex("cutex.b", "cutex/jobService/terminalObserved", json!({"eventId": "jev-1", "jobId": "job-9", "terminalStatus": "exited",
            "targetCutexSessionId": "cutex.b", "facts": {"exitCode": 1, "execution": {"observedRunDurationMillis": 5000}}})),
        &facts,
        &mut pending,
    );
    assert_eq!(job[0].row.kind, "job.finished");
    assert_eq!((job[0].row.detail.as_deref(), job[0].row.duration_ms), (Some("exit 1"), Some(5000)));
    assert_eq!(job[0].dedupe, "job:jev-1");

    let offline = normalize(&cutex("cutex.a", "cutex/runtime/failed", json!({"status": "failed", "reason": "exited"})), &facts, &mut pending);
    assert_eq!((offline[0].row.kind.as_str(), offline[0].row.status.as_deref()), ("runtime.offline", Some("failed")));
    // Our own announcements and unrelated events: nothing.
    assert!(normalize(&cutex(host, APPENDED_EVENT, json!({"projectId": "proj-a", "cursor": "pe:1"})), &facts, &mut pending).is_empty());
    assert!(normalize(&cutex("cutex.a", "cutex/userInput/submitted", json!({})), &facts, &mut pending).is_empty());
}

#[test]
fn membership_changes_are_rows_in_both_projects() {
    let facts = FakeFacts::new();
    let previous = BTreeMap::from([("cutex.a".to_string(), "proj-a".to_string()), ("cutex.b".to_string(), "proj-b".to_string())]);
    let next = BTreeMap::from([("cutex.a".to_string(), "proj-b".to_string()), ("cutex.c".to_string(), "proj-a".to_string())]);
    let rows = membership_rows(&previous, &next, AT, &facts);
    let mut kinds: Vec<(String, String, String)> = rows
        .iter()
        .map(|n| (n.row.kind.clone(), n.row.cutex_session_id.clone().unwrap(), n.row.project_id.clone()))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            ("member.joined".into(), "cutex.a".into(), "proj-b".into()),
            ("member.joined".into(), "cutex.c".into(), "proj-a".into()),
            ("member.left".into(), "cutex.a".into(), "proj-a".into()),
            ("member.left".into(), "cutex.b".into(), "proj-b".into()),
        ]
    );
    assert!(rows.iter().all(|n| n.row.source.method.is_none()));
    assert!(membership_rows(&next, &next, AT, &facts).is_empty());
}

fn row(project: &str, session: &str, kind: &str, n: usize) -> Normalized {
    Normalized {
        dedupe: format!("{project}:{kind}:{n}"),
        row: ProjectEvent {
            at: chrono::Utc::now().to_rfc3339(),
            project_id: project.into(),
            cutex_session_id: Some(session.into()),
            kind: kind.into(),
            title: Some(format!("#{n}")),
            ..ProjectEvent::default()
        },
    }
}

fn titles(page: &Page) -> Vec<String> {
    page.events.iter().map(|e| e.title.clone().unwrap()).collect()
}

#[test]
fn journal_pages_newest_first_filters_and_dedupes() {
    let dir = TempDir::new("page");
    let mut journal = dir.journal();
    let mut rows = Vec::new();
    for n in 0..10 {
        rows.push(row("proj-a", if n % 2 == 0 { "cutex.a" } else { "cutex.b" }, if n % 3 == 0 { "ask.created" } else { "turn.completed" }, n));
    }
    rows.push(row("proj-b", "cutex.a", "turn.completed", 99));
    let written = journal.commit(&rows, &[]).unwrap();
    assert_eq!(written.len(), 11);
    assert_eq!(written[0].1.cursor, "pe:1");
    // Duplicates are ignored (and not reported as written).
    assert!(journal.commit(&rows[..2], &[]).unwrap().is_empty());

    let all = |_: &ProjectEvent| true;
    let query = PageQuery { project_id: "proj-a".into(), limit: 4, ..PageQuery::default() };
    let first = journal.page(&query, &mut { all }).unwrap();
    assert_eq!(titles(&first), ["#9", "#8", "#7", "#6"]);
    assert!(first.has_more);
    let before = parse_cursor(&first.events[3].cursor).unwrap();
    let second = journal.page(&PageQuery { before: Some(before), ..query.clone() }, &mut { all }).unwrap();
    assert_eq!(titles(&second), ["#5", "#4", "#3", "#2"]);
    let last = journal.page(&PageQuery { before: Some(3), ..query.clone() }, &mut { all }).unwrap();
    assert_eq!((titles(&last), last.has_more), (vec!["#1".to_string(), "#0".to_string()], false));
    // after: newer rows, oldest first.
    let newer = journal.page(&PageQuery { after: Some(7), ..query.clone() }, &mut { all }).unwrap();
    assert_eq!((titles(&newer), newer.has_more), (vec!["#7".to_string(), "#8".to_string(), "#9".to_string()], false));
    // kinds (prefix) and sessionId.
    let asks = journal.page(&PageQuery { kinds: vec!["ask".into()], limit: 10, ..query.clone() }, &mut { all }).unwrap();
    assert_eq!(titles(&asks), ["#9", "#6", "#3", "#0"]);
    let of_b = journal.page(&PageQuery { session_id: Some("cutex.b".into()), limit: 10, ..query.clone() }, &mut { all }).unwrap();
    assert_eq!(titles(&of_b), ["#9", "#7", "#5", "#3", "#1"]);
    // Hidden rows do not count towards the limit; hasMore only for visible rows.
    let mut only_a = |row: &ProjectEvent| row.cutex_session_id.as_deref() == Some("cutex.a");
    let visible = journal.page(&PageQuery { limit: 2, ..query.clone() }, &mut only_a).unwrap();
    assert_eq!((titles(&visible), visible.has_more), (vec!["#8".to_string(), "#6".to_string()], true));
    let mut only_nine = |row: &ProjectEvent| row.title.as_deref() == Some("#9");
    let single = journal.page(&PageQuery { limit: 1, ..query.clone() }, &mut only_nine).unwrap();
    assert_eq!((titles(&single), single.has_more), (vec!["#9".to_string()], false));
}

#[test]
fn every_row_is_kept_and_old_cursors_stay_valid() {
    // Owner decision 2026-10-23: no age limit, no row cap.
    let dir = TempDir::new("keep");
    let mut journal = dir.journal();
    let mut old = row("proj-a", "cutex.a", "turn.completed", 0);
    old.row.at = "2020-01-01T00:00:00Z".into();
    let mut rows = vec![old];
    rows.extend((1..60_005).map(|n| row("proj-a", "cutex.a", "turn.completed", n)));
    journal.commit(&rows, &[]).unwrap();
    journal.commit(&[row("proj-b", "cutex.b", "turn.completed", 0)], &[]).unwrap();
    let count: i64 = journal.db.query_row("SELECT COUNT(*) FROM events WHERE project_id='proj-a'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 60_005, "more than the old 50 000 cap, including a 2020 row");
    assert_eq!(journal.floor("proj-a").unwrap(), 0);
    let all = |_: &ProjectEvent| true;
    let oldest = journal.page(&PageQuery { project_id: "proj-a".into(), after: Some(0), limit: 2, ..PageQuery::default() }, &mut { all }).unwrap();
    assert_eq!(titles(&oldest), ["#0", "#1"]);
    assert!(journal.check_cursor("proj-a", 1, true, None).unwrap().is_ok(), "the first cursor never expires");
    // Unknown (future / other stream) cursors still fail.
    assert!(journal.check_cursor("proj-a", 60_007, true, None).unwrap().is_err());
    assert!(journal.check_cursor("proj-a", 4, true, Some("another-stream")).unwrap().is_err());
    assert_eq!(journal.assigned_seq().unwrap(), 60_006);
}

fn page_of(events: Vec<EventEnvelope>, has_more: bool) -> super::super::model::EventPage {
    serde_json::from_value(json!({
        "contractVersion": 2, "hostId": "h", "streamId": "s", "scope": {"cutexSessionId": null},
        "events": events, "nextCursor": null,
        "checkpoint": {"streamId": "s", "sequence": 0, "cursor": null},
        "hasMore": has_more, "scannedCount": 0
    }))
    .unwrap()
}

#[test]
fn first_pass_backfills_and_an_expired_position_is_a_gap() {
    let facts = FakeFacts::new();
    let turn = |id: &str| native("cutex.a", AT, "turn/completed", json!({"threadId": "root-a", "turn": {"id": id, "status": "completed", "items": []}}));
    let mut persisted = Persisted::default();
    let events = vec![turn("t1"), cutex(super::super::services::HOST_EVENT_SESSION_ID, APPENDED_EVENT, json!({"projectId": "proj-a"}))];
    let last_cursor = events[1].cursor.clone();
    let mut pages = vec![Some(page_of(events, false))].into_iter();
    let pass = project_pass(|_| Ok(pages.next().flatten()), &mut persisted, &facts).unwrap();
    assert_eq!(pass.rows.len(), 1, "own announcements are skipped");
    assert!(pass.rows[0].row.backfilled);
    assert!(pass.caught_up && !pass.gap);
    assert_eq!(pass.position, Some(("s".to_string(), last_cursor)));

    // Initialized: live rows; the checkpoint expired → gap, then from the start.
    persisted.initialized = true;
    persisted.checkpoint = pass.position;
    let mut calls = 0;
    let pass = project_pass(
        |position| {
            calls += 1;
            Ok(if position.is_some() { None } else { Some(page_of(vec![turn("t2")], false)) })
        },
        &mut persisted,
        &facts,
    )
    .unwrap();
    assert_eq!(calls, 2);
    assert!(pass.gap);
    assert!(!pass.rows[0].row.backfilled);
    let gaps = gap_rows(&BTreeSet::from(["proj-a".to_string()]), AT);
    assert_eq!((gaps[0].row.kind.as_str(), gaps[0].row.project_id.as_str()), ("journal.gap", "proj-a"));
}

#[test]
fn stream_writes_backlog_live_rows_and_lagged() {
    let event = |seq: i64, session: &str| {
        let mut row = row("proj-a", session, "turn.completed", seq as usize).row;
        row.cursor = format_cursor(seq);
        (seq, row)
    };
    let (sender, receiver) = mpsc::sync_channel(4);
    sender.send(event(2, "cutex.a")).unwrap(); // already in the backlog
    sender.send(event(3, "cutex.b")).unwrap(); // hidden
    sender.send(event(4, "cutex.a")).unwrap();
    let lagged = Arc::new(AtomicBool::new(true));
    drop(sender);
    let mut out = Vec::new();
    let mut visible = |row: &ProjectEvent| row.cutex_session_id.as_deref() == Some("cutex.a");
    stream_rows(&mut out, vec![event(1, "cutex.a"), event(2, "cutex.a")], receiver, lagged, RevocationWatch::new(None), &mut visible).unwrap();
    let text = String::from_utf8(out).unwrap();
    let ids: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("id: ")).collect();
    assert_eq!(ids, ["pe:1", "pe:2", "pe:4"]);
    assert!(text.contains(&format!("event: {STREAM_EVENT}\n")));
    assert!(text.contains("lagged"), "{text}");
    assert!(text.contains("pe:4"));
}

#[test]
fn routes_and_cursors_parse_exactly() {
    assert_eq!(parse_route("/v2/projects/proj-a/events"), Some(Route::Events("proj-a".into())));
    assert_eq!(parse_route("/v2/projects/proj%3Aa/events/stream"), Some(Route::Stream("proj:a".into())));
    for bad in ["/v2/projects/proj-a/events/x", "/v2/projects//events", "/v2/projects/a%2Fb/events", "/v2/projects/proj-a", "/v2/projects/proj-a/memory"] {
        assert_eq!(parse_route(bad), None, "{bad}");
    }
    assert!(!super::super::workspace::is_workspace_path("/v2/projects/proj-a/events"));
    assert_eq!(parse_cursor("pe:42"), Some(42));
    for bad in ["42", "pe:", "pe:-1", "c2:42", "pe:x"] {
        assert_eq!(parse_cursor(bad), None, "{bad}");
    }
}

#[test]
fn rows_serialize_in_the_documented_shape() {
    let mut row = row("proj-a", "cutex.a", "turn.completed", 1).row;
    row.cursor = "pe:1".into();
    let value = serde_json::to_value(&row).unwrap();
    for key in ["cursor", "at", "projectId", "cutexSessionId", "agentName", "kind", "status", "title", "detail", "parent", "refs", "source"] {
        assert!(value.get(key).is_some(), "{key}: {value}");
    }
    assert_eq!(value["refs"]["askId"], Value::Null);
    assert_eq!(value["parent"]["turnId"], Value::Null);
    assert!(value.get("backfilled").is_none() && value.get("durationMs").is_none());
    super::super::schema_export::assert_valid("project-events", "projectEvent", &value);
    let mut full = row.clone();
    (full.duration_ms, full.tool_calls, full.backfilled) = (Some(5), Some(2), true);
    super::super::schema_export::assert_valid("project-events", "projectEvent", &serde_json::to_value(&full).unwrap());
    super::super::schema_export::assert_valid("project-events", "appended", &json!({"projectId": "proj-a", "cursor": "pe:1"}));
    assert_eq!(serde_json::from_value::<ProjectEvent>(value).unwrap(), row);
}

/// One request through the page handler over a loopback socket pair.
fn get(path: &str, visible: &mut dyn FnMut(&ProjectEvent) -> bool) -> (u16, Value) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    let request = SimpleHttpRequest { method: "GET".into(), path: path.into(), headers: Default::default(), body: Vec::new() };
    let route = parse_route(path.split('?').next().unwrap()).unwrap();
    handle(&mut server, &request, &route, visible).unwrap();
    drop(server);
    let mut text = String::new();
    client.read_to_string(&mut text).unwrap();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn page_route_answers_with_cursors_filters_and_errors() {
    let dir = TempDir::new("http");
    let hub = Arc::new(Hub::new(dir.journal()));
    assert!(HUB.set(hub.clone()).is_ok(), "only this test installs the hub");
    let rows: Vec<Normalized> = (0..3).map(|n| row("proj-a", if n == 1 { "cutex.b" } else { "cutex.a" }, "turn.completed", n)).collect();
    hub.append(&rows, &[]).unwrap();
    let all = |_: &ProjectEvent| true;

    let (status, body) = get("/v2/projects/proj-a/events?limit=2", &mut { all });
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["order"], "newestFirst");
    assert_eq!(body["cursor"], "pe:3");
    assert_eq!(body["earliestCursor"], "pe:0");
    assert_eq!(body["hasMore"], true);
    assert_eq!(body["nextCursor"], "pe:2");
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
    assert_eq!(body["events"][0]["cursor"], "pe:3");
    assert_eq!(body["streamId"], hub.lock().stream_id());
    super::super::schema_export::assert_valid("project-events", "projectEventPage", &body);

    let (_, older) = get("/v2/projects/proj-a/events?before=pe:2", &mut { all });
    assert_eq!(older["events"][0]["title"], "#0");
    assert_eq!((&older["hasMore"], &older["nextCursor"]), (&json!(false), &Value::Null));
    super::super::schema_export::assert_valid("project-events", "projectEventPage", &older);
    let (_, newer) = get("/v2/projects/proj-a/events?after=pe:1", &mut { all });
    assert_eq!((newer["order"].as_str(), newer["events"][0]["title"].as_str()), (Some("oldestFirst"), Some("#1")));
    // Percent-encoded, as URLSearchParams sends them (the desktop client).
    let (status, encoded) = get("/v2/projects/proj-a/events?after=pe%3A1&kinds=turn%2Cask", &mut { all });
    assert_eq!(status, 200, "{encoded}");
    assert_eq!(encoded["order"], "oldestFirst");
    let mut only_a = |row: &ProjectEvent| row.cutex_session_id.as_deref() == Some("cutex.a");
    let (_, filtered) = get("/v2/projects/proj-a/events", &mut only_a);
    assert_eq!(filtered["events"].as_array().unwrap().len(), 2);
    let (_, by_session) = get("/v2/projects/proj-a/events?sessionId=cutex.b", &mut { all });
    assert_eq!(by_session["events"].as_array().unwrap().len(), 1);
    let (_, other) = get("/v2/projects/proj-z/events", &mut { all });
    assert_eq!(other["events"], json!([]));

    let (status, body) = get("/v2/projects/proj-a/events?after=pe:9", &mut { all });
    assert_eq!((status, body["error"]["code"].as_str()), (409, Some("cursor_expired")), "{body}");
    assert_eq!(body["error"]["details"]["latestCursor"], "pe:3");
    for bad in ["?after=3", "?kinds=turn,bogus", "?limit=0", "?limit=501", "?before=pe:1&after=pe:1"] {
        let (status, body) = get(&format!("/v2/projects/proj-a/events{bad}"), &mut { all });
        assert_eq!(status, 400, "{bad}: {body}");
    }
}

fn claude_task(session: &str, task: Value) -> EventEnvelope {
    cutex(session, CLAUDE_TASK_EVENT, json!({"threadId": "root-a", "task": task}))
}

#[test]
fn claude_background_commands_start_and_finish_and_runtimes_reset_running_tasks() {
    let facts = FakeFacts::new();
    let mut pending = Pending::default();
    let bash = |status: &str| {
        claude_task("cutex.a", json!({"taskId": "b1", "turnId": "t1", "taskType": "local_bash", "status": status,
            "description": "Run the test suite", "command": "cargo test", "usage": {"duration_ms": 9000}}))
    };
    let started = normalize(&bash("pending"), &facts, &mut pending);
    assert_eq!(started.len(), 1);
    let row = &started[0].row;
    assert_eq!((row.kind.as_str(), row.status.as_deref()), ("background.started", Some("pending")));
    assert_eq!((row.title.as_deref(), row.detail.as_deref()), (Some("Run the test suite"), Some("cargo test")));
    assert_eq!((row.parent.task_id.as_deref(), row.parent.turn_id.as_deref()), (Some("b1"), Some("t1")));
    assert!(normalize(&bash("running"), &facts, &mut pending).is_empty(), "one start per task");
    assert_eq!(pending.running_tasks["cutex.a|b1"]["status"], "running");
    assert_eq!(pending.running_tasks["cutex.a|b1"]["startedAt"], "2026-10-23T01:00:00.000Z");
    let finished = normalize(&bash("failed"), &facts, &mut pending);
    assert_eq!((finished[0].row.kind.as_str(), finished[0].row.duration_ms), ("background.finished", Some(9000)));
    assert_ne!(finished[0].dedupe, started[0].dedupe);
    assert!(pending.running_tasks.is_empty());

    // Sub-agents: tracked as running, but their history is subagent.* rows.
    let agent = claude_task("cutex.a", json!({"taskId": "a1", "taskType": "local_agent", "status": "running", "description": "Explore"}));
    assert!(normalize(&agent, &facts, &mut pending).is_empty());
    assert!(pending.running_tasks.contains_key("cutex.a|a1"));
    let offline = offline_claude_items(&pending, "cutex.a");
    assert_eq!(offline.len(), 1);
    assert_eq!((offline[0]["kind"].as_str(), offline[0]["state"].as_str()), (Some("subagent"), Some("unknown")));
    assert_eq!(offline[0]["stop"], Value::Null);
    super::super::schema_export::assert_valid("project-events", "backgroundItem", &offline[0]);
    // A new runtime generation forgets the old one's tasks.
    normalize(&cutex("cutex.a", "cutex/runtime/online", json!({"status": "online"})), &facts, &mut pending);
    assert!(pending.running_tasks.is_empty());
}

#[test]
fn claude_task_changes_are_recorded_once_per_status() {
    let session = format!("cutex.status-{}", uuid::Uuid::new_v4());
    assert!(claude_task_status_changed(&session, "t", "running"));
    assert!(!claude_task_status_changed(&session, "t", "running"), "progress ticks repeat the status");
    assert!(claude_task_status_changed(&session, "t", "completed"));
    assert!(claude_task_status_changed(&session, "t", "completed"), "terminal statuses are forgotten");
}

#[test]
fn background_items_come_from_runtime_replies_and_jobs() {
    let claude = json!({"result": {"tasks": [
        {"taskId": "a1", "taskType": "local_agent", "status": "running", "description": "Review the diff", "turnId": "t1"},
        {"taskId": "b1", "taskType": "local_bash", "status": "pending", "command": "npm test"},
        {"taskId": "b0", "taskType": "local_bash", "status": "completed", "command": "ls"}
    ], "source": "observedSdkLifecycle", "completeInventory": false}});
    let items = items_from_reply("cutex.a", "th-a", true, &claude).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!((items[0]["kind"].as_str(), items[0]["title"].as_str()), (Some("subagent"), Some("Review the diff")));
    assert_eq!((items[1]["kind"].as_str(), items[1]["state"].as_str(), items[1]["title"].as_str()), (Some("background"), Some("pending"), Some("npm test")));
    assert_eq!(items[1]["detail"], Value::Null);
    assert_eq!(items[0]["stop"], json!({"kind": "native", "cutexSessionId": "cutex.a", "method": "claude/tasks/stop", "params": {"threadId": "th-a", "taskId": "a1"}}));
    let codex = json!({"result": {"data": [{"itemId": "i1", "processId": "p7", "command": "npm run dev", "cwd": "/w"}], "nextCursor": null}});
    let items = items_from_reply("cutex.b", "th-b", false, &codex).unwrap();
    assert_eq!((items[0]["id"].as_str(), items[0]["detail"].as_str()), (Some("p7"), Some("/w")));
    assert_eq!(items[0]["stop"]["method"], "thread/backgroundTerminals/terminate");
    assert_eq!(items[0]["stop"]["params"], json!({"threadId": "th-b", "processId": "p7"}));
    assert!(items_from_reply("cutex.b", "th-b", false, &json!({"error": {"code": -32601}})).is_none());

    let job = job_item(&json!({"jobId": "job-1", "sessionId": "cutex.a", "state": "running", "createdAt": 1_792_000_000, "revision": 4, "command": "make"}));
    assert_eq!((job["kind"].as_str(), job["title"].as_str(), job["startedAt"].as_str()), (Some("job"), Some("make"), Some("2026-10-14T17:46:40.000Z")));
    assert_eq!(job["stop"], json!({"kind": "job", "jobId": "job-1", "expectedRevision": 4}));

    let mut body = json!({"projectId": "p", "generatedAt": "2026-10-23T00:00:00.000Z", "jobsAvailable": true, "agents": [
        {"cutexSessionId": "cutex.a", "agentName": "A", "backend": "claude", "runtime": "online", "complete": true, "items": [items_from_reply("cutex.a", "th-a", true, &claude).unwrap()[0].clone(), job]},
        {"cutexSessionId": "cutex.b", "agentName": "B", "backend": "codex", "runtime": "online", "complete": true, "items": items},
    ]});
    super::super::schema_export::assert_valid("project-events", "background", &body);
    let mut owner = body.clone();
    filter_background(&mut owner, &mut |_| true, true);
    assert_eq!(owner, body);
    filter_background(&mut body, &mut |session| session == "cutex.a", false);
    assert_eq!(body["agents"].as_array().unwrap().len(), 1);
    assert!(body["agents"][0]["items"].as_array().unwrap().iter().all(|item| item["stop"].is_null()));
    super::super::schema_export::assert_valid("project-events", "background", &body);
    assert_eq!(parse_route("/v2/projects/proj-a/background"), Some(Route::Background("proj-a".into())));
}

#[test]
fn kinds_and_sessions_read_through_indexes() {
    let dir = TempDir::new("plan");
    let journal = dir.journal();
    let plan = |query: &PageQuery, kind: Option<&str>| {
        // The statement select_kind builds, with literal values.
        let mut sql = format!("EXPLAIN QUERY PLAN SELECT seq, row FROM events WHERE project_id='{}' AND seq < 9", query.project_id);
        if let Some(session) = &query.session_id {
            sql.push_str(&format!(" AND session_id='{session}'"));
        }
        if let Some(kind) = kind {
            sql.push_str(&format!(" AND {FAMILY_SQL}='{kind}'"));
        }
        sql.push_str(" ORDER BY seq DESC LIMIT 10");
        let mut statement = journal.db.prepare(&sql).unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(3)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap().join(" | ")
    };
    let project = PageQuery { project_id: "proj-a".into(), ..PageQuery::default() };
    let session = PageQuery { session_id: Some("cutex.a".into()), ..project.clone() };
    for (query, kind, index) in [
        (&project, None, "events_project "),
        (&project, Some("ask"), "events_project_family "),
        (&session, None, "events_project_session "),
        (&session, Some("ask"), "events_project_session_family "),
    ] {
        let plan = plan(query, kind);
        assert!(format!("{plan} ").contains(index), "{kind:?} {:?}: {plan}", query.session_id);
        assert!(!plan.contains("TEMP B-TREE"), "no sort: {plan}");
    }
}

#[test]
fn rare_kinds_merge_in_order_and_an_older_journal_gets_the_indexes() {
    let dir = TempDir::new("migrate");
    let path = dir.0.join("journal.sqlite3");
    {
        // An r251 journal: no kind-prefix indexes.
        let mut journal = dir.journal();
        let rows: Vec<Normalized> = (0..300)
            .map(|n| {
                let kind = match n % 100 {
                    7 => "ask.created",
                    50 => "artifact.published",
                    _ => "turn.completed",
                };
                row("proj-a", if n % 2 == 0 { "cutex.a" } else { "cutex.b" }, kind, n)
            })
            .collect();
        journal.commit(&rows, &[]).unwrap();
        journal.db.execute_batch("DROP INDEX events_project_family; DROP INDEX events_project_session_family;").unwrap();
    }
    let journal = Journal::open(&path).unwrap();
    let indexes: Vec<String> = journal
        .db
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'events_project%family' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(indexes, ["events_project_family", "events_project_session_family"]);
    let all = |_: &ProjectEvent| true;
    let query = PageQuery { project_id: "proj-a".into(), limit: 4, kinds: vec!["artifact".into(), "ask".into()], ..PageQuery::default() };
    let first = journal.page(&query, &mut { all }).unwrap();
    assert_eq!(titles(&first), ["#250", "#207", "#150", "#107"]);
    assert!(first.has_more);
    assert_eq!(first.next, Some(108));
    let second = journal.page(&PageQuery { before: first.next, ..query.clone() }, &mut { all }).unwrap();
    assert_eq!((titles(&second), second.has_more, second.next), (vec!["#50".to_string(), "#7".to_string()], false, None));
    let oldest = journal.page(&PageQuery { after: Some(0), ..query.clone() }, &mut { all }).unwrap();
    assert_eq!(titles(&oldest), ["#7", "#50", "#107", "#150"]);
    // A repeated kind is read once.
    let twice = PageQuery { kinds: vec!["ask".into(), "ask".into()], session_id: Some("cutex.b".into()), limit: 10, ..query.clone() };
    assert_eq!(titles(&journal.page(&twice, &mut { all }).unwrap()), ["#207", "#107", "#7"]);
}

#[test]
fn a_reader_who_sees_few_rows_gets_bounded_pages_with_a_next_cursor() {
    let dir = TempDir::new("budget");
    let mut journal = dir.journal();
    let rows: Vec<Normalized> =
        (0..1_000).map(|n| row("proj-a", if n == 3 || n == 900 { "cutex.a" } else { "cutex.b" }, "turn.completed", n)).collect();
    journal.commit(&rows, &[]).unwrap();
    let mut only_a = |row: &ProjectEvent| row.cutex_session_id.as_deref() == Some("cutex.a");
    let query = PageQuery { project_id: "proj-a".into(), limit: 5, ..PageQuery::default() };
    // Walk the journal newest first, 150 rows read per page at most.
    let (mut found, mut pages, mut before) = (Vec::new(), 0, None);
    loop {
        let page = journal.page_with_budget(&PageQuery { before, ..query.clone() }, 150, &mut only_a).unwrap();
        pages += 1;
        found.extend(titles(&page));
        if !page.has_more {
            assert_eq!(page.next, None);
            break;
        }
        assert!(page.next < before.or(Some(i64::MAX)), "progress");
        before = page.next;
    }
    assert_eq!(found, ["#900", "#3"]);
    assert_eq!(pages, 7, "1000 rows, 150 read per page");
    // Oldest first likewise; a full page continues after its last event.
    let page = journal.page_with_budget(&PageQuery { after: Some(0), ..query.clone() }, 150, &mut only_a).unwrap();
    assert_eq!((titles(&page), page.has_more, page.next), (vec!["#3".to_string()], true, Some(150)));
    let mut all = |_: &ProjectEvent| true;
    let full = journal.page_with_budget(&PageQuery { limit: 3, ..query.clone() }, 1, &mut all).unwrap();
    assert_eq!((titles(&full), full.has_more, full.next), (vec!["#999".to_string(), "#998".to_string(), "#997".to_string()], true, Some(998)));
    // The default budget reads the whole small journal.
    let mut none = |_: &ProjectEvent| false;
    let empty = journal.page(&query, &mut none).unwrap();
    assert_eq!((empty.events.len(), empty.has_more, empty.next), (0, false, None));
}

/// `PE_BENCH_ROWS=1000000 cargo test --release --lib bench_big_journal -- --ignored --nocapture`
/// (release needs `CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true` for the test
/// helpers): time of every page shape on a big journal.
#[test]
#[ignore]
fn bench_big_journal() {
    use std::time::Instant;
    let n_rows: usize = std::env::var("PE_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let dir = TempDir::new("bench");
    let mut journal = dir.journal();
    let kind_of = |n: usize| if n % 10_000 == 5_000 { "ask.created" } else if n % 5 == 0 { "artifact.published" } else { "turn.completed" };
    let t = Instant::now();
    let mut n = 0;
    while n < n_rows {
        let rows: Vec<Normalized> = (n..(n + 10_000).min(n_rows))
            .map(|i| {
                let mut r = row("proj-a", &format!("cutex.s{}", i % 20), kind_of(i), i);
                r.row.detail = Some("x".repeat(120));
                r
            })
            .collect();
        journal.commit(&rows, &[]).unwrap();
        n += 10_000;
    }
    let size = std::fs::metadata(dir.0.join("journal.sqlite3")).unwrap().len();
    println!("BENCH fill {n_rows} rows: {:.1} s, file {:.0} MB", t.elapsed().as_secs_f64(), size as f64 / 1e6);
    let all = |_: &ProjectEvent| true;
    let q = |before: Option<i64>, after: Option<i64>, kinds: &[&str], session: Option<&str>| PageQuery {
        project_id: "proj-a".into(),
        before,
        after,
        limit: 100,
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        session_id: session.map(str::to_string),
    };
    let time = |label: &str, journal: &Journal, query: PageQuery, visible: &mut dyn FnMut(&ProjectEvent) -> bool| {
        let t = Instant::now();
        let page = journal.page(&query, visible).unwrap();
        println!("BENCH {label}: {} rows, has_more {}, {:.1} ms", page.events.len(), page.has_more, t.elapsed().as_secs_f64() * 1000.0);
    };
    time("newest page (owner)", &journal, q(None, None, &[], None), &mut { all });
    time("middle page (before)", &journal, q(Some(n_rows as i64 / 2), None, &[], None), &mut { all });
    time("oldest page (after=0)", &journal, q(None, Some(0), &[], None), &mut { all });
    time("one session", &journal, q(None, None, &[], Some("cutex.s7")), &mut { all });
    time("kinds=artifact (common)", &journal, q(None, None, &["artifact"], None), &mut { all });
    time("kinds=turn (very common)", &journal, q(None, None, &["turn"], None), &mut { all });
    time("kinds=ask (1 in 10 000)", &journal, q(None, None, &["ask"], None), &mut { all });
    time("kinds=ask after=0", &journal, q(None, Some(0), &["ask"], None), &mut { all });
    time("kinds=ask,artifact", &journal, q(None, None, &["ask", "artifact"], None), &mut { all });
    time("kinds=ask + one session", &journal, q(None, None, &["ask"], Some("cutex.s0")), &mut { all });
    time("kinds=turn + one session", &journal, q(None, None, &["turn"], Some("cutex.s7")), &mut { all });
    let mut one_session = |row: &ProjectEvent| row.cutex_session_id.as_deref() == Some("cutex.s3");
    time("member sees 1 of 20 sessions", &journal, q(None, None, &[], None), &mut one_session);
    let mut nothing = |_: &ProjectEvent| false;
    time("member sees nothing (worst case)", &journal, q(None, None, &[], None), &mut nothing);
    let t = Instant::now();
    journal.commit(&[row("proj-a", "cutex.s1", "turn.completed", n_rows + 1)], &[]).unwrap();
    println!("BENCH append 1 row: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    journal.db.execute_batch("DROP INDEX events_project_family; DROP INDEX events_project_session_family;").unwrap();
    drop(journal);
    let t = Instant::now();
    let _ = dir.journal();
    println!("BENCH migrate (open builds both kind indexes): {:.1} s", t.elapsed().as_secs_f64());
}
