use super::*;
use std::cell::RefCell;
use std::path::PathBuf;

use crate::management::v2::input_ledger::{
    Evidence, Expect, InputIntent, InputSender, NewInput, Next, SenderKind, Strategy, Timing,
};

struct Fixture {
    root: PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-input-bind-{}", uuid::Uuid::new_v4().simple()));
        Self { ledger: InputLedger::open(root.join(il::DATABASE_FILE)), root }
    }

    fn go(&self, id: &str, next: Next) {
        il::capture_events(|| self.ledger.transition("s1", id, Expect::ANY, next)).0.unwrap();
    }

    fn get(&self, id: &str) -> InputRecord {
        self.ledger.get("s1", &[id.to_string()]).unwrap().records.pop().unwrap()
    }

    fn page(&self) -> Vec<InputRecord> {
        self.ledger.in_context_page(None, BATCH).unwrap()
    }

    /// One bind pass; the published events.
    fn bind(&self, evidence: &Fake) -> (Counts, Vec<(String, String, serde_json::Value)>) {
        il::capture_events(|| bind_page(&self.ledger, &self.page(), evidence))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn input(kind: InputKind, sender: SenderKind) -> NewInput {
    NewInput {
        kind,
        sender: InputSender { kind: sender, id: "sender".into(), name: "Sender".into(), host: None, cutex_session_id: None },
        intent: InputIntent {
            strategy: (kind == InputKind::Owner).then_some(Strategy::Auto),
            delivery_mode: None,
            timing: Timing::AfterTurn,
        },
        preview: "text".into(),
        ask_id: None,
        remote: None,
    }
}

/// A result of results event `event` read as its turn's input: `inContext`
/// on the timeline with no item (how r274 wrote it).
fn result_in_context(f: &Fixture, id: &str, event: &str) {
    let mut sending = Next::sending().create(input(InputKind::DelegationResult, SenderKind::Agent));
    sending.batch_id = Some(event.into());
    f.go(id, Next::waiting("resultCoalescing").create(input(InputKind::DelegationResult, SenderKind::Agent)));
    f.go(id, sending);
    f.go(id, Next::in_context(Evidence::TurnStartInput).turn("turn-1"));
}

/// A delegation request that ran in `turn` on its target (how r274 wrote it).
fn request_in_context(f: &Fixture, id: &str, turn: &str) {
    let mut request = input(InputKind::DelegationRequest, SenderKind::Agent);
    request.ask_id = Some(id.into());
    f.go(id, Next::waiting("targetBusy").create(request));
    f.go(id, Next::sending().turn(turn));
    f.go(id, Next::in_context(Evidence::TurnStartInput).turn(turn));
}

fn bus_in_context(f: &Fixture, id: &str, kind: InputKind, sender: SenderKind) {
    f.go(id, Next::waiting("afterTurn").create(input(kind, sender)));
    f.go(id, Next::sending());
    f.go(id, Next::in_context(Evidence::ExternalInputPersisted).turn("turn-2"));
}

fn owner_in_context(f: &Fixture, id: &str) {
    f.go(id, Next::received().create(input(InputKind::Owner, SenderKind::Owner)));
    f.go(id, Next::sending());
    f.go(id, Next::in_context(Evidence::TurnStartInput).turn("turn-3"));
}

/// Stored native evidence by input id; records every lookup.
#[derive(Default)]
struct Fake {
    delegations: Vec<(String, String, String)>,
    requests: Vec<(String, String)>,
    bus: Vec<(String, String)>,
    asked: RefCell<Vec<String>>,
}

impl NativeEvidence for Fake {
    fn delegations(&self, records: &[&InputRecord]) -> Vec<Option<(String, String)>> {
        records
            .iter()
            .map(|record| {
                self.asked.borrow_mut().push(record.input_id.clone());
                self.delegations.iter().find(|(id, _, _)| *id == record.input_id).map(|(_, item, event)| (item.clone(), event.clone()))
            })
            .collect()
    }

    fn requests(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
        records
            .iter()
            .map(|record| {
                self.asked.borrow_mut().push(record.input_id.clone());
                self.requests.iter().find(|(id, _)| *id == record.input_id).map(|(_, item)| item.clone())
            })
            .collect()
    }

    fn bus(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
        records
            .iter()
            .map(|record| {
                self.asked.borrow_mut().push(record.input_id.clone());
                self.bus.iter().find(|(id, _)| *id == record.input_id).map(|(_, item)| item.clone())
            })
            .collect()
    }
}

fn evidence() -> Fake {
    Fake {
        delegations: vec![
            ("dlv-a".into(), "cutex-ask-event-1".into(), "aev-1".into()),
            ("dlv-b".into(), "cutex-ask-event-1".into(), "aev-1".into()),
        ],
        requests: vec![("ask-1".into(), "cutex-ask-msg-1".into())],
        bus: vec![("msg-1".into(), "msg-1".into()), ("alarm-1".into(), "alarm-1".into())],
        asked: RefCell::new(Vec::new()),
    }
}

#[test]
fn backfill_binds_derivable_records_once_and_is_idempotent() {
    let f = Fixture::new();
    // Two results coalesced into one event share its native item.
    result_in_context(&f, "dlv-a", "aev-1");
    result_in_context(&f, "dlv-b", "aev-1");
    bus_in_context(&f, "msg-1", InputKind::AgentMessage, SenderKind::Agent);
    bus_in_context(&f, "alarm-1", InputKind::Alarm, SenderKind::Alarm);
    owner_in_context(&f, "owner-1");
    request_in_context(&f, "ask-1", "turn-4");
    let before: Vec<InputRecord> = ["dlv-a", "dlv-b", "msg-1", "alarm-1", "owner-1", "ask-1"].iter().map(|id| f.get(id)).collect();

    let (counts, events) = f.bind(&evidence());
    assert_eq!(counts, Counts { candidates: 6, fixed: 5, not_derivable: 1, owner: 1, refused: 0 });
    // A request: its attempt's message, never its askId.
    assert_eq!(f.get("ask-1").timeline_item_id.as_deref(), Some("cutex-ask-msg-1"));
    assert_eq!(f.get("dlv-a").timeline_item_id.as_deref(), Some("cutex-ask-event-1"));
    assert_eq!(f.get("dlv-b").timeline_item_id.as_deref(), Some("cutex-ask-event-1"));
    assert_eq!(f.get("msg-1").timeline_item_id.as_deref(), Some("msg-1"));
    assert_eq!(f.get("alarm-1").timeline_item_id.as_deref(), Some("alarm-1"));
    assert_eq!(f.get("owner-1"), before[4], "owner records are left alone");
    // Each binding is a normal revisioned change the clients see.
    let bound: Vec<_> = events
        .iter()
        .filter(|(_, method, _)| method == il::STATE_CHANGED_EVENT)
        .map(|(_, _, params)| (params["record"]["inputId"].clone(), params["record"]["timelineItemId"].clone()))
        .collect();
    assert_eq!(bound.len(), 5, "{events:?}");
    let changed = [&before[0], &before[1], &before[2], &before[3], &before[5]];
    for (record, old) in ["dlv-a", "dlv-b", "msg-1", "alarm-1", "ask-1"].iter().map(|id| f.get(id)).zip(changed) {
        assert_eq!(record.rev, old.rev + 1);
        assert!(record.state_revision > old.state_revision);
    }

    // Idempotent: nothing is a candidate any more but the owner record.
    let (again, events) = f.bind(&evidence());
    assert_eq!(again, Counts { candidates: 1, fixed: 0, not_derivable: 1, owner: 1, refused: 0 });
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn backfill_leaves_a_not_derivable_record_as_it_is_and_counts_it() {
    let f = Fixture::new();
    result_in_context(&f, "dlv-lost", "aev-9");
    bus_in_context(&f, "msg-lost", InputKind::SystemNotice, SenderKind::ClaudeTask);
    request_in_context(&f, "ask-lost", "turn-9");
    let before = (f.get("dlv-lost"), f.get("msg-lost"), f.get("ask-lost"));
    let (counts, events) = f.bind(&evidence());
    assert_eq!(counts, Counts { candidates: 3, fixed: 0, not_derivable: 3, owner: 0, refused: 0 });
    assert!(events.is_empty());
    assert_eq!((f.get("dlv-lost"), f.get("msg-lost"), f.get("ask-lost")), before);
}

#[test]
fn backfill_never_binds_another_batch_or_rebinds() {
    let f = Fixture::new();
    // Proven by attempt 2 (event aev-2); stale evidence names aev-1.
    result_in_context(&f, "dlv-a", "aev-2");
    let before = f.get("dlv-a");
    let (counts, _) = f.bind(&evidence());
    assert_eq!(counts, Counts { candidates: 1, fixed: 0, not_derivable: 1, owner: 0, refused: 1 });
    assert_eq!(f.get("dlv-a"), before);
    // A bound record is never rebound, the same item is no change.
    let ledger = &f.ledger;
    let bound = il::capture_events(|| ledger.bind_timeline_item("s1", "dlv-a", Expect::ANY, "cutex-ask-event-2", Some("aev-2"))).0.unwrap();
    assert!(bound.changed);
    let same = il::capture_events(|| ledger.bind_timeline_item("s1", "dlv-a", Expect::ANY, "cutex-ask-event-2", Some("aev-2"))).0.unwrap();
    assert!(!same.changed);
    let other = il::capture_events(|| ledger.bind_timeline_item("s1", "dlv-a", Expect::ANY, "cutex-ask-event-3", None)).0;
    assert!(matches!(other, Err(LedgerError::Invalid(_))), "{other:?}");
    assert_eq!(f.get("dlv-a").timeline_item_id.as_deref(), Some("cutex-ask-event-2"));
}

#[test]
fn backfill_changes_nothing_but_the_item_and_never_redelivers() {
    let f = Fixture::new();
    result_in_context(&f, "dlv-a", "aev-1");
    let before = f.get("dlv-a");
    let (counts, events) = f.bind(&evidence());
    assert_eq!(counts.fixed, 1);
    let after = f.get("dlv-a");
    // Only the binding (and the revision it takes) changed: same inputId,
    // state, surface, attempt, evidence, turn and batch; no deadline, no
    // retry action, no new attempt.
    let mut probe = after.clone();
    probe.timeline_item_id = None;
    probe.rev = before.rev;
    probe.state_revision = before.state_revision;
    probe.updated_at = before.updated_at.clone();
    assert_eq!(probe, before);
    assert_eq!(after.state, InputState::InContext);
    assert_eq!(after.attempt, 1);
    // The only events are the record's own stateChanged (no stream reset,
    // no delivery change, nothing for another input).
    assert!(events.iter().all(|(session, method, params)| session == "s1"
        && method == il::STATE_CHANGED_EVENT
        && params["record"]["inputId"] == "dlv-a"), "{events:?}");
    // Records that are not in context (waiting, sending, settled other
    // ways) are never looked at.
    f.go("dlv-w", Next::waiting("resultCoalescing").create(input(InputKind::DelegationResult, SenderKind::Agent)));
    f.go("dlv-s", Next::waiting("resultCoalescing").create(input(InputKind::DelegationResult, SenderKind::Agent)));
    f.go("dlv-s", Next::sending());
    let fake = evidence();
    let (counts, _) = il::capture_events(|| bind_page(&f.ledger, &f.page(), &fake));
    assert_eq!(counts.candidates, 0);
    assert!(fake.asked.borrow().is_empty());
    assert_eq!(f.get("dlv-w").state, InputState::Waiting);
    assert_eq!(f.get("dlv-s").state, InputState::Sending);
}

#[test]
fn backfill_of_a_request_changes_nothing_but_the_item_and_never_redelivers() {
    let f = Fixture::new();
    request_in_context(&f, "ask-1", "turn-4");
    let before = f.get("ask-1");
    let (counts, events) = f.bind(&evidence());
    assert_eq!(counts.fixed, 1);
    let after = f.get("ask-1");
    let mut probe = after.clone();
    probe.timeline_item_id = None;
    probe.rev = before.rev;
    probe.state_revision = before.state_revision;
    probe.updated_at = before.updated_at.clone();
    assert_eq!(probe, before, "same inputId, state, surface, turn, attempt, evidence");
    assert_eq!(after.timeline_item_id.as_deref(), Some("cutex-ask-msg-1"));
    assert!(events.iter().all(|(session, method, params)| session == "s1"
        && method == il::STATE_CHANGED_EVENT
        && params["record"]["inputId"] == "ask-1"), "{events:?}");
    // Idempotent.
    let (again, events) = f.bind(&evidence());
    assert_eq!(again.candidates, 0);
    assert!(events.is_empty());
}

#[test]
fn backfill_steps_through_batches_and_finishes() {
    let f = Fixture::new();
    for n in 0..(BATCH + 3) {
        bus_in_context(&f, &format!("m-{n:04}"), InputKind::AgentMessage, SenderKind::Agent);
    }
    struct All;
    impl NativeEvidence for All {
        fn delegations(&self, records: &[&InputRecord]) -> Vec<Option<(String, String)>> {
            vec![None; records.len()]
        }
        fn requests(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
            vec![None; records.len()]
        }
        fn bus(&self, records: &[&InputRecord]) -> Vec<Option<String>> {
            records.iter().map(|record| Some(record.input_id.clone())).collect()
        }
    }
    let mut backfill = Backfill::start();
    let mut passes = 0;
    while !backfill.done() {
        il::capture_events(|| backfill.step(&f.ledger, &All));
        passes += 1;
        assert!(passes <= 3);
    }
    assert_eq!(passes, 2);
    assert_eq!(backfill.counts().fixed, BATCH + 3);
    assert!(f.page().iter().all(|record| record.timeline_item_id.as_deref() == Some(record.input_id.as_str())));
}
