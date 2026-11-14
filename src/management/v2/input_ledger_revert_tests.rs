use super::*;
use std::collections::HashMap;

use crate::management::v2::input_ledger::{
    Evidence, InputIntent, InputSender, NewInput, SenderKind, Stage, Strategy, Surface, Timing, DATABASE_FILE,
};
use crate::management::v2::input_ledger_owner::{plan, Step};

struct Fixture {
    root: PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-input-revert-{}", uuid::Uuid::new_v4().simple()));
        Self { ledger: InputLedger::open(root.join(DATABASE_FILE)), root }
    }

    fn go_in(&self, session: &str, id: &str, next: Next) -> Transition {
        il::capture_events(|| self.ledger.transition(session, id, Expect::ANY, next)).0.unwrap()
    }

    fn go(&self, id: &str, next: Next) -> Transition {
        self.go_in("s1", id, next)
    }

    fn get_in(&self, session: &str, id: &str) -> InputRecord {
        self.ledger.get(session, &[id.to_string()]).unwrap().records.pop().unwrap()
    }

    fn get(&self, id: &str) -> InputRecord {
        self.get_in("s1", id)
    }

    /// Retract `turns` in s1; the changed transitions and published events.
    fn retract(&self, turns: &[&str]) -> (Vec<Transition>, Vec<(String, String, Value)>) {
        let removed: Vec<String> = turns.iter().map(|turn| turn.to_string()).collect();
        let (result, events) = il::capture_events(|| retract(&self.ledger, "s1", &removed, Some("revert-1")));
        (result.unwrap(), events)
    }

    fn head(&self, session: &str) -> u64 {
        self.ledger.get(session, &[]).unwrap().state_revision
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sender(kind: SenderKind) -> InputSender {
    InputSender { kind, id: "sender".into(), name: "Sender".into(), host: None, cutex_session_id: None }
}

fn owner() -> NewInput {
    NewInput {
        kind: InputKind::Owner,
        sender: sender(SenderKind::Owner),
        intent: InputIntent { strategy: Some(Strategy::Auto), delivery_mode: None, timing: Timing::AfterTurn },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    }
}

fn note() -> NewInput {
    NewInput {
        kind: InputKind::AgentMessage,
        sender: sender(SenderKind::Agent),
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::AfterTurn },
        preview: "note".into(),
        ask_id: None,
        remote: None,
    }
}

/// Owner record `id` in `session` through sending, then `last` (with `turn`).
fn owner_to(f: &Fixture, session: &str, id: &str, last: Option<Next>) {
    f.go_in(session, id, Next::received().create(owner()));
    f.go_in(session, id, Next::sending());
    if let Some(last) = last {
        f.go_in(session, id, last);
    }
}

fn in_context(turn: &str) -> Next {
    Next::in_context(Evidence::TurnStartInput).turn(turn)
}

#[test]
fn retracts_every_bound_state_and_keeps_the_audit() {
    let f = Fixture::new();
    owner_to(&f, "s1", "ctx", Some(in_context("t2")));
    owner_to(&f, "s1", "acc", Some(Next::accepted().turn("t2").stage(Stage::Registered)));
    owner_to(&f, "s1", "unc", Some(Next::accepted().turn("t2")));
    f.go("unc", Next::uncertain("turnInterrupted"));
    owner_to(&f, "s1", "fail", Some(Next::failed("notDelivered").turn("t2")));
    owner_to(&f, "s1", "send", None);
    f.go("send", Next::sending().turn("t2"));
    // A bus note in context in the removed turn: retracted too.
    f.go("bus", Next::waiting("afterTurn").create(note()));
    f.go("bus", Next::sending());
    f.go("bus", Next::in_context(Evidence::A4Receipt).turn("t2"));
    let bound = f.ledger.bind_timeline_item("s1", "ctx", Expect::ANY, "item-ctx", None);
    il::capture_events(|| bound.map(|transition| il::publish(&transition))).0.unwrap().unwrap();
    let before = f.get("ctx");
    assert_eq!(before.surface, Surface::Timeline);
    let head = f.head("s1");

    let (transitions, events) = f.retract(&["t2", "t3"]);
    let ids: Vec<&str> = transitions.iter().map(|transition| transition.record.input_id.as_str()).collect();
    assert_eq!(ids, ["acc", "bus", "ctx", "fail", "send", "unc"]);
    // One batch: one stateRevision; one stateChanged per record.
    assert_eq!(f.head("s1"), head + 1);
    assert_eq!(events.len(), 6, "{events:?}");
    assert!(events.iter().all(|(session, method, params)| session == "s1"
        && method == il::STATE_CHANGED_EVENT && params["stateRevision"] == head + 1));
    for id in ids {
        let record = f.get(id);
        assert_eq!(record.state, InputState::Discarded, "{id}");
        assert_eq!(record.reason.as_deref(), Some(HISTORY_REVERTED), "{id}");
        assert_eq!(record.surface, Surface::None, "{id}");
        assert_eq!(record.turn_id.as_deref(), Some("t2"), "{id}");
        assert!(record.actions.is_empty(), "{id}: {:?}", record.actions);
        assert_eq!(record.details[DETAILS_KEY], json!({"actionId": "revert-1", "turnId": "t2"}), "{id}");
        assert_eq!(record.deadline, None, "{id}");
    }
    let ctx = f.get("ctx");
    assert_eq!(ctx.evidence, Some(Evidence::TurnStartInput));
    assert_eq!(ctx.timeline_item_id.as_deref(), Some("item-ctx"));
    assert_eq!(ctx.rev, before.rev + 1);
    assert_eq!(f.get("bus").evidence, Some(Evidence::A4Receipt));
}

#[test]
fn rerun_is_a_no_op() {
    let f = Fixture::new();
    owner_to(&f, "s1", "a", Some(in_context("t2")));
    assert_eq!(f.retract(&["t2"]).0.len(), 1);
    let record = f.get("a");
    let head = f.head("s1");
    let (again, events) = f.retract(&["t2"]);
    assert!(again.is_empty());
    assert!(events.is_empty());
    assert_eq!(f.head("s1"), head);
    assert_eq!(f.get("a"), record);
    assert!(f.retract(&[]).0.is_empty());
}

#[test]
fn kept_turns_unbound_records_and_other_sessions_are_untouched() {
    let f = Fixture::new();
    owner_to(&f, "s1", "kept", Some(in_context("t1")));
    owner_to(&f, "s1", "gone", Some(in_context("t2")));
    // Waiting / nextUp / pendingNotes without a turn.
    f.go("queued", Next::waiting("explicitQueue").create(NewInput {
        intent: InputIntent { strategy: Some(Strategy::Queue), delivery_mode: None, timing: Timing::AfterTurn },
        ..owner()
    }));
    f.go("passive", Next::waiting("passive").create(NewInput {
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::Passive },
        ..note()
    }));
    // A bus note left uncertain on the removed turn.
    f.go("busfail", Next::waiting("afterTurn").create(note()));
    f.go("busfail", Next::sending().turn("t2"));
    f.go("busfail", Next::accepted());
    f.go("busfail", Next::uncertain("turnInterrupted"));
    // Already terminal for another reason.
    f.go("removed", Next::waiting("explicitQueue").create(owner()).turn("t2"));
    f.go("removed", Next::discarded("ownerRemoved"));
    // Same turn id in another session.
    owner_to(&f, "s2", "other", Some(in_context("t2")));
    let untouched = ["kept", "queued", "passive", "removed"].map(|id| f.get(id));
    let other = f.get_in("s2", "other");
    let s2_head = f.head("s2");

    let (transitions, _) = f.retract(&["t2"]);
    let ids: Vec<&str> = transitions.iter().map(|transition| transition.record.input_id.as_str()).collect();
    // `busfail` is `uncertain` (any kind): retracted.
    assert_eq!(ids, ["busfail", "gone"]);
    for record in untouched {
        assert_eq!(f.get(&record.input_id), record, "{}", record.input_id);
    }
    assert_eq!(f.get_in("s2", "other"), other);
    assert_eq!(f.head("s2"), s2_head);
}

#[test]
fn a_failed_non_owner_record_is_not_retracted() {
    let f = Fixture::new();
    let mut failed = f.go("note", Next::waiting("afterTurn").create(note())).record;
    failed.state = InputState::Failed;
    failed.turn_id = Some("t2".into());
    assert!(!candidate(&failed, &["t2".to_string()]));
    failed.kind = InputKind::Owner;
    assert!(candidate(&failed, &["t2".to_string()]));
    assert!(!candidate(&failed, &["t1".to_string()]));
    failed.turn_id = None;
    assert!(!candidate(&failed, &["t2".to_string()]));
}

#[test]
fn the_new_edges_take_only_history_reverted() {
    let f = Fixture::new();
    owner_to(&f, "s1", "ctx", Some(in_context("t2")));
    owner_to(&f, "s1", "acc", Some(Next::accepted().turn("t2")));
    owner_to(&f, "s1", "send", None);
    owner_to(&f, "s1", "fail", Some(Next::failed("notDelivered")));
    for (id, reasons) in [
        ("ctx", &["ownerRemoved", "ownerDiscarded", "archived", "superseded", "settlementClosed"][..]),
        ("acc", &["ownerRemoved", "ownerDiscarded", "archived"][..]),
        ("send", &["ownerDiscarded", "senderRevoked"][..]),
        ("fail", &["ownerRemoved", "archived"][..]),
    ] {
        for reason in reasons {
            let refused = f.ledger.transition("s1", id, Expect::ANY, Next::discarded(reason));
            assert!(matches!(refused, Err(LedgerError::ForbiddenEdge { .. })), "{id} → discarded{{{reason}}}: {refused:?}");
        }
    }
    // The owner's own discard of a failed record still works.
    f.go("fail", Next::discarded("ownerDiscarded"));
    // historyReverted never enters from waiting / received / held, nor a
    // non-owner failed record.
    f.go("w", Next::waiting("afterTurn").create(note()));
    f.go("r", Next::received().create(owner()));
    f.go("h", Next::received().create(owner()));
    f.go("h", Next::held("hostBusy"));
    for id in ["w", "r", "h"] {
        let refused = f.ledger.transition("s1", id, Expect::ANY, Next::discarded(HISTORY_REVERTED));
        assert!(matches!(refused, Err(LedgerError::ForbiddenEdge { .. })), "{id}: {refused:?}");
    }
    let kind = InputKind::AgentMessage;
    assert!(!il::edge_allowed(kind, Some(InputState::Failed), &Next::discarded(HISTORY_REVERTED)));
    assert!(il::edge_allowed(InputKind::Owner, Some(InputState::Failed), &Next::discarded(HISTORY_REVERTED)));
    for from in [InputState::InContext, InputState::Accepted, InputState::Sending, InputState::Uncertain] {
        assert!(il::edge_allowed(kind, Some(from), &Next::discarded(HISTORY_REVERTED)), "{from}");
    }
    // Terminal: nothing leaves discarded{historyReverted}.
    f.go("ctx", Next::discarded(HISTORY_REVERTED));
    for state in InputState::ALL {
        let next = Next { reason: Some(HISTORY_REVERTED.into()), ..Next::to(*state) };
        assert!(!il::edge_allowed(InputKind::Owner, Some(InputState::Discarded), &next), "→ {state}");
    }
}

#[test]
fn late_native_evidence_after_the_retraction_is_a_no_op() {
    let f = Fixture::new();
    owner_to(&f, "s1", "ctx", Some(in_context("t2")));
    owner_to(&f, "s1", "acc", Some(Next::accepted().turn("t2").stage(Stage::Registered)));
    f.retract(&["t2"]);
    for id in ["ctx", "acc"] {
        let record = f.get(id);
        let head = f.head("s1");
        // Claude contextPersisted, Codex clientId echo, A4 receipt, history read.
        for evidence in [Evidence::ContextPersisted, Evidence::ClientIdEcho, Evidence::A4Receipt, Evidence::HistoryRead] {
            let late = f.ledger.transition("s1", id, Expect::ANY, Next::in_context(evidence).turn("t2"));
            assert!(matches!(&late,Err(LedgerError::ForbiddenEdge{..}))||late.as_ref().is_ok_and(|done|!done.changed),"{id} {evidence}: {late:?}");
        }
        let accepted = f.ledger.transition("s1", id, Expect::ANY, Next::accepted().stage(Stage::Offered));
        assert!(matches!(&accepted,Err(LedgerError::ForbiddenEdge{..}))||accepted.as_ref().is_ok_and(|done|!done.changed),"{id}: {accepted:?}");
        // r274.1 timeline-id backfill / native item bind.
        let bind = f.ledger.bind_timeline_item("s1", id, Expect::ANY, "late-item", None);
        assert!(matches!(&bind,Err(LedgerError::Invalid(_)))||bind.as_ref().is_ok_and(|done|!done.changed),"{id}: {bind:?}");
        assert!(!super::super::input_ledger_bind::candidate(&record));
        // Owner native item / turn start / steer echo / history read: no write.
        for step in [
            Step::Started { turn_id: Some("t2".into()) },
            Step::Steered { turn_id: Some("t2".into()), stage: None, generation: None },
            Step::ClientIdEcho,
            Step::SteerEcho { turn_id: Some("t2".into()), item: Some("late-item".into()) },
            Step::AlreadyInContext,
            Step::Uncertain("turnInterrupted"),
            Step::Failed("notDelivered"),
            Step::Removed,
            Step::Sending { steer: false },
        ] {
            assert!(plan(Some(&record), &step, Some(&owner())).is_empty(), "{id}: {step:?}");
        }
        // No owner action: no retry, no dismiss.
        assert!(record.actions.is_empty());
        assert!(!il::dismissible(&record));
        assert!(!super::super::input_ledger_settle::retryable(&record));
        assert!(!il::dismiss_one(&f.ledger,"s1",id,record.rev).unwrap().changed,"already disposed is idempotent");
        assert_eq!(f.get(id), record);
        assert_eq!(f.head("s1"), head);
    }
}

// ---------------------------------------------------------------------------
// Receipts

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

/// cli_app `session_history` receipt facts.
fn codex_facts(action: &str, thread: &str, status: &str, removed: &[&str]) -> Value {
    json!({"actionId": action, "threadId": thread, "keepTurnId": "t1", "beforeTurnId": removed.last(),
        "removedTurns": removed.len(), "removedTurnIds": removed, "live": true, "source": "x",
        "backup": "y", "sources": [], "status": status})
}

/// `session_ops.py` `audit` record of a revert.
fn claude_facts(action: &str, thread: &str, state: &str, committed: bool, removed: &[&str]) -> Value {
    json!({"threadId": thread, "beforeTurnId": removed.first(), "removedTurnIds": removed,
        "sourceSessionId": "native", "backupPath": null, "rewindFilesRequested": false, "filesRewound": false,
        "historyCommitted": committed, "nativeSessionId": "native-2", "runtime": {"state": "waiting"},
        "actionId": action, "state": state, "receiptPath": "p"})
}

fn sessions(root: &Path) -> Vec<SessionView> {
    vec![
        SessionView { id: "codex-1".into(), thread_id: Some("thread-c".into()), claude_state_dir: None, codex: true },
        SessionView { id: "claude-1".into(), thread_id: Some("thread-a".into()),
            claude_state_dir: Some(root.join("runtime/claude/sessions/claude-1")), codex: false },
        // A Claude session with the Codex thread id never takes a Codex receipt.
        SessionView { id: "claude-2".into(), thread_id: Some("thread-x".into()),
            claude_state_dir: Some(root.join("runtime/claude/sessions/claude-2")), codex: false },
    ]
}

fn fixture_receipts(root: &Path) {
    let runtime = root.join("runtime");
    let reverts = runtime.join("history-reverts");
    write(&reverts.join("revert-a/receipt.json"), &codex_facts("revert-a", "thread-c", "complete", &["c3", "c2"]));
    for (action, status) in [("revert-b", "prepared"), ("revert-c", "outcome_unknown"), ("revert-d", "rejected")] {
        write(&reverts.join(action).join("receipt.json"), &codex_facts(action, "thread-c", status, &["c1"]));
    }
    // A thread no Codex session holds.
    write(&reverts.join("revert-e/receipt.json"), &codex_facts("revert-e", "thread-x", "complete", &["c1"]));
    write(&reverts.join("revert-f/receipt.json"), &codex_facts("revert-f", "thread-c", "complete", &[]));
    std::fs::create_dir_all(reverts.join("revert-g")).unwrap();
    std::fs::write(reverts.join("revert-g/receipt.json"), b"{ not json").unwrap();
    let ops = runtime.join("claude/sessions/claude-1/session-operations");
    write(&ops.join("op-1.json"), &claude_facts("op-1", "thread-a", "completed", true, &["a2"]));
    write(&ops.join("op-2.json"), &claude_facts("op-2", "thread-a", "failed", false, &["a1"]));
    write(&ops.join("op-3.json"), &claude_facts("op-3", "thread-a", "completed", false, &["a1"]));
    write(&ops.join("op-4.json"), &claude_facts("op-4", "thread-a", "partial", true, &["a1"]));
    write(&ops.join("op-5.json"), &claude_facts("op-5", "thread-other", "completed", true, &["a1"]));
    // A fork receipt (no removedTurnIds / historyCommitted).
    write(&ops.join("op-6.json"), &json!({"threadId": "thread-f", "nativeSessionId": "n", "actionId": "op-6",
        "state": "completed", "receiptPath": "p"}));
    write(&ops.join("op-7.tmp"), &claude_facts("op-7", "thread-a", "completed", true, &["a1"]));
}

#[test]
fn only_completed_committed_receipts_count() {
    let root = std::env::temp_dir().join(format!("cutex-input-revert-receipts-{}", uuid::Uuid::new_v4().simple()));
    fixture_receipts(&root);
    let found = receipts(&root.join("runtime"), &sessions(&root));
    assert_eq!(found, vec![
        Receipt { session: "codex-1".into(), action_id: Some("revert-a".into()), removed: vec!["c3".into(), "c2".into()] },
        Receipt { session: "claude-1".into(), action_id: Some("op-1".into()), removed: vec!["a2".into()] },
    ]);
    // Two Codex sessions on one thread: ambiguous, skipped.
    let mut twice = sessions(&root);
    twice.push(SessionView { id: "codex-2".into(), thread_id: Some("thread-c".into()), claude_state_dir: None, codex: true });
    assert!(codex_receipt(&codex_facts("r", "thread-c", "complete", &["c1"]), &twice).is_none());
    assert!(receipts(&root.join("absent"), &[]).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_recovery_retracts_from_codex_and_claude_receipts() {
    let f = Fixture::new();
    fixture_receipts(&f.root);
    owner_to(&f, "codex-1", "c-kept", Some(in_context("c1")));
    owner_to(&f, "codex-1", "c-gone", Some(in_context("c2")));
    owner_to(&f, "codex-1", "c-steer", Some(Next::accepted().turn("c3")));
    owner_to(&f, "claude-1", "a-kept", Some(in_context("a1")));
    owner_to(&f, "claude-1", "a-gone", Some(Next::in_context(Evidence::ContextPersisted).turn("a2")));
    let kept = [f.get_in("codex-1", "c-kept"), f.get_in("claude-1", "a-kept")];
    let found = receipts(&f.root.join("runtime"), &sessions(&f.root));
    let ((count, retracted), events) = il::capture_events(|| recover(&f.ledger, &found));
    assert_eq!((count, retracted), (2, 3));
    assert_eq!(events.len(), 3);
    let mut by_session: HashMap<&str, usize> = HashMap::new();
    for (session, _, _) in &events {
        *by_session.entry(session.as_str()).or_default() += 1;
    }
    assert_eq!(by_session, HashMap::from([("codex-1", 2), ("claude-1", 1)]));
    for (session, id, action, turn) in [
        ("codex-1", "c-gone", "revert-a", "c2"),
        ("codex-1", "c-steer", "revert-a", "c3"),
        ("claude-1", "a-gone", "op-1", "a2"),
    ] {
        let record = f.get_in(session, id);
        assert_eq!((record.state, record.reason.as_deref(), record.surface),
            (InputState::Discarded, Some(HISTORY_REVERTED), Surface::None), "{id}");
        assert_eq!(record.details[DETAILS_KEY], json!({"actionId": action, "turnId": turn}), "{id}");
    }
    assert_eq!(f.get_in("claude-1", "a-gone").evidence, Some(Evidence::ContextPersisted));
    for record in kept {
        assert_eq!(f.get_in(&record.cutex_session_id, &record.input_id), record);
    }
    // A second start finds the same receipts and changes nothing.
    let ((_, again), events) = il::capture_events(|| recover(&f.ledger, &found));
    assert_eq!(again, 0);
    assert!(events.is_empty());
}
