//! C3 unit tests: every §2.9 bus row (state sequence + surface), RC7 system
//! senders, RC8 native statuses, §2.11 Claude stages, decision 2 refusal,
//! decision 9 dismiss + backfill, and the switch-off path.

use super::*;
use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_bus::model::{AgentBusEnvelopeKind, AgentMessageKind};
use crate::management::v2::agent_bus_state::{AgentBusMessageRepository, AgentBusQueuedMessage};
use crate::management::v2::input_ledger::{InputLedger, ListQuery, DATABASE_FILE};

const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
const FROM: &str = "cutex.22222222-2222-4222-8222-222222222222";

struct Fixture {
    root: std::path::PathBuf,
    ledger: &'static InputLedger,
    repository: AgentBusMessageRepository,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-il-bus-{}", uuid::Uuid::new_v4().simple()));
        let ledger: &'static InputLedger = Box::leak(Box::new(InputLedger::open(root.join(DATABASE_FILE))));
        let repository = AgentBusMessageRepository::open(root.join("bus")).unwrap();
        Self { root, ledger, repository }
    }

    /// Run with this fixture's ledger as the thread's ledger (switch on);
    /// bus and ledger events are captured, never appended.
    fn on<T>(&self, body: impl FnOnce() -> T) -> (T, Vec<(String, String, Value)>) {
        let ((result, _bus), ledger) = input_ledger::capture_events(|| {
            with_test_ledger(self.ledger, || crate::management::v2::agent_bus_state::capture_events(body))
        });
        (result, ledger)
    }

    fn record(&self, id: &str) -> Option<InputRecord> {
        self.ledger.get(TO, &[id.to_string()]).unwrap().records.pop()
    }

    fn all(&self) -> Vec<InputRecord> {
        self.ledger.list(TO, ListQuery { since_revision: Some(0), include_settled_secs: 0 }).unwrap().records
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn label(record: &InputRecord) -> String {
    match (&record.reason, record.stage, record.evidence) {
        (Some(reason), _, _) => format!("{}{{{reason}}}", record.state),
        (None, Some(stage), _) => format!("{}[{stage}]", record.state),
        (None, None, Some(evidence)) => format!("{}({evidence})", record.state),
        _ => record.state.to_string(),
    }
}

/// The state path and surfaces of `id` from the captured stateChanged events.
fn path(events: &[(String, String, Value)], id: &str) -> (Vec<String>, Vec<String>) {
    let mut states = Vec::new();
    let mut surfaces: Vec<String> = Vec::new();
    for (_, method, params) in events {
        if method != input_ledger::STATE_CHANGED_EVENT || params["record"]["inputId"] != id {
            continue;
        }
        let record: InputRecord = serde_json::from_value(params["record"].clone()).unwrap();
        let state = label(&record);
        if states.last() != Some(&state) {
            states.push(state);
        }
        let surface = record.surface.to_string();
        if surfaces.last() != Some(&surface) {
            surfaces.push(surface);
        }
    }
    (states, surfaces)
}

fn envelope(id: &str, from: &str, kind: AgentMessageKind, mode: AgentDeliveryMode, from_session: Option<&str>) -> AgentBusMessage {
    AgentBusMessage {
        id: id.to_string(),
        kind: AgentBusEnvelopeKind::Message,
        from: from.to_string(),
        to: "rt-to".to_string(),
        from_cutex_session_id: from_session.map(str::to_string),
        to_cutex_session_id: Some(TO.to_string()),
        content: format!("content of {id}"),
        trigger_turn: mode.trigger_turn(),
        delivery_mode: mode,
        created_at_epoch_secs: 1,
        sender_kind: kind,
        display_source: None,
        submit_mode: None,
        control_type: None,
        control_payload: None,
        external_action_id: None,
        external_message_id: None,
    }
}

fn queued(envelope: AgentBusMessage) -> AgentBusQueuedMessage {
    let content = envelope.content.clone();
    AgentBusQueuedMessage {
        owner_cutex_session_id: TO.to_string(),
        message_id: envelope.id.clone(),
        from_cutex_session_id: envelope.from_cutex_session_id.clone(),
        to_cutex_session_id: TO.to_string(),
        from_runtime_agent_id: envelope.from_cutex_session_id.as_ref().map(|_| "rt-from".to_string()),
        to_runtime_agent_id: Some("rt-to".to_string()),
        delivery_mode: envelope.delivery_mode.event_label().to_string(),
        content: content.clone(),
        queued_at: Utc::now(),
        canonical_envelope: envelope,
        semantic_sha256: crate::role_revision::sha256_bytes(content.as_bytes()).as_str().to_string(),
    }
}

fn wake(id: &str) -> AgentBusQueuedMessage {
    queued(envelope(id, "sender-agent", AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, Some(FROM)))
}

fn note(id: &str) -> AgentBusQueuedMessage {
    queued(envelope(id, "sender-agent", AgentMessageKind::Agent, AgentDeliveryMode::Passive, Some(FROM)))
}

fn alarm(id: &str) -> AgentBusQueuedMessage {
    queued(envelope(id, crate::agent_bus::alarms::ALARM_SYSTEM_SENDER, AgentMessageKind::AlarmSystem, AgentDeliveryMode::AfterTurn, None))
}

fn inbox_receipt(id: &str) -> Value {
    json!({"messageId": id, "threadId": "thread", "durable": true, "receiptScope": "local-inbox"})
}

// ---------------------------------------------------------------------------
// Pure mapping

#[test]
fn sender_kinds_and_required_timing_by_sender() {
    let agent = envelope("m", "a", AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, Some(FROM));
    let input = new_input(Some(&agent), "m", Some(FROM), "after_turn", "hi", Some("Ada"));
    assert_eq!((input.kind, input.sender.kind, input.intent.timing), (InputKind::AgentMessage, SenderKind::Agent, Timing::AfterTurn));
    assert_eq!(input.sender.cutex_session_id.as_deref(), Some(FROM));
    assert_eq!(input.sender.name, "Ada");

    let alarm = envelope("alarm-1", crate::agent_bus::alarms::ALARM_SYSTEM_SENDER, AgentMessageKind::AlarmSystem, AgentDeliveryMode::AfterTurn, None);
    let input = new_input(Some(&alarm), "alarm-1", None, "after_turn", "x", None);
    assert_eq!((input.kind, input.sender.kind, input.intent.timing), (InputKind::Alarm, SenderKind::Alarm, Timing::AfterTurn));

    let mut approval = alarm.clone();
    approval.external_message_id = Some(format!("{}x", crate::agent_bus::alarms::APPROVAL_NOTICE_MESSAGE_PREFIX));
    let input = new_input(Some(&approval), "alarm-approval-x", None, "after_turn", "x", None);
    assert_eq!((input.kind, input.sender.kind, input.intent.timing), (InputKind::SystemNotice, SenderKind::Approval, Timing::NextStep));

    let job = envelope("j", "cutex-job-service", AgentMessageKind::JobServiceSystem, AgentDeliveryMode::AfterTurn, None);
    let input = new_input(Some(&job), "j", None, "after_turn", "x", None);
    assert_eq!((input.kind, input.sender.kind), (InputKind::SystemNotice, SenderKind::JobService));

    let management = envelope("s", crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER, AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, None);
    let input = new_input(Some(&management), "s", None, "after_turn", "x", None);
    assert_eq!((input.kind, input.sender.kind), (InputKind::SystemNotice, SenderKind::Management));

    let mut task = envelope("ctask-1", crate::agent_bus::claude_task::SOURCE_ID, AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, None);
    task.control_type = Some(crate::agent_bus::claude_task::CONTROL_TYPE.into());
    let input = new_input(Some(&task), "ctask-1", None, "after_turn", "x", None);
    assert_eq!((input.kind, input.sender.kind), (InputKind::SystemNotice, SenderKind::ClaudeTask));
    // A legacy row without its canonical envelope still has a sender.
    assert_eq!(new_input(None, "ctask-2", None, "after_turn", "x", None).sender.kind, SenderKind::ClaudeTask);

    let mut peer = envelope("ptell-1", crate::agent_bus::peer_tell::SOURCE_ID, AgentMessageKind::Agent, AgentDeliveryMode::Passive, None);
    peer.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE.into());
    peer.control_payload = Some(json!({"peerId": "ci-1", "host": "host-b", "senderId": "cutex.x", "senderName": "Bo", "tellId": "t"}));
    let input = new_input(Some(&peer), "ptell-1", None, "passive", "x", None);
    assert_eq!((input.kind, input.sender.kind, input.intent.timing), (InputKind::PeerMessage, SenderKind::Peer, Timing::Passive));
    assert_eq!(input.remote.unwrap().host, "host-b");
    assert_eq!(input_ledger::creation_surface(input.kind, &input.intent), Surface::PendingNotes);
}

fn facts(state: &str) -> BusFacts {
    BusFacts {
        state: state.into(), timing: Timing::AfterTurn, frozen: false, a2: false, inbox: false, a4: false,
        external_receipt: false, observed: None, turn: None, settlement_closed: false, error_code: None,
        item: None,
    }
}

#[test]
fn every_bus_state_has_its_ledger_target() {
    assert_eq!(target_of(&facts("pending")), Target::Waiting { reason: "afterTurn", details: None });
    assert_eq!(target_of(&BusFacts { timing: Timing::Passive, ..facts("pending") }), Target::Waiting { reason: "passive", details: None });
    assert_eq!(target_of(&BusFacts { timing: Timing::NextStep, ..facts("pending") }), Target::Waiting { reason: "nextStep", details: None });
    assert_eq!(target_of(&facts("stale_held")), Target::Waiting { reason: "staleHeld", details: None });
    assert_eq!(target_of(&facts("inbox_accepted")), Target::Accepted { turn: None });
    assert_eq!(target_of(&BusFacts { a2: true, ..facts("pending") }), Target::Accepted { turn: None });
    // RC8: native statuses are no longer folded into accepted.
    let observed = |observed| target_of(&BusFacts { frozen: true, observed: Some(observed), ..facts("pending") });
    assert_eq!(observed(NativeObserved::Accepted), Target::Accepted { turn: None });
    assert_eq!(observed(NativeObserved::Held("plan_mode".into())),
        Target::Waiting { reason: "nativeHeld", details: Some(json!({"nativeReason": "plan_mode"})) });
    assert_eq!(observed(NativeObserved::Retry), Target::Waiting { reason: "nativeRetry", details: None });
    assert_eq!(observed(NativeObserved::Conflict), Target::Uncertain("nativeConflict"));
    let delivered = |a4, external| target_of(&BusFacts { a4, external_receipt: external, turn: Some("t1".into()), ..facts("delivered") });
    assert_eq!(delivered(true, false), Target::InContext { evidence: Evidence::A4Receipt, turn: Some("t1".into()), item: None });
    assert_eq!(delivered(false, true), Target::InContext { evidence: Evidence::ExternalInputPersisted, turn: Some("t1".into()), item: None });
    assert_eq!(delivered(false, false), Target::InContext { evidence: Evidence::ContextPersisted, turn: Some("t1".into()), item: None });
    assert_eq!(target_of(&facts("quarantined")), Target::Rejected("quarantined"));
    assert_eq!(target_of(&BusFacts { error_code: Some("native_semantic_conflict".into()), ..facts("quarantined") }), Target::Uncertain("nativeConflict"));
    let owner = Some(crate::management::v2::agent_bus_state::OWNER_DISCARD_CODE.to_string());
    assert_eq!(target_of(&BusFacts { error_code: owner.clone(), ..facts("quarantined") }), Target::Discarded("ownerDiscarded"));
    assert_eq!(target_of(&BusFacts { error_code: owner, ..facts("discarded") }), Target::Discarded("ownerDiscarded"));
    assert_eq!(target_of(&BusFacts { settlement_closed: true, inbox: true, ..facts("inbox_accepted") }), Target::Discarded("settlementClosed"));
    assert_eq!(target_of(&facts("failed")), Target::Rejected("invalid"));
}

#[test]
fn plan_only_uses_allowed_edges_and_never_moves_back() {
    use InputState::*;
    let names = |plan: Vec<Next>| plan.into_iter().map(|next| next.state).collect::<Vec<_>>();
    let accepted = Target::Accepted { turn: None };
    assert_eq!(names(plan(None, &accepted, Timing::AfterTurn)), vec![Sending, Accepted]);
    assert_eq!(names(plan(None, &Target::Discarded("ownerDiscarded"), Timing::Passive)), vec![Waiting, Discarded]);
    assert_eq!(names(plan(Some(Waiting), &Target::InContext { evidence: Evidence::A4Receipt, turn: None, item: None }, Timing::AfterTurn)), vec![Sending, InContext]);
    // No accepted → waiting (invariant 8), no plain waiting after sending.
    assert!(plan(Some(Accepted), &Target::Waiting { reason: "nativeHeld", details: None }, Timing::AfterTurn).is_empty());
    assert!(plan(Some(Sending), &Target::Waiting { reason: "afterTurn", details: None }, Timing::AfterTurn).is_empty());
    assert_eq!(names(plan(Some(Sending), &Target::Waiting { reason: "nativeRetry", details: None }, Timing::AfterTurn)), vec![Waiting]);
    assert_eq!(names(plan(Some(Accepted), &Target::Discarded("ownerDiscarded"), Timing::AfterTurn)), vec![Uncertain, Discarded]);
    assert!(plan(Some(InContext), &Target::Discarded("ownerDiscarded"), Timing::AfterTurn).is_empty());
    assert_eq!(names(plan(Some(Uncertain), &Target::InContext { evidence: Evidence::ContextPersisted, turn: None, item: None }, Timing::AfterTurn)), vec![InContext]);
    // Every planned edge is a §2.3 edge.
    for from in [None, Some(Waiting), Some(Sending), Some(Accepted), Some(Uncertain)] {
        for target in [accepted.clone(), Target::Sending, Target::Uncertain("receiptTimeout"), Target::Rejected("quarantined"),
            Target::Discarded("settlementClosed"), Target::Waiting { reason: "nativeHeld", details: None },
            Target::InContext { evidence: Evidence::A4Receipt, turn: Some("t".into()), item: None }] {
            let mut state = from;
            for next in plan(from, &target, Timing::AfterTurn) {
                assert!(input_ledger::edge_allowed(InputKind::AgentMessage, state, &next), "{from:?} -> {target:?}: {state:?} -> {}", next.state);
                state = Some(next.state);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Repository writes → ledger (§2.9 rows)

#[test]
fn unbound_acceptance_deadline_hands_off_to_its_carrying_turn_once() {
    let f = Fixture::new();
    let input = new_input(None, "unbound", Some(FROM), "after_turn", "notice", None);
    f.on(|| drive(TO, "unbound", &input, &Target::Accepted { turn:None }));
    assert!(f.record("unbound").unwrap().deadline.is_some());
    let bound = Target::Accepted { turn:Some("carrying-turn".into()) };
    f.on(|| drive(TO, "unbound", &input, &bound));
    assert!(f.record("unbound").unwrap().deadline.is_none());
    let grace = Utc::now() + chrono::Duration::seconds(60);
    f.on(|| f.ledger.transition(TO, "unbound", input_ledger::Expect::ANY,
        Next::accepted().deadline(Some(grace))).unwrap());
    f.on(|| drive(TO, "unbound", &input, &bound));
    assert_eq!(f.record("unbound").unwrap().deadline.as_deref(), Some(grace.to_rfc3339_opts(chrono::SecondsFormat::Millis, true).as_str()));
}

#[test]
fn wake_queued_sent_accepted_read_in_moves_next_up_to_timeline() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(wake("w1")).unwrap();
        f.repository.input_ledger_sending(TO, "w1");
        f.repository.record_local_inbox(TO, "w1", &inbox_receipt("w1")).unwrap();
        assert!(f.repository.record_context_injected(TO, "w1", "turn-7").unwrap());
    });
    let (states, surfaces) = path(&events, "w1");
    assert_eq!(states, ["waiting{afterTurn}", "sending", "accepted", "inContext(contextPersisted)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    let record = f.record("w1").unwrap();
    assert_eq!(record.turn_id.as_deref(), Some("turn-7"));
    assert_eq!(record.sender.cutex_session_id.as_deref(), Some(FROM));
    assert_eq!(record.intent.timing, Timing::AfterTurn);
}

#[test]
fn passive_note_waits_in_pending_notes_and_moves_once_when_read_in() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(note("n1")).unwrap();
        f.repository.input_ledger_sending(TO, "n1");
        f.repository.record_local_inbox(TO, "n1", &inbox_receipt("n1")).unwrap();
        // Repeats change nothing (no duplicate record, no extra event).
        f.repository.record_local_inbox(TO, "n1", &inbox_receipt("n1")).unwrap();
        f.repository.record_context_injected(TO, "n1", "turn-2").unwrap();
        f.repository.record_context_injected(TO, "n1", "turn-2").unwrap();
    });
    let (states, surfaces) = path(&events, "n1");
    assert_eq!(states, ["waiting{passive}", "sending", "accepted", "inContext(contextPersisted)"]);
    assert_eq!(surfaces, ["pendingNotes", "timeline"]);
    assert_eq!(f.all().len(), 1);
}

#[test]
fn alarms_and_system_notices_get_records_but_no_old_bus_events() {
    let f = Fixture::new();
    let mut job = queued(envelope("j1", "cutex-job-service", AgentMessageKind::JobServiceSystem, AgentDeliveryMode::AfterTurn, None));
    job.canonical_envelope.content = "job".into();
    let ((), ledger_events) = {
        let ((result, bus_events), ledger_events) = input_ledger::capture_events(|| with_test_ledger(f.ledger, || {
            crate::management::v2::agent_bus_state::capture_events(|| {
                f.repository.record_queued(alarm("alarm-1")).unwrap();
                f.repository.record_queued(job).unwrap();
            })
        }));
        // §4.3: system senders still emit no messageQueued (old desktops
        // would show them as notes).
        assert!(bus_events.iter().all(|(_, _, params)| params.get("messageId").is_none()), "{bus_events:?}");
        (result, ledger_events)
    };
    let alarm = f.record("alarm-1").unwrap();
    assert_eq!((alarm.kind, alarm.sender.kind, alarm.surface), (InputKind::Alarm, SenderKind::Alarm, Surface::NextUp));
    assert_eq!(label(&alarm), "waiting{afterTurn}");
    let job = f.record("j1").unwrap();
    assert_eq!((job.kind, job.sender.kind), (InputKind::SystemNotice, SenderKind::JobService));
    assert_eq!(ledger_events.iter().filter(|(_, method, _)| method == input_ledger::STATE_CHANGED_EVENT).count(), 2);
}

#[test]
fn a2_submission_and_a4_receipt_map_to_accepted_and_in_context() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(wake("a1")).unwrap();
        f.repository.input_ledger_sending(TO, "a1");
        f.repository.record_a2_submission(TO, "a1", "sub-1", Utc::now()).unwrap();
        let receipt: crate::app_server::commands::InterAgentContextPersistedReceipt = serde_json::from_value(json!({
            "receiptId": "r1", "threadId": "thread", "turnId": "turn-1", "responseItemId": "item",
            "messageId": "a1", "rolloutOrdinal": 1
        })).unwrap();
        f.repository.record_delivered(TO, "a1", &receipt, Utc::now()).unwrap();
    });
    let (states, surfaces) = path(&events, "a1");
    assert_eq!(states, ["waiting{afterTurn}", "sending", "accepted", "inContext(a4Receipt)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
}

#[test]
fn stale_hold_quarantine_and_owner_discard() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        let message = wake("s1");
        let digest = message.semantic_sha256.clone();
        f.repository.record_queued_isolated(message).unwrap();
        assert!(f.repository.hold_stale_pending(TO, "s1", &digest, 3600, Utc::now()).unwrap());
        f.repository.release_stale_held(TO, "s1", &digest, Utc::now()).unwrap();
        f.repository.record_quarantined(TO, "s1", json!({"code": "canonical_envelope_invalid"}), Utc::now()).unwrap();

        f.repository.record_queued_isolated(wake("s2")).unwrap();
        f.repository.owner_discard("s2", None, "no longer needed", false, Utc::now()).unwrap().unwrap();
    });
    let (states, surfaces) = path(&events, "s1");
    assert_eq!(states, ["waiting{afterTurn}", "waiting{staleHeld}", "waiting{afterTurn}", "rejected{quarantined}"]);
    assert_eq!(surfaces, ["nextUp", "none"]);
    let (states, surfaces) = path(&events, "s2");
    assert_eq!(states, ["waiting{afterTurn}", "discarded{ownerDiscarded}"]);
    assert_eq!(surfaces, ["nextUp", "none"]);
}

#[test]
fn native_statuses_after_submit_are_not_awaiting_receipt() {
    let f = Fixture::new();
    let input = new_input(None, "x", Some(FROM), "after_turn", "x", None);
    let (_, events) = f.on(|| {
        for (id, observed) in [("h", NativeObserved::Held("plan_mode".into())), ("r", NativeObserved::Retry), ("c", NativeObserved::Conflict)] {
            let before = BusFacts { frozen: true, ..facts("pending") };
            drive(TO, id, &input, &target_of(&before));
            sending(TO, id, &input);
            apply_changes(vec![BusChange {
                session: TO.into(), message_id: id.into(), before: Some(before.clone()),
                after: BusFacts { observed: Some(observed), ..before }, input: input.clone(),
            }]);
        }
    });
    assert_eq!(path(&events, "h").0, ["waiting{afterTurn}", "sending", "waiting{nativeHeld}"]);
    assert_eq!(f.record("h").unwrap().details, json!({"nativeReason": "plan_mode"}));
    assert_eq!(path(&events, "r").0, ["waiting{afterTurn}", "sending", "waiting{nativeRetry}"]);
    assert_eq!(path(&events, "c").0, ["waiting{afterTurn}", "sending", "uncertain{nativeConflict}"]);
}

#[test]
fn native_status_of_maps_every_delivery_state() {
    let status = |delivery: &str, processing: &str, reason: Value| -> Status {
        serde_json::from_value(json!({"messageId": "m", "semanticSha256": "0".repeat(64), "deliveryState": delivery,
            "receipt": null, "processing": {"state": processing, "attemptId": null, "reason": reason}})).unwrap()
    };
    assert_eq!(NativeObserved::of(&status("pending", "held", json!("plan_mode"))), Some(NativeObserved::Held("plan_mode".into())));
    assert_eq!(NativeObserved::of(&status("pending", "claimed", Value::Null)), Some(NativeObserved::Accepted));
    assert_eq!(NativeObserved::of(&status("retryable_error", "none", Value::Null)), Some(NativeObserved::Retry));
    assert_eq!(NativeObserved::of(&status("conflict", "none", Value::Null)), Some(NativeObserved::Conflict));
    assert_eq!(NativeObserved::of(&status("unknown", "none", Value::Null)), None);
}

// ---------------------------------------------------------------------------
// Claude stages (§2.11), decision 2, decision 9

#[test]
fn claude_stages_advance_an_owner_insert_monotonically() {
    let f = Fixture::new();
    let owner = NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(input_ledger::Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "steer".into(), ask_id: None, remote: None,
    };
    f.ledger.transition(TO, "c-1", Expect::NEW, Next::received().create(owner)).unwrap();
    f.ledger.transition(TO, "c-1", Expect::ANY, Next::sending()).unwrap();
    let stage = |stage: &str| json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": "c-1", "stage": stage});
    let (_, events) = f.on(|| {
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("registered")));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("offered")));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("registered")));
        claude_input_stage(TO, "claude/input/contextPersisted", Some(&json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": "c-1"})));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("contextPersisted")));
        // No clientId (a bus input) or no record: nothing.
        claude_input_stage(TO, "claude/input/contextPersisted", Some(&json!({"turnId": "turn-1", "messageId": "native-2", "clientId": null})));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&json!({"turnId": "turn-1", "clientId": "missing", "stage": "offered"})));
    });
    assert_eq!(path(&events, "c-1").0, ["accepted[registered]", "accepted[offered]", "inContext(contextPersisted)"]);
    assert_eq!(f.record("c-1").unwrap().turn_id.as_deref(), Some("turn-1"));
    assert!(f.record("missing").is_none());
    assert_eq!(events.len(), 3, "{events:?}");
}

/// r274.2: an unread owner steer is on nextUp; `registered` keeps it there,
/// `offered` moves it once, `contextPersisted` keeps the timeline. A steer
/// that slips to the next turn moves at `contextPersisted` with that turn.
#[test]
fn claude_offered_moves_an_owner_insert_from_next_up_once() {
    let f = Fixture::new();
    let owner = || NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(input_ledger::Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "steer".into(), ask_id: None, remote: None,
    };
    for id in ["c-1", "c-2"] {
        f.ledger.transition(TO, id, Expect::NEW, Next::received().create(owner()).surface(Surface::NextUp)).unwrap();
        f.ledger.transition(TO, id, Expect::ANY, Next::sending()).unwrap();
    }
    let stage = |id: &str, stage: &str| json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": id, "stage": stage});
    let (_, events) = f.on(|| {
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("c-1", "registered")));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("c-1", "offered")));
        claude_input_stage(TO, "claude/input/contextPersisted", Some(&json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": "c-1"})));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("c-2", "registered")));
        claude_input_stage(TO, "claude/input/contextPersisted", Some(&json!({"threadId": "t", "turnId": "turn-2", "messageId": "native-2", "clientId": "c-2"})));
    });
    let (states, surfaces) = path(&events, "c-1");
    assert_eq!(states, ["accepted[registered]", "accepted[offered]", "inContext(contextPersisted)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    let moved: Vec<&Value> = events.iter()
        .filter(|(_, method, params)| method == input_ledger::STATE_CHANGED_EVENT && params["record"]["inputId"] == "c-1")
        .map(|(_, _, params)| &params["record"]["surface"])
        .collect();
    assert_eq!(moved, [&json!("nextUp"), &json!("timeline"), &json!("timeline")], "moves exactly once");
    let (states, surfaces) = path(&events, "c-2");
    assert_eq!(states, ["accepted[registered]", "inContext(contextPersisted)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    assert_eq!(f.record("c-2").unwrap().turn_id.as_deref(), Some("turn-2"));
}

/// interruptSend took the steer out of the turn being interrupted: a late
/// stage of that steer leaves `sending{interruptSend}` alone; the proof that
/// the runtime read it (`contextPersisted`) still counts.
#[test]
fn a_late_claude_stage_does_not_undo_interrupt_send() {
    let f = Fixture::new();
    let owner = NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(input_ledger::Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "steer".into(), ask_id: None, remote: None,
    };
    f.ledger.transition(TO, "c-1", Expect::NEW, Next::received().create(owner)).unwrap();
    f.ledger.transition(TO, "c-1", Expect::ANY, Next::sending()).unwrap();
    f.ledger.transition(TO, "c-1", Expect::ANY, Next::accepted().turn("turn-1")).unwrap();
    f.ledger.transition(TO, "c-1", Expect::ANY, Next::sending().reason("interruptSend")
        .details(json!({"interruptedTurnId": "turn-1"}))).unwrap();
    let stage = |stage: &str| json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": "c-1", "stage": stage});
    let (_, events) = f.on(|| {
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("registered")));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&stage("offered")));
    });
    assert!(events.is_empty(), "{events:?}");
    let record = f.record("c-1").unwrap();
    assert_eq!((record.state, record.reason.as_deref()), (InputState::Sending, Some("interruptSend")));
    let (_, events) = f.on(|| {
        claude_input_stage(TO, "claude/input/contextPersisted", Some(&json!({"threadId": "t", "turnId": "turn-1", "messageId": "native-1", "clientId": "c-1"})));
    });
    assert_eq!(path(&events, "c-1").0, ["inContext(contextPersisted)"]);
}

#[test]
fn refused_tell_is_recorded_for_the_target_and_the_sender() {
    let f = Fixture::new();
    let (id, events) = f.on(|| tell_refused(TO, "tell-1", FROM, Some("Ada"), "hello", false));
    assert_eq!(id.as_deref(), Some("tell-1"));
    let record = f.record("tell-1").unwrap();
    assert_eq!(label(&record), "rejected{targetUnavailable}");
    assert_eq!((record.surface, record.kind), (Surface::None, InputKind::AgentMessage));
    assert_eq!(record.sender.cutex_session_id.as_deref(), Some(FROM));
    // Owner decision 2: the sender side keeps an outgoing notice too.
    let notice = f.ledger.get(FROM, &["tell-1".to_string()]).unwrap().records.pop().expect("sender-side record");
    assert_eq!(label(&notice), "rejected{targetUnavailable}");
    assert_eq!((notice.surface, notice.kind, notice.sender.kind), (Surface::None, InputKind::SystemNotice, SenderKind::Cutex));
    assert_eq!((notice.details["direction"].as_str(), notice.details["targetCutexSessionId"].as_str()), (Some("outgoing"), Some(TO)));
    assert_eq!(events.iter().map(|(session, _, _)| session.as_str()).collect::<Vec<_>>(), vec![TO, FROM]);
    // A retry of the same tell reuses the record.
    let (again, events) = f.on(|| tell_refused(TO, "tell-1", FROM, Some("Ada"), "hello", false));
    assert_eq!(again.as_deref(), Some("tell-1"));
    assert!(events.is_empty());
}

#[test]
fn owner_dismiss_settles_protected_and_closed_rows_without_refusing() {
    let f = Fixture::new();
    let mut task = envelope("ctask-1", crate::agent_bus::claude_task::SOURCE_ID, AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, None);
    task.control_type = Some(crate::agent_bus::claude_task::CONTROL_TYPE.into());
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(queued(task)).unwrap();
        f.repository.input_ledger_sending(TO, "ctask-1");
        f.repository.record_local_inbox(TO, "ctask-1", &inbox_receipt("ctask-1")).unwrap();
    });
    assert_eq!(path(&events, "ctask-1").0, ["waiting{afterTurn}", "sending", "accepted"]);
    // Settlement close would refuse this one (message_protected, no evidence).
    let record = f.ledger.transition(TO, "ctask-1", Expect::ANY, Next::uncertain("receiptTimeout")).unwrap().record;
    let (result, events) = f.on(|| dismiss_with(&f.repository, &record, Utc::now()));
    assert!(result.unwrap());
    // The dismiss itself writes the ledger (its own CAS): nothing here.
    assert!(events.is_empty(), "{events:?}");
    let snapshot = f.repository.snapshot_by_message_id("ctask-1").unwrap().unwrap();
    assert_eq!(snapshot.state, "discarded", "a local-inbox acceptance is not native submission evidence");
    assert_eq!(snapshot.error.unwrap()["code"], crate::management::v2::agent_bus_state::OWNER_DISCARD_CODE);
    // Already settled, or gone: ledger only, never an error.
    assert!(!dismiss_with(&f.repository, &record, Utc::now()).unwrap());
    let gone = InputRecord { input_id: "gone".into(), ..record };
    assert!(!dismiss_with(&f.repository, &gone, Utc::now()).unwrap());
}

#[test]
fn backfill_creates_dismissible_records_for_open_messages_once() {
    let f = Fixture::new();
    // Written while the switch was off: no records.
    f.repository.record_queued_isolated(wake("b-pending")).unwrap();
    f.repository.record_queued_isolated(note("b-inbox")).unwrap();
    f.repository.record_local_inbox(TO, "b-inbox", &inbox_receipt("b-inbox")).unwrap();
    let held = wake("b-held");
    let digest = held.semantic_sha256.clone();
    f.repository.record_queued_isolated(held).unwrap();
    f.repository.hold_stale_pending(TO, "b-held", &digest, 3600, Utc::now()).unwrap();
    f.repository.record_queued_isolated(wake("b-done")).unwrap();
    f.repository.record_context_injected(TO, "b-done", "turn").unwrap_or_default();
    assert!(f.all().is_empty());

    let later = Utc::now() + chrono::Duration::hours(2);
    let ((items, cursor), _) = f.on(|| f.repository.input_ledger_backfill_items(None, 2).unwrap());
    assert_eq!(items.len(), 2);
    let ((rest, done), _) = f.on(|| f.repository.input_ledger_backfill_items(cursor.as_deref(), 2).unwrap());
    assert!(done.is_none());
    let (created, _) = f.on(|| backfill_batch(&items, later) + backfill_batch(&rest, later));
    assert_eq!(created, 3);
    assert_eq!(label(&f.record("b-pending").unwrap()), "waiting{afterTurn}");
    assert_eq!(label(&f.record("b-held").unwrap()), "waiting{staleHeld}");
    let inbox = f.record("b-inbox").unwrap();
    assert_eq!(label(&inbox), "uncertain{receiptTimeout}");
    assert!(f.record("b-done").is_none());
    for id in ["b-pending", "b-held", "b-inbox"] {
        assert!(input_ledger::dismissible(&f.record(id).unwrap()), "{id}");
    }
    // Idempotent.
    let (again, _) = f.on(|| backfill_batch(&items, later) + backfill_batch(&rest, later));
    assert_eq!(again, 0);
}

#[test]
fn switch_off_writes_nothing() {
    let f = Fixture::new();
    assert!(!active(), "the ledger switch is off by default");
    let ((), _) = crate::management::v2::agent_bus_state::capture_events(|| {
        f.repository.record_queued_isolated(wake("off")).unwrap();
        f.repository.input_ledger_sending(TO, "off");
        f.repository.record_local_inbox(TO, "off", &inbox_receipt("off")).unwrap();
        claude_input_stage(TO, "claude/input/stageChanged", Some(&json!({"clientId": "off", "stage": "registered"})));
        assert!(tell_refused(TO, "tell-off", FROM, None, "x", false).is_none());
    });
    assert!(f.all().is_empty());
}

#[test]
fn recipient_without_endpoint_waits_agent_offline_until_sent() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(wake("o1")).unwrap();
        f.repository.input_ledger_agent_offline("o1");
        f.repository.input_ledger_sending(TO, "o1");
        // Never back to waiting once sent.
        f.repository.input_ledger_agent_offline("o1");
    });
    assert_eq!(path(&events, "o1").0, ["waiting{afterTurn}", "waiting{agentOffline}", "sending"]);
    assert_eq!(path(&events, "o1").1, ["nextUp"]);
}

#[test]
fn tell_refusal_keeps_the_refusal_and_names_reason_and_record() {
    let f = Fixture::new();
    let refusal = anyhow::Error::from(crate::agent_bus::routing::AgentTargetResolutionError::unavailable(
        TO, format!("Durable cutex session `{TO}` has no current online endpoint")));
    // Switch off: the r273.1 error path is unchanged (no body, no record).
    assert!(crate::agent_bus::server::tell_target_unavailable(&refusal, FROM, "tell-x", "hi", true).is_none());
    let (body, events) = f.on(|| crate::agent_bus::server::tell_target_unavailable(&refusal, FROM, "tell-x", "hi", true));
    let body = body.unwrap();
    assert_eq!((body["ok"].clone(), body["status"].clone()), (json!(false), json!("no_write")));
    assert_eq!((body["code"].clone(), body["reasonCode"].clone(), body["messageId"].clone()),
        (json!("target_unavailable"), json!("targetUnavailable"), json!("tell-x")));
    assert_eq!(path(&events, "tell-x"), (vec!["rejected{targetUnavailable}".to_string()], vec!["none".to_string()]));
    let record = f.record("tell-x").unwrap();
    assert_eq!((record.sender.kind, record.sender.cutex_session_id.as_deref(), record.intent.timing),
        (SenderKind::Agent, Some(FROM), Timing::AfterTurn));
    // Other resolution errors are not recorded.
    let other = anyhow::Error::from(crate::agent_bus::routing::AgentTargetResolutionError::ambiguous("two"));
    assert!(f.on(|| crate::agent_bus::server::tell_target_unavailable(&other, FROM, "tell-y", "hi", true)).0.is_none());
}

#[test]
fn writes_under_the_authority_fence_are_deferred_until_applied() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        let ((), changes) = deferred(|| {
            f.repository.record_queued_isolated(wake("d1")).unwrap();
        });
        // Nothing reached the ledger while the fence scope was open.
        assert!(f.record("d1").is_none());
        assert_eq!(changes.len(), 1);
        apply_changes(changes);
        assert_eq!(label(&f.record("d1").unwrap()), "waiting{afterTurn}");
    });
    assert_eq!(path(&events, "d1").0, ["waiting{afterTurn}"]);
}

#[test]
fn owner_retry_redelivers_a_wake_under_the_same_id() {
    let f = Fixture::new();
    let (_, events) = f.on(|| {
        f.repository.record_queued_isolated(wake("r1")).unwrap();
        f.repository.input_ledger_sending(TO, "r1");
        f.repository.record_local_inbox(TO, "r1", &inbox_receipt("r1")).unwrap();
        // The carrying turn was interrupted: settled uncertain (C5).
        let record = f.record("r1").unwrap();
        let uncertain = f.ledger.transition(TO, "r1", input_ledger::Expect::rev(record.rev), Next::uncertain("turnInterrupted")).unwrap();
        assert_eq!(f.repository.input_ledger_redeliverable(TO, "r1").unwrap(), Ok(()));
        assert_eq!(f.repository.input_ledger_redeliverable(FROM, "r1").unwrap().unwrap_err().0, "bus_message_not_found");
        // cutex/input/retry: the ledger first, then the store, without an echo.
        f.ledger.transition(TO, "r1", input_ledger::Expect::rev(uncertain.record.rev), Next::waiting("ownerRetry")).unwrap();
        let (reset, echo) = deferred(|| f.repository.input_ledger_redeliver(TO, "r1", Utc::now()));
        assert!(reset.unwrap());
        assert!(!echo.is_empty(), "the reset is a store change the hook would have mapped");
        let (state, receipt) = f.repository.message_state("r1").unwrap().unwrap();
        assert_eq!((state.as_str(), receipt), ("pending", false), "receipts of the first attempt are gone");
        // The bridge delivers it again: same id, next attempt.
        f.repository.input_ledger_sending(TO, "r1");
        f.repository.record_local_inbox(TO, "r1", &inbox_receipt("r1")).unwrap();
        f.repository.record_context_injected(TO, "r1", "turn-3").unwrap();
        // Delivered: nothing to resend any more.
        assert_eq!(f.repository.input_ledger_redeliverable(TO, "r1").unwrap().unwrap_err().0, "bus_message_not_redeliverable");
        assert!(!f.repository.input_ledger_redeliver(TO, "r1", Utc::now()).unwrap());
    });
    let (states, _) = path(&events, "r1");
    assert_eq!(states, ["waiting{afterTurn}", "sending", "accepted", "sending", "accepted", "inContext(contextPersisted)"],
        "the direct ledger writes are not captured here; the bridge's are");
    let record = f.record("r1").unwrap();
    assert_eq!((record.state, record.attempt, record.turn_id.as_deref()), (InputState::InContext, 2, Some("turn-3")));
    let view = f.repository.input_ledger_view("r1").unwrap().unwrap();
    assert_eq!((view.0.as_str(), view.1.state.as_str()), (TO, "delivered"));
}

// ---------------------------------------------------------------------------
// r274.1: the native timeline item (054 §3.2 `timelineItemId`)

#[test]
fn delivered_target_names_the_receipt_item_or_the_message_item() {
    let delivered = BusFacts { turn: Some("t1".into()), item: Some("item-1".into()), ..facts("delivered") };
    assert_eq!(target_of(&delivered), Target::InContext { evidence: Evidence::ContextPersisted, turn: Some("t1".into()), item: Some("item-1".into()) });
    let plan = plan(Some(InputState::Accepted), &target_of(&delivered), Timing::AfterTurn);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].timeline_item_id.as_deref(), Some("item-1"));
}

/// Queue `message`, send it, take it into the local inbox and read it into
/// turn `turn` (the Claude adapter's context injection).
fn read_in(f: &Fixture, message: AgentBusQueuedMessage, turn: &str) -> String {
    let id = message.message_id.clone();
    f.repository.record_queued_isolated(message).unwrap();
    f.repository.input_ledger_sending(TO, &id);
    f.repository.record_local_inbox(TO, &id, &inbox_receipt(&id)).unwrap();
    // `false` for a system sender (no messageDelivered event), still read in.
    f.repository.record_context_injected(TO, &id, turn).unwrap();
    let state = f.repository.snapshot_by_message_id(&id).unwrap().map(|snapshot| snapshot.state);
    assert_eq!(state.as_deref(), Some("delivered"), "{id}");
    id
}

#[test]
fn agent_messages_alarms_job_notices_and_claude_tasks_bind_their_message_item() {
    let f = Fixture::new();
    let mut task = envelope("ctask-1", crate::agent_bus::claude_task::SOURCE_ID, AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, None);
    task.control_type = Some(crate::agent_bus::claude_task::CONTROL_TYPE.into());
    let job = queued(envelope("job-1", "cutex-job-service", AgentMessageKind::JobServiceSystem, AgentDeliveryMode::AfterTurn, None));
    let (ids, _) = f.on(|| {
        vec![
            read_in(&f, wake("w1"), "turn-1"),
            read_in(&f, note("n1"), "turn-1"),
            read_in(&f, alarm("alarm-1"), "turn-2"),
            read_in(&f, job, "turn-2"),
            read_in(&f, queued(task), "turn-3"),
        ]
    });
    for id in &ids {
        let record = f.record(id).unwrap();
        assert_eq!((record.state, record.surface), (InputState::InContext, Surface::Timeline), "{id}");
        // The adapter's `external_event` item carries the message id.
        assert_eq!(record.timeline_item_id.as_deref(), Some(id.as_str()), "{id}");
    }
    let kinds: Vec<InputKind> = ids.iter().map(|id| f.record(id).unwrap().kind).collect();
    assert_eq!(kinds, [InputKind::AgentMessage, InputKind::AgentMessage, InputKind::Alarm, InputKind::SystemNotice, InputKind::SystemNotice]);
    // The backfill reads the same items from the store.
    let items = f.repository.input_ledger_items(&[ids[0].clone(), "unknown".into()]).unwrap();
    assert_eq!(items, vec![(ids[0].clone(), TO.to_string(), Some(ids[0].clone()))]);
}

#[test]
fn an_a4_receipt_binds_its_response_item() {
    let f = Fixture::new();
    f.on(|| {
        f.repository.record_queued_isolated(wake("a1")).unwrap();
        f.repository.input_ledger_sending(TO, "a1");
        // Not delivered yet: no item.
        assert_eq!(f.repository.input_ledger_items(&["a1".into()]).unwrap()[0].2, None);
        let receipt: crate::app_server::commands::InterAgentContextPersistedReceipt = serde_json::from_value(json!({
            "receiptId": "r1", "threadId": "thread", "turnId": "turn-1", "responseItemId": "resp-item-1",
            "messageId": "a1", "rolloutOrdinal": 1
        })).unwrap();
        f.repository.record_delivered(TO, "a1", &receipt, Utc::now()).unwrap();
    });
    let record = f.record("a1").unwrap();
    assert_eq!(label(&record), "inContext(a4Receipt)");
    assert_eq!(record.timeline_item_id.as_deref(), Some("resp-item-1"));
}

// ---------------------------------------------------------------------------
// r274.2 §9.1: Claude records move at their native row, `inContext` at the receipt

fn changes(events: &[(String, String, Value)], id: &str) -> Vec<InputRecord> {
    events.iter()
        .filter(|(_, method, params)| method == input_ledger::STATE_CHANGED_EVENT && params["record"]["inputId"] == id)
        .map(|(_, _, params)| serde_json::from_value(params["record"].clone()).unwrap())
        .collect()
}

/// A frozen r8 note / wake: the native `external_event` item moves it once
/// to the timeline as `accepted` (turn, item = message id, readSignal);
/// item/started + item/completed are one move; the receipt is a state-only
/// `inContext` write.
#[test]
fn claude_native_item_moves_a_note_to_the_timeline_as_accepted() {
    let f = Fixture::new();
    let note = new_input(None, "n1", Some(FROM), "passive", "note", None);
    let wake = new_input(None, "w1", Some(FROM), "after_turn", "wake", None);
    let (_, events) = f.on(|| {
        sending(TO, "n1", &note);
        drive(TO, "n1", &note, &Target::Accepted { turn: None });
        native_item(TO, "n1", &note, "turn-1");
        native_item(TO, "n1", &note, "turn-1");
        // A later bus observation without a turn does not undo it.
        drive(TO, "n1", &note, &Target::Accepted { turn: None });
        drive(TO, "n1", &note, &Target::InContext { evidence: Evidence::ExternalInputPersisted,
            turn: Some("turn-1".into()), item: Some("n1".into()) });
        // Never seen before (the record is created on the way).
        native_item(TO, "w1", &wake, "turn-1");
    });
    let n1 = changes(&events, "n1");
    let labels: Vec<String> = n1.iter().map(label).collect();
    assert_eq!(labels, ["sending", "accepted", "accepted", "inContext(externalInputPersisted)"]);
    let surfaces: Vec<Surface> = n1.iter().map(|record| record.surface).collect();
    assert_eq!(surfaces, [Surface::PendingNotes, Surface::PendingNotes, Surface::Timeline, Surface::Timeline]);
    let moved = &n1[2];
    assert_eq!((moved.turn_id.as_deref(), moved.timeline_item_id.as_deref()), (Some("turn-1"), Some("n1")));
    assert_eq!(moved.details[READ_SIGNAL], "nativeItem");
    let (states, surfaces) = path(&events, "w1");
    assert_eq!((states, surfaces), (vec!["sending".to_string(), "accepted".into()], vec!["nextUp".to_string(), "timeline".into()]));
}

/// Nothing for a record the receipt or a check already decided; a note the
/// next turn reads again (released by a failed one) takes that turn.
#[test]
fn claude_native_item_leaves_decided_records_and_follows_a_rereading_turn() {
    let f = Fixture::new();
    let input = |id: &str| new_input(None, id, Some(FROM), "passive", "note", None);
    let (_, events) = f.on(|| {
        drive(TO, "done", &input("done"), &Target::InContext { evidence: Evidence::ExternalInputPersisted, turn: Some("t0".into()), item: None });
        drive(TO, "lost", &input("lost"), &Target::Uncertain("turnInterrupted"));
        native_item(TO, "done", &input("done"), "turn-1");
        native_item(TO, "lost", &input("lost"), "turn-1");
        native_item(TO, "again", &input("again"), "turn-1");
        f.ledger.transition(TO, "again", Expect::ANY, Next::accepted().deadline(Some(Utc::now()))).unwrap();
        native_item(TO, "again", &input("again"), "turn-2");
    });
    // `sending` then the decided state, nothing after.
    assert_eq!(changes(&events, "done").iter().map(label).collect::<Vec<_>>(), ["sending", "inContext(externalInputPersisted)"]);
    assert_eq!(changes(&events, "lost").iter().map(label).collect::<Vec<_>>(), ["sending", "uncertain{turnInterrupted}"]);
    let again = f.record("again").unwrap();
    assert_eq!((again.surface, again.turn_id.as_deref(), again.deadline.as_deref()), (Surface::Timeline, Some("turn-2"), None));
}

/// `offered` binds the steer item and says it was the read signal; an echo
/// that moved the record first keeps its signal.
#[test]
fn claude_offered_binds_the_steer_item() {
    let f = Fixture::new();
    let owner = || NewInput {
        kind: InputKind::Owner,
        sender: InputSender { kind: SenderKind::Owner, id: "owner".into(), name: "owner".into(), host: None, cutex_session_id: None },
        intent: InputIntent { strategy: Some(input_ledger::Strategy::Auto), delivery_mode: None, timing: Timing::NextStep },
        preview: "steer".into(), ask_id: None, remote: None,
    };
    for id in ["c-1", "c-2"] {
        f.ledger.transition(TO, id, Expect::NEW, Next::received().create(owner()).surface(Surface::NextUp)).unwrap();
        f.ledger.transition(TO, id, Expect::ANY, Next::sending()).unwrap();
        f.ledger.transition(TO, id, Expect::ANY, Next::accepted().turn("turn-1").stage(Stage::Registered)).unwrap();
    }
    f.ledger.transition(TO, "c-2", Expect::ANY, Next::accepted().surface(Surface::Timeline).timeline_item("steer-c-2")
        .details(json!({READ_SIGNAL: "steerEcho"}))).unwrap();
    let offered = |id: &str| json!({"threadId": "t", "turnId": "turn-1", "messageId": format!("steer-{id}"), "clientId": id, "stage": "offered"});
    f.on(|| {
        claude_input_stage(TO, "claude/input/stageChanged", Some(&offered("c-1")));
        claude_input_stage(TO, "claude/input/stageChanged", Some(&offered("c-2")));
    });
    let c1 = f.record("c-1").unwrap();
    assert_eq!((c1.surface, c1.stage, c1.timeline_item_id.as_deref()), (Surface::Timeline, Some(Stage::Offered), Some("steer-c-1")));
    assert_eq!(c1.details[READ_SIGNAL], "offered");
    let c2 = f.record("c-2").unwrap();
    assert_eq!((c2.stage, c2.details[READ_SIGNAL].as_str()), (Some(Stage::Offered), Some("steerEcho")));
}
