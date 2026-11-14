use super::*;
use chrono::TimeZone;
use std::cell::RefCell;
use std::path::PathBuf;

use crate::management::v2::input_ledger::{
    InputIntent, InputSender, NewInput, SenderKind, Strategy, Timing,
};

struct Fixture {
    root: PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-input-settle-{}", uuid::Uuid::new_v4().simple()));
        let ledger = InputLedger::open(root.join(il::DATABASE_FILE));
        ledger.set_clock(t0());
        Self { root, ledger }
    }

    fn go(&self, id: &str, next: Next) -> Transition {
        il::capture_events(|| self.ledger.transition("s1", id, Expect::ANY, next)).0.unwrap()
    }

    fn get(&self, id: &str) -> InputRecord {
        self.ledger.get("s1", &[id.to_string()]).unwrap().records.pop().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap()
}

fn sender(kind: SenderKind) -> InputSender {
    InputSender { kind, id: "sender".into(), name: "Sender".into(), host: None, cutex_session_id: None }
}

fn owner_input() -> NewInput {
    NewInput {
        kind: InputKind::Owner,
        sender: sender(SenderKind::Owner),
        intent: InputIntent { strategy: Some(Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    }
}

fn note_input() -> NewInput {
    NewInput {
        kind: InputKind::AgentMessage,
        sender: sender(SenderKind::Agent),
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::AfterTurn },
        preview: "note".into(),
        ask_id: None,
        remote: None,
    }
}

/// An owner steer accepted into `turn`.
fn accepted_owner(f: &Fixture, id: &str, turn: &str) {
    f.go(id, Next::received().create(owner_input()));
    f.go(id, Next::sending());
    f.go(id, Next::accepted().turn(turn));
}

/// A note claimed (accepted) without a turn id, like a Claude local inbox receipt.
fn accepted_note(f: &Fixture, id: &str, turn: Option<&str>) {
    f.go(id, Next::waiting("afterTurn").create(note_input()));
    f.go(id, Next::sending());
    let next = match turn {
        Some(turn) => Next::accepted().turn(turn),
        None => Next::accepted(),
    };
    f.go(id, next);
}

#[derive(Default)]
struct FakeRuntime {
    view: RuntimeView,
    /// method → response (Err: a transport failure).
    answers: BTreeMap<String, Result<Value, String>>,
    external: BTreeMap<String, (String, String, Value)>,
    calls: RefCell<Vec<(String, Value)>>,
}

impl FakeRuntime {
    fn connected(turn: Option<&str>) -> Self {
        Self {
            view: RuntimeView {
                present: true,
                connected: true,
                stopped: false,
                generation: Some(3),
                thread_id: Some("thread-1".into()),
                active_turn: turn.map(str::to_string),
                turn_observed_at: turn.map(|_| "2026-11-02T12:00:00.000Z".into()),
            },
            ..Self::default()
        }
    }

    fn answer(mut self, method: &str, answer: Result<Value, String>) -> Self {
        self.answers.insert(method.into(), answer);
        self
    }
}

impl Runtime for FakeRuntime {
    fn view(&self, _session: &str) -> RuntimeView {
        self.view.clone()
    }
    fn request(&self, _session: &str, method: &str, params: Value) -> Result<Value, String> {
        self.calls.borrow_mut().push((method.to_string(), params));
        self.answers.get(method).cloned().unwrap_or_else(|| Err(format!("{method}: no fixture")))
    }
    fn bus_external_key(&self, message_id: &str) -> Option<(String, String, Value)> {
        self.external.get(message_id).cloned()
    }
}

fn status(delivery: &str, processing: &str, reason: Option<&str>) -> Value {
    json!({"statuses": [{"messageId": "m", "deliveryState": delivery,
        "processing": {"state": processing, "attemptId": null, "reason": reason}}]})
}

#[test]
fn reconnect_checks_accepted_bus_without_a_carrying_turn_and_does_not_reaccept_uncertain() {
    for persisted in [true, false] {
        let f = Fixture::new();
        f.go("orphan", Next::sending().create(note_input()));
        f.go("orphan", Next::accepted());
        let reply = json!({"contextPersisted":persisted,"state":"failed","turnId":"proved-turn"});
        let runtime = FakeRuntime::connected(None).answer("claude/input/status", Ok(reply));
        let (changed, _) = il::capture_events(|| reconnected(&f.ledger, &runtime, "s1", false, t0()));
        assert_eq!(changed, 1);
        let row = f.get("orphan");
        assert_eq!(row.state, if persisted { InputState::InContext } else { InputState::Uncertain });
        if persisted { assert_eq!(row.turn_id.as_deref(), Some("proved-turn")); }
        if !persisted {
            assert!(row.actions.contains(&il::Action::Dismiss));
            let target = super::super::input_ledger_bus::Target::Accepted { turn:None };
            assert!(super::super::input_ledger_bus::plan(Some(row.state), &target, Timing::AfterTurn).is_empty());
        }
        il::capture_events(|| reconnected(&f.ledger, &runtime, "s1", false, t0()));
        assert_eq!(runtime.calls.borrow().len(), 1, "no replay or repeated check of settled inputs");
    }
}

#[test]
fn unbound_native_pending_input_retires_its_one_shot_deadline() {
    let f = Fixture::new();
    f.go("pending", Next::sending().create(note_input()));
    let record = f.go("pending", Next::accepted().deadline(Some(t0()))).record;
    let runtime = FakeRuntime::connected(None).answer("claude/input/status", Ok(json!({"state":"queued"})));
    let next = deadline_decision(&record, &runtime, &NoStores, t0()).unwrap();
    assert_eq!(next.state, InputState::Accepted);
    assert_eq!(next.deadline, Some(None));
    assert_eq!(runtime.calls.borrow().len(), 1);
}

#[test]
fn native_statuses_map_to_checks() {
    assert_eq!(parse_external_status(&status("context_persisted", "none", None)), Check::Persisted(Evidence::ExternalInputPersisted));
    assert_eq!(parse_external_status(&status("unknown", "none", None)), Check::Absent);
    assert_eq!(parse_external_status(&status("pending", "held", Some("interrupted"))), Check::Held(Some("interrupted".into())));
    assert_eq!(parse_external_status(&status("pending", "claimed", None)), Check::Unpersisted);
    assert_eq!(parse_external_status(&status("pending", "output_observed", None)), Check::Unpersisted);
    assert_eq!(parse_external_status(&status("pending", "pending", None)), Check::Queued);
    assert_eq!(parse_external_status(&status("conflict", "none", None)), Check::Conflict);
    assert_eq!(parse_external_status(&json!({})), Check::Unavailable);
    assert_eq!(parse_local_status(&json!({"contextPersisted": true, "state": "done"})), Check::Persisted(Evidence::ContextPersisted));
    assert_eq!(parse_local_status(&json!({"contextPersisted": false, "state": "interrupted"})), Check::Held(Some("interrupted".into())));
    assert_eq!(parse_local_status(&json!({"contextPersisted": false, "state": "queued"})), Check::Queued);
    let items = json!({"data": [{"turnId": "t1", "item": {"type": "userMessage", "clientId": "m1"}}]});
    assert_eq!(parse_turn_items(&items, "m1"), Check::Persisted(Evidence::HistoryRead));
    assert_eq!(parse_turn_items(&items, "m2"), Check::Absent);
    assert_eq!(parse_turn_items(&json!({"error": 1}), "m1"), Check::Unavailable);
}

#[test]
fn settle_table_rows() {
    use Occasion::*;
    let state = |next: Option<Next>| next.map(|next| (next.state, next.reason));
    let uncertain = |reason: &str| Some((InputState::Uncertain, Some(reason.to_string())));
    // Persisted → inContext, for every occasion.
    for occasion in [TurnInterrupted, TurnFailed, ReceiptDeadline, GenerationChanged] {
        assert_eq!(state(settle_next(InputKind::AgentMessage, occasion, &Check::Persisted(Evidence::A4Receipt))),
            Some((InputState::InContext, None)));
    }
    // Absent: owner failed{notDelivered}; others uncertain (accepted never goes back to waiting).
    assert_eq!(state(settle_next(InputKind::Owner, TurnInterrupted, &Check::Absent)), Some((InputState::Failed, Some("notDelivered".into()))));
    assert_eq!(state(settle_next(InputKind::AgentMessage, TurnInterrupted, &Check::Absent)), uncertain("turnInterrupted"));
    // Held → uncertain{turnInterrupted} with details.nativeReason.
    let held = settle_next(InputKind::AgentMessage, TurnInterrupted, &Check::Held(Some("interrupted".into()))).unwrap();
    assert_eq!((held.state, held.reason.as_deref()), (InputState::Uncertain, Some("turnInterrupted")));
    assert_eq!(held.details, Some(json!({"nativeReason": "interrupted"})));
    assert_eq!(state(settle_next(InputKind::AgentMessage, TurnFailed, &Check::Unpersisted)), uncertain("turnFailed"));
    // Unavailable: an interrupted turn waits for the deadline; the deadline is receiptTimeout.
    assert_eq!(state(settle_next(InputKind::AgentMessage, TurnInterrupted, &Check::Unavailable)), None);
    assert_eq!(state(settle_next(InputKind::AgentMessage, ReceiptDeadline, &Check::Unavailable)), uncertain("receiptTimeout"));
    assert_eq!(state(settle_next(InputKind::Owner, GenerationChanged, &Check::Unavailable)), uncertain("runtimeLost"));
    // A note still waiting for its turn is left alone at a turn end.
    assert_eq!(state(settle_next(InputKind::AgentMessage, TurnInterrupted, &Check::Queued)), None);
    assert_eq!(state(settle_next(InputKind::AgentMessage, TurnInterrupted, &Check::Conflict)), uncertain("nativeConflict"));
}

fn facts(connected: bool, turn: Option<&str>) -> DeliveryFacts {
    DeliveryFacts { runtime: FakeRuntime::connected(turn).view, ..DeliveryFacts::default() }
        .tap(|facts| facts.runtime.connected = connected)
}

trait Tap: Sized {
    fn tap(mut self, edit: impl FnOnce(&mut Self)) -> Self {
        edit(&mut self);
        self
    }
}
impl Tap for DeliveryFacts {}

#[test]
fn delivery_decision_order() {
    let now = t0();
    let state = |facts: &DeliveryFacts| {
        let value = session_delivery(facts, now);
        (value["state"].as_str().unwrap().to_string(), value["reason"].as_str().map(str::to_string))
    };
    let s = |state: &str, reason: Option<&str>| (state.to_string(), reason.map(str::to_string));
    // 1 offline: no runtime, or stopped on purpose.
    assert_eq!(state(&DeliveryFacts::default()), s("offline", None));
    assert_eq!(state(&facts(false, None).tap(|f| f.runtime.stopped = true)), s("offline", None));
    // 3 disconnected: never reports a turn.
    let lost = session_delivery(&facts(false, Some("t1")), now);
    assert_eq!((lost["state"].clone(), lost["turn"].clone()), (json!("disconnected"), Value::Null));
    // 4–6 held, in order: hostBusy before stopHold before paused.
    let all = facts(true, Some("t1")).tap(|f| {
        f.held_deadlines = vec!["2026-11-02T12:01:00.000Z".into()];
        f.stop_hold = true;
        f.paused = true;
    });
    assert_eq!(state(&all), s("held", Some("hostBusy")));
    let held = session_delivery(&all, now);
    assert_eq!((held["heldCount"].clone(), held["heldUntil"].clone()), (json!(1), json!("2026-11-02T12:01:00.000Z")));
    assert_eq!(state(&all.clone().tap(|f| f.held_deadlines.clear())), s("held", Some("stopHold")));
    assert_eq!(state(&all.clone().tap(|f| { f.held_deadlines.clear(); f.stop_hold = false; })), s("held", Some("paused")));
    // 7 finishing, 8 turn.
    assert_eq!(state(&facts(true, Some("t1")).tap(|f| f.finishing = true)), s("finishing", None));
    let turn = session_delivery(&facts(true, Some("t1")).tap(|f| { f.turn_kind = Some("owner".into()); f.turn_inputs = vec!["m1".into()]; }), now);
    assert_eq!(turn["state"], "turn");
    assert_eq!(turn["turn"], json!({"turnId": "t1", "kind": "owner", "askId": null,
        "startedAt": "2026-11-02T12:00:00.000Z", "inputIds": ["m1"]}));
    // The lease of the running turn names its kind (ask → askId).
    let lease = json!({"holder": "ask", "turnId": "t1", "since": "x", "runtimeGeneration": 3, "inputIds": ["ask-1"],
        "reconcile": {"state": "waitingTurn", "lastCheckedAt": null, "nextCheckAt": null, "lastResult": null}});
    let ask_turn = session_delivery(&facts(true, Some("t1")).tap(|f| f.lease = Some(lease.clone())), now);
    assert_eq!((ask_turn["turn"]["kind"].clone(), ask_turn["turn"]["askId"].clone()), (json!("ask"), json!("ask-1")));
    assert_eq!(ask_turn["lease"]["reconcile"]["state"], "waitingTurn");
    // 9 a lease without a turn, 10 idle.
    assert_eq!(state(&facts(true, None).tap(|f| f.lease = Some(lease.clone()))), s("held", Some("delegationLease")));
    let event_lease = json!({"holder": "event", "turnId": null, "since": "x", "runtimeGeneration": 3, "inputIds": [],
        "reconcile": {"state": "stale"}});
    assert_eq!(state(&facts(true, None).tap(|f| f.lease = Some(event_lease))), s("held", Some("eventLease")));
    assert_eq!(state(&facts(true, None)), s("idle", None));
}

#[test]
fn delivery_is_published_only_on_change() {
    let f = Fixture::new();
    let first = session_delivery(&facts(true, None), t0());
    let a = f.ledger.delivery_changed("s1", &first).unwrap();
    // observedAt alone is no change.
    let again = session_delivery(&facts(true, None), t0() + chrono::Duration::seconds(5));
    let b = f.ledger.delivery_changed("s1", &again).unwrap();
    assert!(a.changed && !b.changed);
    let busy = session_delivery(&facts(true, Some("t1")), t0());
    let c = f.ledger.delivery_changed("s1", &busy).unwrap();
    assert!(c.changed && c.state_revision == a.state_revision + 1);
}

#[test]
fn interrupted_turn_settles_each_accepted_record_with_one_check() {
    let f = Fixture::new();
    accepted_owner(&f, "m-persisted", "t1");
    accepted_owner(&f, "m-absent", "t1");
    accepted_owner(&f, "m-other-turn", "t0");
    accepted_note(&f, "n-held", None);
    accepted_note(&f, "n-queued", None);
    let mut runtime = FakeRuntime::connected(None).answer("thread/items/list", Ok(json!({"data": [
        {"turnId": "t1", "item": {"type": "userMessage", "clientId": "m-persisted"}}]})));
    runtime.external.insert("n-held".into(), ("s1".into(), "thread-1".into(), json!({"messageId": "n-held"})));
    let runtime = runtime.answer("thread/externalInput/status", Ok(status("pending", "held", Some("interrupted"))))
        .answer("claude/input/status", Ok(json!({"state": "queued", "contextPersisted": false})));
    let (changed, events) = il::capture_events(|| turn_ended(&f.ledger, &runtime, "s1", "t1", "interrupted", t0()));
    assert_eq!(changed, 3);
    assert_eq!(events.len(), 3);
    assert_eq!(f.get("m-persisted").state, InputState::InContext);
    assert_eq!(f.get("m-persisted").evidence, Some(Evidence::HistoryRead));
    assert_eq!((f.get("m-absent").state, f.get("m-absent").reason.as_deref()), (InputState::Failed, Some("notDelivered")));
    assert_eq!(f.get("m-other-turn").state, InputState::Accepted, "another turn's record is not touched");
    // Held by the interrupt: carried by the turn, checked once more in 10 s.
    // r274.2 §9.4: held is not read evidence, so it stays in nextUp.
    let held = f.get("n-held");
    assert_eq!(held.state, InputState::Accepted);
    assert_eq!(held.deadline.as_deref(), Some("2026-11-02T12:00:10.000Z"));
    assert_eq!((held.surface, held.turn_id.as_deref()), (Surface::NextUp, Some("t1")), "held by the interrupted turn, not read");
    assert_eq!(f.get("n-queued").state, InputState::Accepted, "a note still waiting for its turn stays");
    // One request per checked record, never a history scan.
    // Still held at the re-check → uncertain{turnInterrupted}; persisted by then → inContext.
    let next = deadline_decision(&held, &runtime, &NoStores, t0() + chrono::Duration::seconds(11)).unwrap();
    assert_eq!((next.state, next.reason.as_deref()), (InputState::Uncertain, Some("turnInterrupted")));
    assert_eq!(next.details, Some(json!({"nativeReason": "interrupted"})));
    let retried = FakeRuntime { external: runtime.external.clone(), ..FakeRuntime::connected(None) }
        .answer("thread/externalInput/status", Ok(status("context_persisted", "none", None)));
    assert_eq!(deadline_decision(&held, &retried, &NoStores, t0() + chrono::Duration::seconds(11)).unwrap().state, InputState::InContext);
    let calls = runtime.calls.borrow();
    assert_eq!(calls.len(), 5);
    assert!(calls.iter().all(|(method, _)| method != "thread/read"));
    assert!(calls.iter().filter(|(method, _)| method == "thread/items/list").all(|(_, params)| params["turnId"] == "t1"));
}

#[test]
fn completed_turn_starts_the_receipt_grace_and_the_deadline_checks_once() {
    let f = Fixture::new();
    accepted_owner(&f, "m1", "t1");
    let runtime = FakeRuntime::connected(None);
    il::capture_events(|| turn_ended(&f.ledger, &runtime, "s1", "t1", "completed", t0()));
    let record = f.get("m1");
    assert_eq!(record.state, InputState::Accepted);
    assert_eq!(record.deadline.as_deref(), Some("2026-11-02T12:01:00.000Z"), "turn end + 60 s");
    assert!(runtime.calls.borrow().is_empty(), "no native IO at a completed turn");
    // At the deadline: one check; history shows it → inContext.
    let runtime = runtime.answer("thread/items/list", Ok(json!({"data": [{"item": {"type": "userMessage", "clientId": "m1"}}]})));
    let next = deadline_decision(&record, &runtime, &NoStores, t0() + chrono::Duration::seconds(61)).unwrap();
    assert_eq!(next.state, InputState::InContext);
    // Unreadable → uncertain{receiptTimeout}.
    let unreadable = FakeRuntime::connected(None).answer("thread/items/list", Err("history unreadable".into()));
    let next = deadline_decision(&record, &unreadable, &NoStores, t0() + chrono::Duration::seconds(61)).unwrap();
    assert_eq!((next.state, next.reason.as_deref()), (InputState::Uncertain, Some("receiptTimeout")));
}

struct NoStores;
impl OwnerStores for NoStores {
    fn queued(&self, _: &str, _: &str) -> bool { false }
    fn held(&self, _: &str, _: &str) -> bool { false }
    fn native_delivery(&self, _: &str, _: &str) -> OwnerDelivery { OwnerDelivery::None }
}

struct Stores(OwnerDelivery, bool, bool);
impl OwnerStores for Stores {
    fn queued(&self, _: &str, _: &str) -> bool { self.1 }
    fn held(&self, _: &str, _: &str) -> bool { self.2 }
    fn native_delivery(&self, _: &str, _: &str) -> OwnerDelivery { self.0.clone() }
}

#[test]
fn owner_deadlines_follow_the_owner_stores() {
    let f = Fixture::new();
    let runtime = FakeRuntime::connected(None);
    f.go("r1", Next::received().create(owner_input()));
    let received = f.get("r1");
    // A slow route (younger than a minute) keeps its record.
    let next = deadline_decision(&received, &runtime, &NoStores, t0() + chrono::Duration::seconds(6)).unwrap();
    assert_eq!(next.state, InputState::Received);
    let next = deadline_decision(&received, &runtime, &NoStores, t0() + chrono::Duration::seconds(61)).unwrap();
    assert_eq!((next.state, next.reason.as_deref()), (InputState::Failed, Some("routeLost")));
    let next = deadline_decision(&received, &runtime, &Stores(OwnerDelivery::None, true, false), t0() + chrono::Duration::seconds(61)).unwrap();
    assert_eq!((next.state, next.deadline.clone()), (InputState::Received, Some(None)), "still queued: no deadline");
    // sending: C-010 decides.
    f.go("r1", Next::sending());
    let sending = f.get("r1");
    let at = t0() + chrono::Duration::seconds(31);
    assert_eq!(deadline_decision(&sending, &runtime, &Stores(OwnerDelivery::Unknown, false, false), at).unwrap().reason.as_deref(), Some("outcomeUnknown"));
    let accepted = deadline_decision(&sending, &runtime, &Stores(OwnerDelivery::Accepted(Some("t9".into())), false, false), at).unwrap();
    assert_eq!((accepted.state, accepted.turn_id.as_deref()), (InputState::Accepted, Some("t9")));
    assert_eq!(deadline_decision(&sending, &runtime, &Stores(OwnerDelivery::Sending, false, false), at).unwrap().state, InputState::Sending);
}

#[test]
fn disconnect_pauses_and_reconnect_checks_once() {
    let f = Fixture::new();
    accepted_owner(&f, "running", "t1");
    accepted_owner(&f, "ended", "t0");
    f.go("sending", Next::received().create(owner_input()));
    f.go("sending", Next::sending());
    assert!(f.get("sending").deadline.is_some());
    il::capture_events(|| disconnected(&f.ledger, "s1", t0()));
    for id in ["running", "ended", "sending"] {
        let record = f.get(id);
        assert_eq!(record.stalled.as_ref().map(|s| s.reason.as_str()), Some("runtimeDisconnected"), "{id}");
        assert!(record.deadline.is_none(), "{id}: deadlines pause");
    }
    assert_eq!(f.get("running").state, InputState::Accepted, "state unchanged");
    // Same generation: t1 still runs, t0 ended while away (one items read).
    let runtime = FakeRuntime::connected(Some("t1")).answer("thread/items/list", Ok(json!({"data": [
        {"item": {"type": "userMessage", "clientId": "ended"}}]})));
    il::capture_events(|| reconnected(&f.ledger, &runtime, "s1", false, t0()));
    assert_eq!(f.get("running").state, InputState::Accepted);
    assert!(f.get("running").stalled.is_none());
    assert_eq!(f.get("ended").state, InputState::InContext);
    assert!(f.get("sending").stalled.is_none() && f.get("sending").deadline.is_some(), "sending deadline restarts");
    assert_eq!(runtime.calls.borrow().len(), 1, "one check, no history scan");
    // Another generation: what cannot be proven is runtimeLost.
    il::capture_events(|| disconnected(&f.ledger, "s1", t0()));
    let lost = FakeRuntime::connected(None).answer("thread/items/list", Err("gone".into()));
    il::capture_events(|| reconnected(&f.ledger, &lost, "s1", true, t0()));
    assert_eq!(f.get("running").reason.as_deref(), Some("runtimeLost"));
    assert_eq!(f.get("sending").reason.as_deref(), Some("runtimeLost"));
}

#[test]
fn restart_reconcile_pages_through_unsettled_records_in_batches() {
    let f = Fixture::new();
    for i in 0..(2 * BATCH + 50) {
        let session = format!("s{}", i % 3);
        il::capture_events(|| f.ledger.transition(&session, &format!("n{i:04}"), Expect::ANY, Next::waiting("afterTurn").create(note_input()))).0.unwrap();
    }
    f.go("done", Next::received().create(owner_input()));
    f.go("done", Next::failed("routeLost"));
    f.go("unsure", Next::received().create(owner_input()));
    f.go("unsure", Next::sending());
    f.go("unsure", Next::uncertain("outcomeUnknown"));
    let mut cursor: Option<(String, String)> = None;
    let mut pages = Vec::new();
    loop {
        let page = f.ledger.unsettled_page(cursor.as_ref(), BATCH).unwrap();
        pages.push(page.len());
        if page.len() < BATCH {
            break;
        }
        let last = page.last().unwrap();
        cursor = Some((last.cutex_session_id.clone(), last.input_id.clone()));
    }
    assert_eq!(pages, vec![BATCH, BATCH, 50], "settled and uncertain records are not reconciled");
}

/// S19 codex 1/10: the engine started late (under load) and its first
/// restart page met a steer this process had just put in `sending`; the
/// restart rule made it `uncertain{outcomeUnknown}` 22 ms later although the
/// native reply was still coming. Only records an earlier process left are
/// restart leftovers.
#[test]
fn restart_reconcile_leaves_records_this_process_wrote() {
    let f = Fixture::new();
    f.go("left", Next::received().create(owner_input()));
    f.go("left", Next::sending());
    accepted_owner(&f, "left-accepted", "t1");
    let process_started = t0() + chrono::Duration::seconds(1);
    f.ledger.set_clock(process_started + chrono::Duration::milliseconds(36));
    f.go("live", Next::received().create(owner_input()));
    f.go("live", Next::sending());
    accepted_owner(&f, "live-accepted", "t2");
    let in_flight = Stores(OwnerDelivery::Sending, false, false);
    assert_eq!(restart_next(&f.get("live"), &in_flight, process_started), Restart::Skip, "a live send is not judged");
    assert_eq!(restart_next(&f.get("live-accepted"), &in_flight, process_started), Restart::Skip);
    assert_eq!(
        restart_next(&f.get("left"), &in_flight, process_started),
        Restart::Apply(Next::uncertain("outcomeUnknown")),
        "a send an earlier process left in flight is C-010 unknown"
    );
    assert_eq!(restart_next(&f.get("left-accepted"), &in_flight, process_started), Restart::ReconnectCheck);
    let accepted = Stores(OwnerDelivery::Accepted(Some("t1".into())), false, false);
    assert_eq!(restart_next(&f.get("left"), &accepted, process_started), Restart::Apply(Next::accepted().turn("t1")));
}

#[test]
fn owner_drift_follows_the_stores_after_the_grace() {
    let f = Fixture::new();
    f.go("q", Next::received().create(owner_input()));
    f.go("q", Next::waiting("explicitQueue"));
    let record = f.get("q");
    let young = t0() + chrono::Duration::seconds(5);
    assert!(owner_drift(&record, &NoStores, young).is_empty(), "never right after a write");
    let old = t0() + chrono::Duration::seconds(120);
    let sent = owner_drift(&record, &Stores(OwnerDelivery::Accepted(Some("t3".into())), false, false), old);
    assert_eq!(sent.iter().map(|next| next.state).collect::<Vec<_>>(), vec![InputState::Sending, InputState::Accepted]);
    assert_eq!(owner_drift(&record, &NoStores, old)[0].reason.as_deref(), Some("ownerRemoved"));
    assert!(owner_drift(&record, &Stores(OwnerDelivery::None, true, false), old).is_empty(), "still queued");
}

#[test]
fn retry_moves_failed_or_uncertain_back_to_waiting_with_the_same_id() {
    let f = Fixture::new();
    f.go("m1", Next::received().create(owner_input()));
    f.go("m1", Next::held("hostBusy"));
    let failed = f.go("m1", Next::failed("admission_timeout")).record;
    let conflict = retry_transition(&f.ledger, "s1", "m1", failed.rev + 1).unwrap_err();
    assert_eq!(conflict.code, "revision_conflict");
    let (retried, events) = il::capture_events(|| retry_transition(&f.ledger, "s1", "m1", failed.rev));
    let retried = retried.unwrap().record;
    assert_eq!((retried.state, retried.reason.as_deref()), (InputState::Waiting, Some("ownerRetry")));
    assert_eq!(retried.attempt, failed.attempt + 1);
    assert_eq!(retried.surface, Surface::Timeline, "an idle admission failure stays timeline on retry (owner 2026-11-04)");
    assert_eq!(events.len(), 1);
    let again = retry_transition(&f.ledger, "s1", "m1", retried.rev).unwrap_err();
    assert_eq!(again.code, "input_not_retryable");
    assert_eq!(retry_transition(&f.ledger, "s1", "nope", 1).unwrap_err().code, "input_not_found");
    // Unknown outcomes keep r274.2's explicit-resend rule.
    f.go("u1", Next::received().create(owner_input()));
    f.go("u1", Next::sending());
    let uncertain_owner = f.go("u1", Next::uncertain("outcomeUnknown")).record;
    let resent = il::capture_events(|| retry_transition(&f.ledger, "s1", "u1", uncertain_owner.rev)).0.unwrap().record;
    assert_eq!(resent.surface, Surface::NextUp);
    // A bus note goes again under the same id (054 §2.10); the record says
    // it may arrive twice, before and after the retry.
    accepted_note(&f, "n1", Some("t1"));
    let uncertain = f.go("n1", Next::uncertain("turnInterrupted")).record;
    assert_eq!(uncertain.details["mayArriveTwice"], true);
    assert!(uncertain.actions.contains(&il::Action::Retry));
    // The owning store refuses (already settled there): the record stays.
    let refused = retry_transition_checked(&f.ledger, "s1", "n1", uncertain.rev, &|_| {
        Err(retry_error("input_not_retryable", "settled".into(), json!({})))
    });
    assert_eq!(refused.unwrap_err().code, "input_not_retryable");
    assert_eq!(f.get("n1").rev, uncertain.rev);
    let note = il::capture_events(|| retry_transition(&f.ledger, "s1", "n1", uncertain.rev)).0.unwrap().record;
    assert_eq!((note.state, note.reason.as_deref(), note.attempt), (InputState::Waiting, Some("ownerRetry"), 2));
    assert_eq!(note.details["mayArriveTwice"], true);
    assert_eq!(note.turn_id, None);
    // The owner input never says so (same clientUserMessageId, deduplicated).
    assert!(retried.details.get("mayArriveTwice").is_none());
}

#[test]
fn bus_and_result_records_are_retryable_like_the_owner_s() {
    let f = Fixture::new();
    for (id, kind) in [("r1", InputKind::DelegationResult), ("q1", InputKind::DelegationQuestion), ("a1", InputKind::Alarm)] {
        let input = NewInput { kind, ..note_input() };
        f.go(id, Next::waiting("afterTurn").create(input));
        f.go(id, Next::sending());
        let record = f.go(id, Next::uncertain("outcomeUnknown")).record;
        assert!(retryable(&record), "{kind:?}");
        assert_eq!(record.details["mayArriveTwice"], true, "{kind:?}");
        let retried = il::capture_events(|| retry_transition(&f.ledger, "s1", id, record.rev)).0.unwrap().record;
        assert_eq!((retried.state, retried.attempt), (InputState::Waiting, 2), "{kind:?}");
        // The delivery goes on from waiting{ownerRetry}: sending, accepted.
        f.go(id, Next::sending());
        let accepted = f.go(id, Next::accepted().turn("t9")).record;
        assert_eq!((accepted.state, accepted.attempt), (InputState::Accepted, 2));
        assert!(accepted.details.get("mayArriveTwice").is_none(), "only on the retry offer and the retried record");
    }
}

#[test]
fn stalled_no_progress_marks_an_idle_waiting_record_and_clears_when_busy() {
    let f = Fixture::new();
    let stall = Duration::from_secs(30);
    let wake = f.go("w1", Next::waiting("afterTurn").create(note_input())).record;
    let note = f.go("p1", Next::waiting("passive").create(note_input())).record;
    let now = t0() + chrono::Duration::seconds(10);
    // Too young; the next look is due when it turns 30 s old.
    assert!(stall_next(&wake, true, now, stall).is_none());
    assert_eq!(next_stall_check(&[wake.clone(), note.clone()], true, stall), Some(t0() + chrono::Duration::seconds(30)));
    assert_eq!(next_stall_check(std::slice::from_ref(&wake), false, stall), None, "busy: nothing to look at");
    let late = t0() + chrono::Duration::seconds(31);
    // A passive note waits for a turn by design; a busy session is no stall.
    assert!(stall_next(&note, true, late, stall).is_none());
    assert!(stall_next(&wake, false, late, stall).is_none());
    let next = stall_next(&wake, true, late, stall).unwrap();
    let marked = il::capture_events(|| f.ledger.transition("s1", "w1", Expect::rev(wake.rev), next)).0.unwrap();
    assert!(marked.changed);
    let record = marked.record;
    assert_eq!((record.state, record.reason.as_deref()), (InputState::Waiting, Some("afterTurn")), "reason kept");
    assert_eq!(record.stalled.as_ref().map(|s| s.reason.as_str()), Some("noProgress"));
    assert!(stall_next(&record, true, late, stall).is_none(), "marked once");
    // The session got busy: the mark goes.
    let cleared = f.go("w1", stall_next(&record, false, late, stall).unwrap()).record;
    assert_eq!((cleared.state, cleared.reason.as_deref(), cleared.stalled.clone()), (InputState::Waiting, Some("afterTurn"), None));
    // Any state change clears it too.
    let again = f.go("w1", Next::waiting("afterTurn").stalled(Some(Stalled { reason: "noProgress".into(), since: "x".into() }))).record;
    assert!(again.stalled.is_some());
    assert!(f.go("w1", Next::sending()).record.stalled.is_none());
}

#[test]
fn delivery_reports_starting_while_a_start_is_pending() {
    let now = t0();
    let state = |facts: DeliveryFacts| session_delivery(&facts, now)["state"].as_str().unwrap().to_string();
    assert_eq!(state(DeliveryFacts { starting: true, ..DeliveryFacts::default() }), "starting");
    assert_eq!(state(facts(false, None).tap(|f| f.starting = true)), "starting");
    // Connected: the start is over, the usual order decides.
    assert_eq!(state(facts(true, None).tap(|f| f.starting = true)), "idle");
    assert_eq!(state(DeliveryFacts::default()), "offline");
}

#[test]
fn bus_drift_corrects_only_a_state_the_store_proves() {
    use crate::management::v2::input_ledger_bus::{BusFacts, Target};
    let f = Fixture::new();
    let facts = |state: &str, inbox: bool| BusFacts {
        state: state.into(), timing: Timing::AfterTurn, frozen: false, a2: false, inbox, a4: false,
        external_receipt: false, observed: None, turn: Some("t1".into()), settlement_closed: false, error_code: None, item: None,
    };
    let waiting = f.go("b1", Next::waiting("hostBusy").create(note_input())).record;
    let view = |facts: BusFacts| Some(("s1".to_string(), facts, note_input()));
    let late = t0() + chrono::Duration::seconds(DRIFT_MIN_AGE_SECS + 1);
    // Young records are never drifted (their writer may be between stores).
    assert_eq!(bus_drift(&waiting, view(facts("inbox_accepted", true)).as_ref(), t0()), None);
    // The store took it into the inbox: accepted.
    assert_eq!(bus_drift(&waiting, view(facts("inbox_accepted", true)).as_ref(), late), Some(Target::Accepted { turn: None }));
    // Same state, another reason: the store does not judge waiting reasons.
    assert_eq!(bus_drift(&waiting, view(facts("pending", false)).as_ref(), late), None);
    // Another recipient, or no message: nothing.
    assert_eq!(bus_drift(&waiting, Some(("s2".to_string(), facts("delivered", false), note_input())).as_ref(), late), None);
    assert_eq!(bus_drift(&waiting, None, late), None);
    // Delivered while the ledger said uncertain: inContext (the only upgrade).
    f.go("b1", Next::sending());
    let uncertain = f.go("b1", Next::uncertain("turnInterrupted")).record;
    assert!(matches!(bus_drift(&uncertain, view(facts("delivered", false)).as_ref(), late), Some(Target::InContext { .. })));
    // An uncertain record is never pulled back to accepted by a receipt.
    assert_eq!(bus_drift(&uncertain, view(facts("inbox_accepted", true)).as_ref(), late), None);
}

#[test]
fn retry_params_are_strict() {
    assert_eq!(parse_retry("s1", &json!({"inputId": "m1", "expectedRev": 3})).unwrap(), ("m1".to_string(), 3));
    assert!(parse_retry("s1", &json!({"inputId": "m1", "expectedRev": 3, "cutexSessionId": "s1"})).is_ok());
    for bad in [json!({"inputId": "m1"}), json!({"inputId": "", "expectedRev": 1}), json!({"inputId": "m1", "expectedRev": 0}),
        json!({"inputId": "m1", "expectedRev": 1, "cutexSessionId": "s2"}), json!({"inputId": "m1", "expectedRev": 1, "x": 1})] {
        assert_eq!(parse_retry("s1", &bad).unwrap_err().code, "invalid_request", "{bad}");
    }
}

#[test]
fn a_lost_ledger_gets_a_new_epoch_and_records_the_loss() {
    let f = Fixture::new();
    let path = f.root.join("lost").join(il::DATABASE_FILE);
    let reopen = || { let ledger = InputLedger::open(&path); ledger.set_clock(t0()); ledger };
    let ledger = reopen();
    il::capture_events(|| ledger.transition("s1", "q", Expect::ANY, Next::waiting("explicitQueue").create(owner_input()))).0.unwrap();
    let first = ledger.epoch().unwrap();
    assert!(ledger.pending_losses().unwrap().is_empty());
    drop(ledger);
    // The process restarts with the same file: nothing is lost.
    let same = reopen();
    assert_eq!(same.epoch().unwrap(), first);
    assert!(same.pending_losses().unwrap().is_empty());
    drop(same);
    // The file is lost between runs.
    std::fs::remove_file(&path).unwrap();
    let lost = reopen();
    let second = lost.epoch().unwrap();
    assert_ne!(first, second, "a new ledgerEpoch");
    let losses = lost.pending_losses().unwrap();
    assert_eq!(losses.len(), 1);
    assert_eq!((losses[0].previous_epoch.as_str(), losses[0].epoch.as_str()), (first.as_str(), second.as_str()));
    lost.loss_handled(losses[0].id).unwrap();
    assert!(lost.pending_losses().unwrap().is_empty());
    drop(lost);
    // A corrupt file is moved aside and replaced the same way.
    std::fs::write(&path, vec![0x5au8; 8192]).unwrap();
    let replaced = reopen();
    let third = replaced.epoch().unwrap();
    assert_ne!(third, second);
    assert_eq!(replaced.pending_losses().unwrap().len(), 1);
}

#[test]
fn retry_payloads_are_kept_and_compacted() {
    let f = Fixture::new();
    f.ledger.put_payload("s1", "m1", &json!({"clientUserMessageId": "m1", "input": []})).unwrap();
    assert_eq!(f.ledger.payload("s1", "m1").unwrap().unwrap()["clientUserMessageId"], "m1");
    assert!(f.ledger.payload("s1", "m2").unwrap().is_none());
    let later = t0() + chrono::Duration::days(8);
    assert_eq!(f.ledger.compact_payloads(later, PAYLOAD_RETENTION, 10).unwrap(), 1);
    assert!(f.ledger.payload("s1", "m1").unwrap().is_none());
}

#[test]
fn switch_off_adds_nothing_and_refuses_retry() {
    let pin = il::pin_enabled(Some(false));
    let mut state = serde_json::Map::new();
    state.insert("latestTokenUsage".into(), Value::Null);
    add_bootstrap_fields(&mut state, "s1");
    assert_eq!(Value::Object(state), json!({"latestTokenUsage": null}), "bootstrap is r273.1's");
    let mut resubmitted = false;
    let error = retry(true, "s1", &json!({"inputId": "m1", "expectedRev": 1}), &mut |_| {
        resubmitted = true;
        Ok(json!({}))
    }).unwrap_err();
    assert_eq!(error.code, "cutex_method_denied");
    assert!(!resubmitted);
    // The dirty set is process-wide and other tests fill it: check only a
    // session no other test uses.
    let session = format!("switch-off-{}", uuid::Uuid::new_v4().simple());
    mark_dirty(&session);
    assert!(!take_dirty().contains(&session), "no settle work while off");
    drop(pin);
}

// ---------------------------------------------------------------------------
// r274.1: a check that proves a record persisted names its native item.

#[test]
fn a_persisted_check_binds_the_native_item() {
    let f = Fixture::new();
    accepted_owner(&f, "m1", "t1");
    accepted_note(&f, "n1", None);
    // Owner: the userMessage item the turn read found.
    let runtime = FakeRuntime::connected(None).answer("thread/items/list", Ok(json!({"data": [
        {"turnId": "t1", "item": {"type": "userMessage", "id": "native-user-1", "clientId": "m1"}}]})));
    let (changed, _) = il::capture_events(|| turn_ended(&f.ledger, &runtime, "s1", "t1", "interrupted", t0()));
    assert_eq!(changed, 1);
    let owner = f.get("m1");
    assert_eq!((owner.state, owner.timeline_item_id.as_deref()), (InputState::InContext, Some("native-user-1")));
    // Bus (reconnect / deadline check): an ExternalInput / local inbox item
    // carries the message id.
    let mut runtime = FakeRuntime::connected(None).answer("thread/externalInput/status", Ok(status("context_persisted", "none", None)));
    runtime.external.insert("n1".into(), ("s1".into(), "thread-1".into(), json!({"messageId": "n1"})));
    let note = f.get("n1");
    let next = deadline_decision(&note, &runtime, &NoStores, t0() + chrono::Duration::seconds(61)).unwrap();
    assert_eq!((next.state, next.timeline_item_id.as_deref()), (InputState::InContext, Some("n1")));
    let (changed, _) = il::capture_events(|| reconnected(&f.ledger, &runtime, "s1", true, t0()));
    assert_eq!(changed, 1);
    assert_eq!(f.get("n1").timeline_item_id.as_deref(), Some("n1"));
    // Not persisted: no item.
    let absent = FakeRuntime::connected(None).answer("thread/items/list", Ok(json!({"data": []})));
    assert_eq!(check_bound(&absent, &owner, Some("t1")), (Check::Absent, None));
    assert_eq!(parse_turn_items_bound(&json!({"data": [{"item": {"type": "userMessage", "clientId": "x"}}]}), "x"),
        (Check::Persisted(Evidence::HistoryRead), None), "an item without an id binds nothing");
}

/// r274.2 §9.4: the owner's retry of a record a lost runtime left
/// `accepted` (`stalled{runtimeDisconnected}`): `uncertain{runtimeLost}`
/// first, then `waiting{ownerRetry}` on nextUp. Nothing resends it before.
#[test]
fn retry_of_a_stuck_owner_record_goes_through_uncertain() {
    let f = Fixture::new();
    f.go("m1", Next::received().create(owner_input()).surface(Surface::NextUp));
    f.go("m1", Next::sending());
    f.go("m1", Next::accepted().turn("t1"));
    let stalled = Stalled { reason: "runtimeDisconnected".into(), since: "2026-11-03T00:00:00.000Z".into() };
    let stuck = f.go("m1", Next::accepted().stalled(Some(stalled)).deadline(None)).record;
    assert!(retryable(&stuck));
    let (retried, events) = il::capture_events(|| retry_transition(&f.ledger, "s1", "m1", stuck.rev));
    let retried = retried.unwrap().record;
    assert_eq!((retried.state, retried.reason.as_deref(), retried.surface), (InputState::Waiting, Some("ownerRetry"), Surface::NextUp));
    assert_eq!((retried.attempt, retried.stalled.clone(), retried.turn_id.clone()), (2, None, None));
    let states: Vec<(String, String)> = events.iter()
        .map(|(_, _, params)| (params["record"]["state"].as_str().unwrap().to_string(), params["record"]["reason"].as_str().unwrap_or("").to_string()))
        .collect();
    assert_eq!(states, [("uncertain".to_string(), "runtimeLost".to_string()), ("waiting".to_string(), "ownerRetry".to_string())]);
    // A stale rev refuses before anything is written.
    let f2 = Fixture::new();
    f2.go("m2", Next::received().create(owner_input()));
    f2.go("m2", Next::sending());
    let stuck = f2.go("m2", Next::sending().stalled(Some(Stalled { reason: "runtimeDisconnected".into(), since: "2026-11-03T00:00:00.000Z".into() }))).record;
    assert_eq!(retry_transition(&f2.ledger, "s1", "m2", stuck.rev + 1).unwrap_err().code, "revision_conflict");
    assert_eq!(f2.get("m2").rev, stuck.rev);
}

// ---------------------------------------------------------------------------
// r274.2 §9.4 action gaps: owner records that will not progress by themselves

fn connected(generation: u64, turn: Option<&str>) -> RuntimeView {
    RuntimeView {
        present: true,
        connected: true,
        generation: Some(generation),
        thread_id: Some("th".into()),
        active_turn: turn.map(str::to_string),
        ..RuntimeView::default()
    }
}

/// An owner steer accepted into `turn` by runtime generation `generation`.
fn steered(f: &Fixture, id: &str, turn: &str, generation: Option<u64>) -> InputRecord {
    f.go(id, Next::received().create(owner_input()).surface(Surface::NextUp));
    f.go(id, Next::sending());
    f.go(id, Next { runtime_generation: generation, ..Next::accepted().turn(turn) }).record
}

const QUIET: Duration = Duration::from_secs(30);

fn later(secs: i64) -> DateTime<Utc> {
    t0() + chrono::Duration::seconds(secs)
}

#[test]
fn a_later_runtime_generation_leaves_an_accepted_owner_steer_stuck() {
    let f = Fixture::new();
    let record = steered(&f, "g", "t1", Some(3));
    assert_eq!(record.runtime_generation, Some(3));
    // Same generation, its turn running: it may still be read.
    assert_eq!(owner_stuck_case(&record, &connected(3, Some("t1")), None, false, later(0), QUIET), None);
    // A later generation is connected: never read by it, at once.
    assert_eq!(owner_stuck_case(&record, &connected(4, Some("t9")), None, false, later(0), QUIET), Some("generationChanged"));
    // Not connected: the disconnect path (stalled{runtimeDisconnected}) owns it.
    let away = RuntimeView { connected: false, ..connected(4, None) };
    assert_eq!(owner_stuck_case(&record, &away, None, false, later(0), QUIET), None);
    // Unknown generation (accepted before r274.2 stamped it): not this case.
    let old = steered(&f, "old", "t1", None);
    assert_eq!(owner_stuck_case(&old, &connected(4, Some("t9")), None, false, later(0), QUIET), None);
}

#[test]
fn a_stale_lease_on_the_record_turn_leaves_it_stuck() {
    let f = Fixture::new();
    let record = steered(&f, "l", "t1", Some(1));
    let lease = |state: &str, turn: &str| json!({"holder": "ask", "turnId": turn, "inputIds": ["ask-1"], "reconcile": {"state": state}});
    let view = connected(1, Some("t1"));
    assert_eq!(owner_stuck_case(&record, &view, Some(&lease("checking", "t1")), false, later(0), QUIET), None, "still being checked");
    assert_eq!(owner_stuck_case(&record, &view, Some(&lease("stale", "t1")), false, later(0), QUIET), Some("leaseStale"));
    assert_eq!(owner_stuck_case(&record, &view, Some(&lease("stale", "t2")), false, later(0), QUIET), None, "the lease of another turn");
}

#[test]
fn an_orphan_accepted_owner_record_is_stuck_only_when_nothing_will_read_or_settle_it() {
    let f = Fixture::new();
    // The Alice case: accepted (Claude registered), no turn, no deadline,
    // not queued, never shown to the agent.
    f.go("o", Next::received().create(owner_input()).surface(Surface::NextUp));
    f.go("o", Next::sending());
    let record = f.go("o", Next { runtime_generation: Some(7), ..Next::accepted().stage(il::Stage::Registered) }).record;
    let idle = connected(7, None);
    assert_eq!(owner_stuck_case(&record, &idle, None, false, later(31), QUIET), Some("orphan"),"an unbound idle registration retains its original orphan diagnosis");
    // Never a record about to be read or settled:
    assert_eq!(owner_stuck_case(&record, &idle, None, false, later(5), QUIET), None, "just accepted");
    assert_eq!(owner_stuck_case(&record, &connected(7, Some("t2")), None, false, later(31), QUIET), None, "a turn runs");
    assert_eq!(owner_stuck_case(&record, &idle, None, true, later(31), QUIET), None, "still in the Management queue");
    let checked = f.go("o", Next::accepted().deadline(Some(later(40)))).record;
    assert_eq!(owner_stuck_case(&checked, &idle, None, false, later(31), QUIET), None, "its receipt check will settle it");
    let f2 = Fixture::new();
    f2.go("x", Next::received().create(owner_input()));
    f2.go("x", Next::sending());
    let offered = f2.go("x", Next::accepted().stage(il::Stage::Offered)).record;
    assert_eq!(owner_stuck_case(&offered, &idle, None, false, later(31), QUIET), None, "the agent was shown it");
    // The timer: one check when the quiet period of a candidate ends.
    assert_eq!(next_orphan_check(std::slice::from_ref(&record), later(1), QUIET), Some(later(30)));
    assert_eq!(next_orphan_check(std::slice::from_ref(&record), later(31), QUIET), None);
}

#[test]
fn a_stuck_mark_gives_retry_and_dismiss_and_clears_when_the_record_can_move_again() {
    let f = Fixture::new();
    let record = steered(&f, "g", "t1", Some(3));
    assert!(record.actions.contains(&il::Action::Dismiss) && !il::owner_stuck(&record) && il::dismissible(&record));
    let mark = owner_stuck_next(&record, Some("generationChanged"), later(1)).expect("mark");
    let stuck = f.go("g", mark).record;
    assert_eq!(stuck.stalled.as_ref().map(|s| s.reason.as_str()), Some("noProgress"));
    assert_eq!(il::stuck_case(&stuck), Some("generationChanged"));
    assert_eq!(stuck.actions, vec![il::Action::Retry, il::Action::Dismiss]);
    assert!(il::owner_stuck(&stuck) && il::dismissible(&stuck) && retryable(&stuck));
    assert_eq!(stuck.surface, Surface::NextUp, "unread: stays on nextUp");
    // Idempotent; and cleared once the case no longer holds.
    assert_eq!(owner_stuck_next(&stuck, Some("generationChanged"), later(2)), None);
    let cleared = f.go("g", owner_stuck_next(&stuck, None, later(3)).expect("clear")).record;
    assert!(cleared.stalled.is_none() && cleared.actions.contains(&il::Action::Dismiss) && il::stuck_case(&cleared).is_none());
    // A noProgress stall without a known case gives no actions.
    let bare = f.go("g", Next::accepted().stalled(Some(Stalled { reason: "noProgress".into(), since: "2026-11-03T00:00:00.000Z".into() }))).record;
    assert!(bare.actions.contains(&il::Action::Dismiss) && !il::owner_stuck(&bare));
    // Never for another kind.
    f.go("n", Next::sending().create(note_input()));
    let note = f.go("n", Next::accepted()).record;
    assert_eq!(owner_stuck_next(&note, Some("orphan"), later(1)), None);
}

#[test]
fn dismiss_and_retry_of_each_stuck_case_go_through_uncertain() {
    for (case, unknown) in [("generationChanged", "runtimeLost"), ("leaseStale", "outcomeUnknown"), ("orphan", "outcomeUnknown")] {
        let f = Fixture::new();
        let record = steered(&f, "d", "t1", Some(1));
        let stuck = f.go("d", owner_stuck_next(&record, Some(case), later(1)).unwrap()).record;
        assert_eq!(il::stuck_uncertain_reason(&stuck), unknown, "{case}");
        let (done, events) = il::capture_events(|| il::dismiss_one(&f.ledger, "s1", "d", stuck.rev));
        let done = done.unwrap().record;
        assert_eq!((done.state, done.reason.as_deref(), done.surface), (InputState::Discarded, Some("ownerDiscarded"), Surface::None), "{case}");
        let states: Vec<String> = events.iter()
            .map(|(_, _, p)| format!("{}{{{}}}", p["record"]["state"].as_str().unwrap(), p["record"]["reason"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(states,["discarded{ownerDiscarded}".to_string()],"{case}");
        // Retry: the same way, then waiting{ownerRetry}. Nothing resends it
        // before the owner asks (no auto-redelivery).
        let f = Fixture::new();
        let record = steered(&f, "r", "t1", Some(1));
        let stuck = f.go("r", owner_stuck_next(&record, Some(case), later(1)).unwrap()).record;
        let retried = il::capture_events(|| retry_transition(&f.ledger, "s1", "r", stuck.rev)).0.unwrap().record;
        assert_eq!((retried.state, retried.reason.as_deref(), retried.surface), (InputState::Waiting, Some("ownerRetry"), Surface::NextUp), "{case}");
    }
    // A failed owner record is dismissed directly (failed -> discarded).
    let f = Fixture::new();
    f.go("f", Next::received().create(owner_input()));
    f.go("f", Next::sending());
    let failed = f.go("f", Next::failed("notDelivered")).record;
    assert!(il::dismissible(&failed));
    let done = il::capture_events(|| il::dismiss_one(&f.ledger, "s1", "f", failed.rev)).0.unwrap().record;
    assert_eq!((done.state, done.reason.as_deref()), (InputState::Discarded, Some("ownerDiscarded")));
}

#[test]
fn an_owner_steer_records_the_runtime_generation_that_accepted_it() {
    use crate::management::v2::input_ledger_owner::{plan, Step};
    let f = Fixture::new();
    f.go("s", Next::received().create(owner_input()));
    f.go("s", Next::sending());
    let step = Step::Steered { turn_id: Some("t1".into()), stage: Some(il::Stage::Registered), generation: Some(5) };
    let nexts = plan(Some(&f.get("s")), &step, None);
    let last = nexts.last().expect("accepted");
    assert_eq!((last.state, last.runtime_generation, last.stage), (InputState::Accepted, Some(5), Some(il::Stage::Registered)));
    let record = f.go("s", last.clone()).record;
    assert_eq!(record.runtime_generation, Some(5));
}

/// r274.2: a bus wake shown in its own turn (Claude native item, or a
/// results event that started the turn) labels the turn `delivery` before
/// its receipt; `accepted` + turn alone, or a passive note, does not.
#[test]
fn a_wake_turn_shown_its_record_is_a_delivery_turn() {
    let f = Fixture::new();
    let signal = |signal: &str| Next::accepted().turn("t1").details(json!({il::READ_SIGNAL: signal}));
    // A Claude Task notice: shown by its native item.
    f.go("ctask-1", Next::waiting("afterTurn").create(NewInput { kind: InputKind::SystemNotice, ..note_input() }));
    f.go("ctask-1", Next::sending());
    f.go("ctask-1", Next::accepted().turn("t1"));
    assert_eq!(turn_inputs(&f.ledger, "s1", "t1"), (None, vec!["ctask-1".to_string()]), "accepted + turn alone");
    f.go("ctask-1", signal("nativeItem"));
    assert_eq!(turn_inputs(&f.ledger, "s1", "t1"), (Some("delivery".into()), vec!["ctask-1".to_string()]));
    // A results event that started its turn.
    f.go("r1", Next::waiting("afterTurn").create(NewInput { kind: InputKind::DelegationResult, ..note_input() }));
    f.go("r1", Next::sending());
    f.go("r1", Next::accepted().turn("t2").details(json!({il::READ_SIGNAL: "turnStart"})));
    assert_eq!(turn_inputs(&f.ledger, "s1", "t2"), (Some("delivery".into()), vec!["r1".to_string()]));
    // A passive note read at a tool boundary rides along: not the turn's cause.
    let passive = NewInput {
        intent: InputIntent { strategy: None, delivery_mode: None, timing: Timing::Passive },
        ..note_input()
    };
    f.go("n1", Next::waiting("passive").create(passive));
    f.go("n1", Next::sending());
    f.go("n1", Next::accepted().turn("t3").details(json!({il::READ_SIGNAL: "nativeItem"})));
    assert_eq!(turn_inputs(&f.ledger, "s1", "t3"), (None, vec!["n1".to_string()]));
    // An owner steer shown by its echo stays an owner turn.
    accepted_owner(&f, "o1", "t4");
    f.go("o1", Next::accepted().turn("t4").details(json!({il::READ_SIGNAL: "steerEcho"})));
    assert_eq!(turn_inputs(&f.ledger, "s1", "t4").0.as_deref(), Some("owner"));
}

// Stuck resend (owner report 2026-11-07).

fn refused_resend() -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: "user_input_in_progress".into(),
        message: "an earlier submit of this clientUserMessageId has not resolved yet; nothing was sent, retry later".into(),
        retryable: true,
        details: json!({"delivery": "unknown"}),
        outcome_unknown: false,
    }
}

#[test]
fn a_refused_owner_resend_returns_to_retry_and_dismiss_and_says_nothing_was_sent() {
    let f = Fixture::new();
    let payload = json!({"clientUserMessageId": "m", "strategy": "auto", "input": []});
    let settled: [(&str, Next); 2] = [("uncertain", Next::uncertain("outcomeUnknown")), ("failed", Next::failed("notDelivered"))];
    for (id, last) in settled {
        f.go(id, Next::received().create(owner_input()));
        f.go(id, Next::sending());
        let before = f.go(id, last).record;
        f.ledger.put_payload("s1", id, &payload).unwrap();
        let mut sent = 0;
        let (result, _) = il::capture_events(|| retry_on(&f.ledger, "s1", id, before.rev, &mut |_| {
            sent += 1;
            Err(refused_resend())
        }));
        let result = result.unwrap();
        assert_eq!(sent, 1);
        assert_eq!(result["redelivery"]["ok"], false, "{result}");
        assert_eq!(result["redelivery"]["error"]["code"], "user_input_in_progress", "{result}");
        assert_eq!(result["redelivery"]["error"]["details"]["nothingSent"], true, "{result}");
        assert!(result["redelivery"]["error"]["message"].as_str().unwrap().contains("nothing was sent"), "{result}");
        let after = f.get(id);
        assert_eq!((after.state, after.reason.clone()), (before.state, before.reason.clone()), "back where it was");
        assert_eq!(after.actions, vec![il::Action::Retry, il::Action::Dismiss]);
        assert_eq!(result["record"]["state"], before.state.to_string());
        // The row keeps working: a later resend may go through.
        let (later, _) = il::capture_events(|| retry_on(&f.ledger, "s1", id, after.rev, &mut |_| Ok(json!({}))));
        assert_eq!(later.unwrap()["redelivery"]["ok"], true);
    }
    // No payload left: the same, not a row without actions.
    f.go("gone", Next::received().create(owner_input()));
    f.go("gone", Next::sending());
    let before = f.go("gone", Next::uncertain("outcomeUnknown")).record;
    let (result, _) = il::capture_events(|| retry_on(&f.ledger, "s1", "gone", before.rev, &mut |_| panic!("no payload to send")));
    assert_eq!(result.unwrap()["redelivery"]["error"]["details"]["nothingSent"], true);
    assert_eq!(f.get("gone").actions, vec![il::Action::Retry, il::Action::Dismiss]);
}

/// The row a refused resend left behind before this fix (r277): no queue
/// item, no held input, no timer. Marked by the stall check, it gets actions.
fn stranded_resend(f: &Fixture, id: &str) -> InputRecord {
    f.go(id, Next::received().create(owner_input()));
    f.go(id, Next::sending());
    f.go(id, Next::uncertain("outcomeUnknown"));
    let waiting = f.go(id, Next::waiting("ownerRetry")).record;
    let late = t0() + chrono::Duration::seconds(31);
    f.go(id, stall_next(&waiting, true, late, Duration::from_secs(30)).unwrap()).record
}

#[test]
fn a_stranded_owner_resend_can_be_retried_or_dismissed() {
    let f = Fixture::new();
    let stranded = stranded_resend(&f, "a");
    assert_eq!(stranded.actions, vec![il::Action::Retry, il::Action::Dismiss]);
    let (retried, _) = il::capture_events(|| retry_transition(&f.ledger, "s1", "a", stranded.rev));
    let retried = retried.unwrap().record;
    assert_eq!((retried.state, retried.reason.as_deref()), (InputState::Waiting, Some("ownerRetry")));
    assert!(retried.stalled.is_none());

    let stranded = stranded_resend(&f, "b");
    let (dismissed, _) = il::capture_events(|| il::dismiss_one(&f.ledger, "s1", "b", stranded.rev));
    let dismissed = dismissed.unwrap().record;
    assert_eq!((dismissed.state, dismissed.reason.as_deref()), (InputState::Discarded, Some("ownerDiscarded")));

    // A resend still on its way has no mark and offers nothing.
    f.go("c", Next::received().create(owner_input()));
    f.go("c", Next::sending());
    f.go("c", Next::uncertain("outcomeUnknown"));
    assert!(f.go("c", Next::waiting("ownerRetry")).record.actions.contains(&il::Action::Dismiss));
}

#[test]
fn the_stall_mark_of_a_stranded_resend_does_not_flip_and_keeps_the_real_wait() {
    let f = Fixture::new();
    let stall = Duration::from_secs(30);
    f.go("a", Next::received().create(owner_input()));
    f.go("a", Next::sending());
    f.go("a", Next::uncertain("outcomeUnknown"));
    let waiting = f.go("a", Next::waiting("ownerRetry")).record;
    let marked = f.go("a", stall_next(&waiting, true, t0() + chrono::Duration::seconds(31), stall).unwrap()).record;
    assert_eq!(marked.stalled.as_ref().unwrap().since, waiting.since, "since the wait began, not since the last look");
    // Busy, idle, busy: no write at all.
    for minutes in 1..6 {
        let now = t0() + chrono::Duration::minutes(minutes);
        assert!(stall_next(&marked, minutes % 2 == 0, now, stall).is_none(), "minute {minutes}");
    }
    // Another waiting record still loses its mark when the session gets busy.
    let wake = f.go("w", Next::waiting("afterTurn").create(note_input())).record;
    let wake = f.go("w", stall_next(&wake, true, t0() + chrono::Duration::seconds(31), stall).unwrap()).record;
    assert_eq!(wake.stalled.as_ref().unwrap().since, wake.since);
    assert!(stall_next(&wake, false, t0() + chrono::Duration::seconds(40), stall).is_some());
}

#[test]
fn payloads_of_records_that_can_still_go_out_outlive_the_retention() {
    let f = Fixture::new();
    let payload = json!({"clientUserMessageId": "m", "input": []});
    for (id, last) in [("waits", Next::waiting("ownerRetry")), ("uncertain", Next::uncertain("outcomeUnknown")), ("failed", Next::failed("notDelivered")), ("done", Next::in_context(il::Evidence::TurnStartInput))] {
        f.go(id, Next::received().create(owner_input()));
        f.go(id, Next::sending());
        if id == "waits" {
            f.go(id, Next::uncertain("outcomeUnknown"));
        }
        f.go(id, last);
        f.ledger.put_payload("s1", id, &payload).unwrap();
    }
    f.ledger.put_payload("s1", "no-record", &payload).unwrap();
    let later = t0() + chrono::Duration::days(8);
    assert_eq!(f.ledger.compact_payloads(later, PAYLOAD_RETENTION, 100).unwrap(), 2, "the settled one and the one without a record");
    for id in ["waits", "uncertain", "failed"] {
        assert!(f.ledger.payload("s1", id).unwrap().is_some(), "{id}");
    }
    assert!(f.ledger.payload("s1", "done").unwrap().is_none());
    assert!(f.ledger.payload("s1", "no-record").unwrap().is_none());
}

fn carried_owner(f: &Fixture, id: &str, note: &str) {
    f.go(id, Next::received().create(NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "owner".into(),
        ask_id: None,
        remote: None,
    }));
    let mut sending = Next::sending();
    sending.carries = Some(vec![note.to_string()]);
    f.go(id, sending);
}

#[test]
fn a_carrier_is_over_when_it_cannot_read_the_note_any_more() {
    let f = Fixture::new();
    carried_owner(&f, "open", "n");
    let ask = || -> bool { panic!("no need to ask") };
    assert!(carrier_over("n", None, ask), "gone");
    assert!(!carrier_over("n", Some(&f.get("open")), ask), "in flight");
    assert!(carrier_over("other", Some(&f.get("open")), ask), "the note is not on this send any more");
    f.go("open", Next::uncertain("outcomeUnknown"));
    assert!(!carrier_over("n", Some(&f.get("open")), ask), "its outcome is open");
    carried_owner(&f, "dismissed", "n");
    f.go("dismissed", Next::uncertain("outcomeUnknown"));
    f.go("dismissed", Next::discarded("ownerDiscarded"));
    assert!(carrier_over("n", Some(&f.get("dismissed")), ask));
    carried_owner(&f, "failed", "n");
    f.go("failed", Next::failed("notDelivered"));
    assert!(!carrier_over("n", Some(&f.get("failed")), ask), "the owner can still resend or dismiss it");
    carried_owner(&f, "read", "n");
    f.go("read", Next::in_context(il::Evidence::TurnStartInput));
    assert!(!carrier_over("n", Some(&f.get("read")), || false), "its turn is running");
    assert!(carrier_over("n", Some(&f.get("read")), || true), "its turn ended and the note was not read");
}

// r279 review: the restart refresh rewrites a whole session's rows. A row on a
// later page must still count as left over from before this process, and a
// session that spans two pages is scanned once.
#[test]
fn the_restart_refresh_leaves_later_pages_to_be_judged_and_scans_a_session_once() {
    let f = Fixture::new();
    let delivery = json!({"state": "turn", "runtimeConnected": true, "turnId": "t1", "turn": {"turnId": "t1"}});
    f.ledger.delivery_changed("s1", &delivery).unwrap();
    for id in ["a", "b", "c"] {
        carried_owner(&f, id, "n");
        f.go(id, Next::accepted().turn("t1"));
    }
    // What an older build stored for them.
    let db = rusqlite::Connection::open(f.ledger.path()).unwrap();
    for id in ["a", "b", "c"] {
        let raw: String = db.query_row("SELECT record_json FROM input_ledger WHERE input_id=?1", [id], |r| r.get(0)).unwrap();
        let mut stored: Value = serde_json::from_str(&raw).unwrap();
        stored["actions"] = json!(["retry"]);
        db.execute("UPDATE input_ledger SET record_json=?1 WHERE input_id=?2", rusqlite::params![stored.to_string(), id]).unwrap();
    }
    drop(db);
    let started = t0() + chrono::Duration::seconds(50);
    f.ledger.set_clock(started + chrono::Duration::seconds(1));
    let first = f.ledger.unsettled_page(None, 2).unwrap();
    assert_eq!(first.len(), 2);
    let sessions = restart_refresh_sessions(&first, None);
    assert_eq!(sessions, ["s1"]);
    let change = f.ledger.refresh_open_actions("s1").unwrap();
    assert_eq!(change.records.len(), 3, "the whole session at once");
    let last = first.last().unwrap();
    let second = f.ledger.unsettled_page(Some(&(last.cutex_session_id.clone(), last.input_id.clone())), 2).unwrap();
    assert_eq!(second.len(), 1);
    assert!(second[0].actions.iter().all(|action| *action != il::Action::Retry), "refreshed: {:?}", second[0].actions);
    assert!(matches!(restart_next(&second[0], &NoStores, started), Restart::ReconnectCheck), "still left over");
    assert!(restart_refresh_sessions(&second, Some("s1")).is_empty(), "no second scan");
    assert_eq!(restart_refresh_sessions(&second, Some("s0")), ["s1"]);
}


#[test]
fn late_old_turn_end_does_not_interrupt_a_new_unbound_native_attempt() {
    let f=Fixture::new();
    accepted_note(&f,"source",None);
    let mut runtime=FakeRuntime::connected(Some("new-turn")).answer("thread/externalInput/status",Ok(status("pending","claimed",None)));
    runtime.external.insert("source".into(),("s1".into(),"thread-1".into(),json!({"messageId":"bus-retry:new:source"})));
    let (changed, _)=il::capture_events(||turn_ended(&f.ledger,&runtime,"s1","old-turn","interrupted",t0()));
    assert_eq!(changed,0);
    assert_eq!(f.get("source").state,InputState::Accepted);
    assert!(f.get("source").turn_id.is_none());
    assert_eq!(runtime.calls.borrow().len(),1,"one check of the current native attempt");
}

#[test]
fn late_terminal_check_binds_the_proven_native_retry_item_and_receipt_turn() {
    let f=Fixture::new();
    accepted_note(&f,"source",None);
    let native="bus-retry:11111111111141118111111111111111:source";
    let reply=json!({"statuses":[{"deliveryState":"context_persisted","messageId":native,
        "processing":{"state":"none"},"receipt":{"turnId":"new-turn","messageId":native,"responseItemId":native}}]});
    let mut runtime=FakeRuntime::connected(Some("new-turn")).answer("thread/externalInput/status",Ok(reply));
    runtime.external.insert("source".into(),("s1".into(),"thread-1".into(),json!({"messageId":native})));
    let (changed, _)=il::capture_events(||turn_ended(&f.ledger,&runtime,"s1","old-turn","interrupted",t0()));
    assert_eq!(changed,1);
    let row=f.get("source");
    assert_eq!(row.state,InputState::InContext);
    assert_eq!(row.timeline_item_id.as_deref(),Some(native));
    assert_eq!(row.turn_id.as_deref(),Some("new-turn"));
}

#[test]
fn rebuild_owner_source_preserves_placement_and_never_invents_explicit_queue() {
    use super::super::input_ledger_owner::SourceAdmission;
    use super::super::user_input::UserInputStrategy;
    let f=Fixture::new();
    let timeline=SourceAdmission{strategy:UserInputStrategy::Auto,surface:Surface::Timeline,reason:Some("hostBusy".into())};
    let nextup=SourceAdmission{strategy:UserInputStrategy::Interrupt,surface:Surface::NextUp,reason:Some("turnRunning".into())};
    let (_,events)=il::capture_events(|| {
        assert!(rebuild_owner_source(&f.ledger,"s1","idle-host",&[],Some(&timeline),None,true));
        assert!(rebuild_owner_source(&f.ledger,"s1","busy-host",&[],Some(&nextup),None,true));
        assert!(rebuild_owner_source(&f.ledger,"s1","legacy-unknown",&[],None,None,false));
    });
    assert!(!events.is_empty());
    assert_eq!(f.ledger.get("s1",&["idle-host".into()]).unwrap().records[0].surface,Surface::Timeline);
    assert_eq!(f.ledger.get("s1",&["busy-host".into()]).unwrap().records[0].surface,Surface::NextUp);
    let unknown=f.ledger.get("s1",&["legacy-unknown".into()]).unwrap().records.remove(0);
    assert_eq!(unknown.state,InputState::Uncertain);
    assert_eq!(unknown.reason.as_deref(),Some("outcomeUnknown"));
    assert!(unknown.actions.contains(&il::Action::Dismiss));
}
