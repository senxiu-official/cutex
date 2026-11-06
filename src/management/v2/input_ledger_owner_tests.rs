//! Block C2 (054 §2.9 owner rows): every mapped row as a state sequence and
//! surface trace on a private ledger, the reason / error mappings, and the
//! switch-off no-ops.

use super::*;
use crate::management::v2::input_ledger::{InputLedger, Transition, DATABASE_FILE};

struct Fixture {
    root: std::path::PathBuf,
    ledger: InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-c2-ledger-{}", uuid::Uuid::new_v4().simple()));
        Self { ledger: InputLedger::open(root.join(DATABASE_FILE)), root }
    }

    fn record(&self, id: &str) -> Option<InputRecord> {
        self.ledger.get("s1", &[id.to_string()]).unwrap().records.pop()
    }

    /// Apply `step` the way `run` does; returns the transitions made.
    fn step(&self, id: &str, step: Step, strategy: UserInputStrategy) -> Vec<Transition> {
        let create = new_input(strategy, &[json!({"type": "text", "text": format!("hello {id}")})]);
        let current = self.record(id);
        plan(current.as_ref(), &step, Some(&create))
            .into_iter()
            .map(|next| {
                self.ledger
                    .transition("s1", id, Expect::ANY, next)
                    .unwrap_or_else(|error| panic!("{id} {step:?}: {error}"))
            })
            .collect()
    }

    fn step_no_create(&self, id: &str, step: Step) -> Vec<Transition> {
        let current = self.record(id);
        plan(current.as_ref(), &step, None)
            .into_iter()
            .map(|next| self.ledger.transition("s1", id, Expect::ANY, next).unwrap())
            .collect()
    }

    /// `state{reason}(evidence)` per transition, and the surfaces seen.
    fn trace(&self, steps: &[Vec<Transition>]) -> (Vec<String>, Vec<&'static str>) {
        let mut labels = Vec::new();
        let mut surfaces: Vec<&'static str> = Vec::new();
        for transition in steps.iter().flatten() {
            let record = &transition.record;
            let mut label = record.state.as_str().to_string();
            if let Some(reason) = &record.reason {
                label.push_str(&format!("{{{reason}}}"));
            }
            if let Some(evidence) = record.evidence {
                label.push_str(&format!("({evidence})"));
            }
            labels.push(label);
            if surfaces.last() != Some(&record.surface.as_str()) {
                surfaces.push(record.surface.as_str());
            }
        }
        (labels, surfaces)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

use UserInputStrategy::{Auto, Interrupt, Queue};

#[test]
fn idle_auto_runs_received_sending_in_context_on_the_timeline() {
    let f = Fixture::new();
    let steps = vec![
        f.step("m1", Step::Received { surface: Surface::Timeline }, Auto),
        f.step("m1", Step::Started { turn_id: Some("t1".into()) }, Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["timeline"]);
    let record = f.record("m1").unwrap();
    assert_eq!(record.turn_id.as_deref(), Some("t1"));
    assert_eq!((record.kind, record.sender.kind), (InputKind::Owner, SenderKind::Owner));
    assert_eq!(record.intent.strategy, Some(Strategy::Auto));
    assert_eq!(record.preview, "hello m1");
    // The submit outcome repeats `started`: nothing changes.
    assert!(f.step("m1", Step::Started { turn_id: Some("t1".into()) }, Auto).is_empty());
}

#[test]
fn busy_steer_is_accepted_with_its_turn_then_in_context_by_the_codex_echo() {
    let f = Fixture::new();
    let steps = vec![
        f.step("m1", Step::Received { surface: Surface::Timeline }, Auto),
        f.step("m1", Step::Steered { turn_id: Some("t9".into()), stage: None, generation: None }, Auto),
        // The submit outcome repeats it: unchanged, no revision used.
        f.step("m1", Step::Steered { turn_id: Some("t9".into()), stage: None, generation: None }, Auto)
            .into_iter()
            .filter(|transition| transition.changed)
            .collect(),
        f.step_no_create("m1", Step::ClientIdEcho),
        f.step_no_create("m1", Step::ClientIdEcho),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "sending", "accepted", "inContext(clientIdEcho)"]);
    assert_eq!(surfaces, ["timeline"]);
    assert_eq!(steps[1][1].record.turn_id.as_deref(), Some("t9"));
}

#[test]
fn an_echo_before_acceptance_changes_nothing_and_an_unknown_record_is_never_created() {
    let f = Fixture::new();
    assert!(f.step_no_create("ghost", Step::ClientIdEcho).is_empty());
    assert!(f.step_no_create("ghost", Step::Removed).is_empty());
    assert!(f.step_no_create("ghost", Step::AlreadyInContext).is_empty(), "deduplicated: no new record");
    assert!(f.record("ghost").is_none());
    f.step("m1", Step::Received { surface: Surface::Timeline }, Auto);
    assert!(f.step_no_create("m1", Step::ClientIdEcho).is_empty());
    // A turn/start input ignores a later echo.
    f.step("m1", Step::Started { turn_id: None }, Auto);
    assert!(f.step_no_create("m1", Step::ClientIdEcho).is_empty());
}

#[test]
fn explicit_queue_waits_in_next_up_and_moves_to_the_timeline_once_sent() {
    let f = Fixture::new();
    let steps = vec![
        f.step("q1", Step::Received { surface: creation_surface(Queue, false, false) }, Queue),
        f.step("q1", Step::Waiting(waiting_reason(Some("explicitQueue"), false)), Queue),
        f.step("q1", Step::Started { turn_id: Some("t2".into()) }, Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "waiting{explicitQueue}", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
}

#[test]
fn queue_steer_is_accepted_and_moves_to_the_timeline() {
    let f = Fixture::new();
    f.step("q1", Step::Received { surface: Surface::NextUp }, Queue);
    f.step("q1", Step::Waiting("explicitQueue"), Queue);
    let steered = f.step("q1", Step::Steered { turn_id: Some("t3".into()), stage: None, generation: None }, Auto);
    let last = &steered.last().unwrap().record;
    // r274.2: accepted is not read yet; the echo moves it.
    assert_eq!((last.state, last.surface), (InputState::Accepted, Surface::NextUp));
    let echo = f.step_no_create("q1", Step::ClientIdEcho);
    assert_eq!((echo[0].record.state, echo[0].record.surface), (InputState::InContext, Surface::Timeline));
}

#[test]
fn cutex_queued_auto_stays_on_the_timeline() {
    for wire in ["turnFinishing", "adapterQueued", "hostBusy", "turnRunning", "imageBusy", "delegationLease", "ownerQueueAhead", "joinRefused"] {
        let f = Fixture::new();
        let steps = vec![
            f.step("a1", Step::Received { surface: creation_surface(Auto, false, false) }, Auto),
            f.step("a1", Step::Waiting(waiting_reason(Some(wire), false)), Auto),
            f.step("a1", Step::Started { turn_id: None }, Auto),
        ];
        let (labels, surfaces) = f.trace(&steps);
        assert_eq!(labels.len(), 4, "{wire}: {labels:?}");
        assert_eq!(surfaces, ["timeline"], "{wire}: never nextUp");
    }
}

#[test]
fn ride_along_ahead_is_a_ledger_reason_apart_from_a_delegation_lease() {
    let f = Fixture::new();
    f.step("a1", Step::Received { surface: Surface::Timeline }, Auto);
    let waiting = f.step("a1", Step::Waiting("rideAlongAhead"), Auto);
    assert_eq!(waiting[0].record.reason.as_deref(), Some("rideAlongAhead"));
    assert_eq!(waiting[0].record.surface, Surface::Timeline);
}

#[test]
fn held_input_runs_held_sending_accepted_or_fails_on_admission_timeout() {
    let f = Fixture::new();
    let steps = vec![
        f.step("h1", Step::Received { surface: Surface::Timeline }, Auto),
        f.step("h1", Step::Held, Auto),
        f.step("h1", Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, Auto),
    ];
    assert_eq!(f.trace(&steps).0, ["received", "held{hostBusy}", "sending", "accepted"]);
    let steps = vec![
        f.step("h2", Step::Received { surface: Surface::Timeline }, Auto),
        f.step("h2", Step::Held, Auto),
        f.step_no_create("h2", Step::Failed("admission_timeout")),
        // The expiry and the ledger's own deadline may both report it.
        f.step_no_create("h2", Step::Failed("admission_timeout")),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "held{hostBusy}", "failed{admission_timeout}"]);
    assert_eq!(surfaces, ["timeline"]);
    let failed = f.record("h2").unwrap();
    assert_eq!(failed.error.unwrap().code, "admission_timeout");
    assert_eq!(failed.actions, vec![ledger::Action::Retry, ledger::Action::Dismiss]);
    // Held behind a waiting record (an owner retry): waiting{hostBusy}.
    f.step("h3", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("h3", Step::Waiting("explicitQueue"), Auto);
    assert_eq!(f.step("h3", Step::Held, Auto)[0].record.reason.as_deref(), Some("hostBusy"));
}

#[test]
fn a_resubmitted_failed_input_is_reopened_with_the_same_id() {
    let f = Fixture::new();
    f.step("r1", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("r1", Step::Failed("runtimeRefused"), Auto);
    let retried = f.step("r1", Step::Started { turn_id: None }, Auto);
    let states: Vec<_> = retried.iter().map(|transition| transition.record.state.as_str()).collect();
    assert_eq!(states, ["waiting", "sending", "inContext"]);
    assert_eq!(retried[0].record.reason.as_deref(), Some("ownerRetry"));
    assert_eq!(f.record("r1").unwrap().attempt, 2);
    // A failure of a settled record changes nothing.
    assert!(f.step("r1", Step::Failed("routeLost"), Auto).is_empty());
}

#[test]
fn offline_start_waits_agent_offline_in_next_up_then_moves_once() {
    let f = Fixture::new();
    let surface = creation_surface(Auto, true, false);
    let steps = vec![
        f.step("o1", Step::Received { surface }, Auto),
        f.step("o1", Step::Waiting("agentOffline"), Auto),
        f.step("o1", Step::Started { turn_id: Some("t1".into()) }, Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "waiting{agentOffline}", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    assert_eq!(f.record("o1").unwrap().intent.strategy, Some(Strategy::Auto), "the caller's strategy, not the queue rewrite");
}

#[test]
fn paused_session_puts_auto_and_interrupt_in_next_up() {
    assert_eq!(creation_surface(Auto, false, true), Surface::NextUp);
    assert_eq!(creation_surface(Interrupt, false, true), Surface::NextUp);
    assert_eq!(creation_surface(Interrupt, false, false), Surface::Timeline);
    assert_eq!(creation_surface(Queue, false, false), Surface::NextUp);
}

#[test]
fn unknown_outcome_is_uncertain_and_accepts_late_echo_evidence() {
    let f = Fixture::new();
    let steps = vec![
        f.step("u1", Step::Received { surface: Surface::Timeline }, Auto),
        f.step("u1", Step::Uncertain("outcomeUnknown"), Auto),
        f.step_no_create("u1", Step::ClientIdEcho),
    ];
    assert_eq!(f.trace(&steps).0, ["received", "sending", "uncertain{outcomeUnknown}", "inContext(clientIdEcho)"]);
    // From accepted (a steer whose turn was lost later): uncertain directly.
    f.step("u2", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("u2", Step::Steered { turn_id: Some("t".into()), stage: None, generation: None }, Auto);
    assert_eq!(f.step("u2", Step::Uncertain("outcomeUnknown"), Auto).len(), 1);
}

#[test]
fn queue_remove_discards_and_leaves_every_surface() {
    let f = Fixture::new();
    f.step("q1", Step::Received { surface: Surface::NextUp }, Queue);
    f.step("q1", Step::Waiting("explicitQueue"), Queue);
    let removed = f.step_no_create("q1", Step::Removed);
    assert_eq!(removed.len(), 1);
    let record = &removed[0].record;
    assert_eq!((record.state, record.reason.as_deref(), record.surface), (InputState::Discarded, Some("ownerRemoved"), Surface::None));
    assert!(f.step_no_create("q1", Step::Removed).is_empty());
}

#[test]
fn a_queue_item_native_already_has_is_in_context_from_history() {
    let f = Fixture::new();
    f.step("q1", Step::Received { surface: Surface::NextUp }, Queue);
    f.step("q1", Step::Waiting("explicitQueue"), Queue);
    let done = f.step_no_create("q1", Step::AlreadyInContext);
    let last = &done.last().unwrap().record;
    assert_eq!((last.state, last.evidence, last.surface), (InputState::InContext, Some(Evidence::HistoryRead), Surface::Timeline));
}

#[test]
fn records_from_before_the_switch_are_created_on_first_use() {
    let f = Fixture::new();
    // A queue item from before the switch is flushed: received → sending → inContext.
    let started = f.step("old", Step::Started { turn_id: None }, Auto);
    let states: Vec<_> = started.iter().map(|transition| transition.record.state.as_str()).collect();
    assert_eq!(states, ["received", "sending", "inContext"]);
}

#[test]
fn errors_map_to_listed_reasons_and_earlier_submits_are_left_alone() {
    let error = |code: &str, stage: &str, unknown: bool| UserInputExecutionError {
        stage: stage.into(),
        code: code.into(),
        message: String::new(),
        retryable: false,
        details: json!({}),
        outcome_unknown: unknown,
    };
    assert_eq!(error_step(&error("idempotency_conflict", "route", false)), None);
    assert_eq!(error_step(&error("user_input_in_progress", "route", false)), None);
    assert_eq!(error_step(&error("app_server_unavailable", "interrupt", true)), Some(Step::Failed("interrupt_timeout")));
    assert_eq!(error_step(&error("request_outcome_unknown", "native_request", true)), Some(Step::Uncertain("outcomeUnknown")));
    assert_eq!(error_step(&error("session_offline", "route", false)), Some(Step::Failed("session_offline")));
    assert_eq!(error_step(&error("native_request_failed", "native_request", false)), Some(Step::Failed("runtimeRefused")));
    assert_eq!(error_step(&error("event_persistence_unavailable", "route", false)), Some(Step::Failed("routeLost")));
    // Every owner error code maps to a listed failed / uncertain reason.
    let extra = ["native_request_failed", "fork_history_signature_rejected", "internal_error", "archive_blocks_input"];
    for code in super::super::queue_order::OWNER_INPUT_ERROR_CODES.iter().chain(extra.iter()) {
        for unknown in [false, true] {
            match error_step(&error(code, "route", unknown)) {
                None => {}
                Some(Step::Failed(reason)) => assert!(ledger::FAILED_REASONS.contains(&reason), "{code}: {reason}"),
                Some(Step::Uncertain(reason)) => assert!(ledger::UNCERTAIN_REASONS.contains(&reason), "{code}: {reason}"),
                other => panic!("{code}: {other:?}"),
            }
        }
    }
    let f = Fixture::new();
    f.step("e1", Step::Received { surface: Surface::Timeline }, Auto);
    let failed = f.step("e1", Step::Failed("runtimeRefused"), Auto);
    assert_eq!(failed[0].record.surface, Surface::Timeline);
}

#[test]
fn every_reason_this_block_writes_is_listed_for_its_state() {
    let wires = super::super::queue_order::QUEUED_REASONS.iter().map(|reason| Some(*reason)).chain([None]);
    for wire in wires {
        for paused in [false, true] {
            let reason = waiting_reason(wire, paused);
            assert!(ledger::WAITING_REASONS.contains(&reason), "{wire:?}: {reason}");
        }
    }
    assert_eq!(waiting_reason(Some("paused"), true), "stopHold");
    assert_eq!(waiting_reason(Some("paused"), false), "paused");
    // Literals in this block's sources.
    let sources = [include_str!("input_ledger_owner.rs"), include_str!("user_input.rs")];
    let calls = [
        ("Next::held(", InputState::Held), ("Next::waiting(", InputState::Waiting),
        ("Next::uncertain(", InputState::Uncertain), ("Next::failed(", InputState::Failed),
        ("Next::discarded(", InputState::Discarded), ("Step::Waiting(", InputState::Waiting),
        ("Step::Failed(", InputState::Failed), ("Step::Uncertain(", InputState::Uncertain),
    ];
    let mut seen = 0;
    for source in sources {
        let source = source.replace("\r\n", "\n");
        for (call, state) in calls {
            for rest in source.split(call).skip(1) {
                let Some(rest) = rest.strip_prefix('"') else { continue };
                let reason = &rest[..rest.find('"').unwrap()];
                seen += 1;
                assert!(state.reasons().unwrap().contains(&reason), "unlisted {state} reason {reason}");
            }
        }
    }
    assert!(seen >= 8);
    // owner_block_reason / lease_reason return waiting reasons only.
    let functions = [
        (include_str!("user_input.rs"), "fn owner_block_reason"),
        (include_str!("input_ledger_owner.rs"), "pub fn lease_reason"),
    ];
    for (source, function) in functions {
        let source = source.replace("\r\n", "\n");
        let body = source.split(function).nth(1).unwrap();
        let body = &body[..body.find("\n}\n").unwrap()];
        let chunks: Vec<&str> = body.split('"').collect();
        let mut returned = 0;
        for pair in chunks.windows(2).step_by(2) {
            let (before, literal) = (pair[0].trim_end(), pair[1]);
            if before.ends_with("=>") || before.ends_with("return") {
                returned += 1;
                assert!(ledger::WAITING_REASONS.contains(&literal), "{function}: {literal}");
            }
        }
        assert!(returned >= 3, "{function}");
    }
}

#[test]
fn switch_off_writes_nothing_and_adds_no_fields() {
    let pin = ledger::pin_enabled(Some(false));
    let mut result = json!({"revision": 1, "items": [], "admission": {"revision": 1, "items": []}, "admissionFresh": false});
    let before = result.clone();
    add_queue_list_fields(&mut result, "s-off", false);
    add_result_input(&mut result, "s-off", "m1");
    assert_eq!(result, before, "byte-for-byte the r273.1 result");
    assert!(result_input("s-off", "m1").is_none());
    note_wait_reason("s-off", "m1", "rideAlongAhead");
    assert_eq!(take_wait_reason("s-off", "m1"), None);
    assert!(lease_reason("s-off").is_none());
    assert!(!super::super::admission::owner_first("s-off"), "decision 1 is behind the switch");
    drop(pin);
}

#[test]
fn offline_context_is_scoped_to_the_call() {
    assert_eq!(offline_start(), None);
    let inside = with_offline_start(Interrupt, true, offline_start);
    assert_eq!(inside, Some((Interrupt, true)));
    assert_eq!(offline_start(), None);
}

#[test]
fn owner_user_items_only() {
    // r274.2 §9.1: Claude user items (steer echoes) count too.
    let item = |method: &str, kind: &str| json!({"method": method, "params": {"item": {"type": kind, "clientId": "c"}}});
    assert!(is_owner_user_item(&item("item/started", "userMessage")));
    assert!(is_owner_user_item(&item("item/completed", "userMessage")));
    assert!(!is_owner_user_item(&item("item/started", "agentMessage")));
    assert!(!is_owner_user_item(&item("turn/started", "userMessage")));
}

#[test]
fn preview_takes_the_text_items() {
    let input = [
        json!({"type": "text", "text": "look"}),
        json!({"type": "localImage", "path": "/x.png"}),
        json!({"type": "text", "text": "here"}),
    ];
    assert_eq!(preview_text(&input), "look [image] here");
}

/// The S7 hook: holds the host authority fence for N seconds (owner submits
/// see a busy host), then releases it; refuses a malformed body.
#[cfg(debug_assertions)]
#[test]
fn test_hook_holds_the_host_fence_for_the_requested_seconds() {
    use std::io::Read;
    if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none() {
        return;
    }
    let call = |body: &str| -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let request = crate::http::server::SimpleHttpRequest {
            method: "POST".into(),
            path: TEST_HOLD_FENCE_PATH.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        };
        handle_test_hold_fence(&mut server, &request).unwrap();
        drop(server);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        response
    };
    assert!(call(r#"{"seconds": 0}"#).contains("400"));
    assert!(call(r#"{"seconds": 1, "x": 1}"#).contains("400"));
    let busy = || matches!(
        crate::asks::pause_admission::try_with_authority(|_| Ok(())).unwrap(),
        crate::asks::pause_admission::Admission::Deferred
    );
    let response = call(r#"{"seconds": 1}"#);
    assert!(response.contains("\"held\":true"), "{response}");
    assert!(busy(), "the fence is held");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while busy() {
        assert!(std::time::Instant::now() < until, "the fence is released after the hold");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Owner dismiss (decision 9): an uncertain owner input still in the
/// Management queue (a queue steer / flush with an unknown outcome) leaves
/// the queue, so it no longer blocks the items behind it.
#[test]
fn dismiss_removes_the_uncertain_owner_inputs_queue_item() {
    if std::env::var_os("CUTEX_TEST_PRIVATE_HOME").is_none() {
        return;
    }
    let session = format!("c2-dismiss-{}", uuid::Uuid::new_v4().simple());
    let repository = super::super::user_input::user_input_repository().unwrap();
    let params = UserInputSubmitParams {
        client_user_message_id: "d1".into(),
        origin: super::super::user_input::UserInputOrigin {
            kind: super::super::user_input::UserInputOriginKind::Backend,
            client_id: "c".into(),
        },
        strategy: Queue,
        input: vec![json!({"type": "text", "text": "hi"})],
        expected_turn_id: None,
        join_delegation: None,
    };
    repository.enqueue(&session, "thread", "request-1", &params).unwrap();
    assert_eq!(repository.list(&session).unwrap().1.len(), 1);
    let f = Fixture::new();
    f.step("d1", Step::Received { surface: Surface::NextUp }, Queue);
    f.step("d1", Step::Waiting("explicitQueue"), Queue);
    f.step("d1", Step::Uncertain("outcomeUnknown"), Auto);
    let mut record = f.record("d1").unwrap();
    assert!(ledger::dismissible(&record));
    record.cutex_session_id = session.clone();
    dismiss(&record).unwrap();
    assert!(repository.list(&session).unwrap().1.is_empty());
    // Nothing left: a second dismiss is a no-op.
    dismiss(&record).unwrap();
}

/// §2.7: the result fields this block adds validate against the methods
/// schema (submit / steer / remove `input`, queue/list ledger fields and
/// `admission.stale`).
#[test]
fn result_fields_match_the_methods_schema() {
    let f = Fixture::new();
    f.step("m1", Step::Received { surface: Surface::NextUp }, Queue);
    let waiting = f.step("m1", Step::Waiting("explicitQueue"), Queue).pop().unwrap();
    let record = serde_json::to_value(&waiting.record).unwrap();
    let validate = |method: &str, result: Value| {
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2, "requestId": "request-1", "cutexSessionId": "s1",
            "cutex": {"method": method, "result": result},
        })).unwrap_or_else(|error| panic!("{method}: {error}"));
    };
    validate("cutex/userInput/submit", json!({
        "clientUserMessageId": "m1", "disposition": "queued", "appServerAccepted": false,
        "nativeRequestId": null, "turnId": null, "queue": null, "input": record,
    }));
    validate("cutex/userInput/queue/remove", json!({"revision": 2, "affectedQueueIds": ["q"], "input": record}));
    validate("cutex/userInput/queue/flush", json!({"revision": 2, "affectedQueueIds": ["q"], "input": record}));
    validate("cutex/userInput/queue/steer", json!({
        "disposition": "steered", "turnId": "t", "clientUserMessageId": "m1", "queueRevision": 3, "input": record,
    }));
    let admission = serde_json::to_value(super::super::admission::AdmissionSnapshot::default()).unwrap();
    let mut list = json!({"revision": 1, "items": [], "admission": admission, "admissionFresh": false});
    list["admission"]["stale"] = json!(true);
    list["stateRevision"] = json!(waiting.state_revision);
    list["ledgerEpoch"] = json!(waiting.ledger_epoch);
    list["inputs"] = json!([record]);
    validate("cutex/userInput/queue/list", list);
}

#[test]
fn queue_merge_is_one_store_write_and_supersedes_the_merged_away_records() {
    use crate::management::v2::user_input::{parse_user_input_submit_params, QueueEnqueueDecision, UserInputRepository};
    let root = std::env::temp_dir().join(format!("cutex-c2-merge-{}", uuid::Uuid::new_v4().simple()));
    let queue = UserInputRepository::open(root.join("queue")).unwrap();
    let enqueue = |client: &str| {
        let params = parse_user_input_submit_params(&json!({"clientUserMessageId": client,
            "origin": {"kind": "other", "clientId": "desktop"}, "strategy": "queue",
            "input": [{"type": "text", "text": client, "text_elements": []}]})).unwrap();
        match queue.enqueue("s1", "thread", "req", &params).unwrap() {
            QueueEnqueueDecision::Queued(item) | QueueEnqueueDecision::Deduplicated(item) => item,
            _ => panic!("identity conflict"),
        }
    };
    let (a, b, c) = (enqueue("a"), enqueue("b"), enqueue("c"));
    let stale = queue.merge("s1", &[(a.queue_id.clone(), a.revision), (c.queue_id.clone(), c.revision + 9)], None);
    assert!(stale.is_err(), "a revision mismatch merges nothing");
    assert_eq!(queue.list("s1").unwrap().1.len(), 3);
    let (merged, removed) = queue.merge("s1", &[(a.queue_id.clone(), a.revision), (c.queue_id.clone(), c.revision)], None).unwrap().unwrap();
    assert_eq!(merged.client_user_message_id, "a");
    let texts: Vec<&str> = merged.input.iter().filter_map(|item| item["text"].as_str()).collect();
    assert_eq!(texts, vec!["a", "c"]);
    assert_eq!(removed.iter().map(|item| item.client_user_message_id.as_str()).collect::<Vec<_>>(), vec!["c"]);
    let left: Vec<String> = queue.list("s1").unwrap().1.into_iter().map(|item| item.client_user_message_id).collect();
    assert_eq!(left, vec!["a".to_string(), b.client_user_message_id]);
    // Ledger side, as queue_merged writes it.
    let f = Fixture::new();
    for id in ["a", "c"] {
        f.step(id, Step::Received { surface: ledger::Surface::NextUp }, UserInputStrategy::Queue);
        f.step(id, Step::Waiting("explicitQueue"), UserInputStrategy::Queue);
    }
    let superseded = f.ledger.transition("s1", "c", Expect::ANY,
        ledger::Next::superseded("mergedInsert").superseded_by("a".to_string())).unwrap().record;
    assert_eq!((superseded.state, superseded.surface, superseded.superseded_by.as_deref()),
        (ledger::InputState::Superseded, ledger::Surface::None, Some("a")));
    assert_eq!(f.record("a").unwrap().state, ledger::InputState::Waiting);
    let result = json!({"revision": 9, "affectedQueueIds": [merged.queue_id, removed[0].queue_id], "item": merged,
        "inputs": [f.record("a").unwrap(), superseded]});
    crate::management::v2::contract_validation::validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r",
        "cutexSessionId": "s1", "cutex": {"method": "cutex/userInput/queue/merge", "result": result}})).unwrap();
    crate::management::v2::contract_validation::validate_cutex_request(&json!({"requestId": "r", "method": "cutex/userInput/queue/merge",
        "params": {"items": [{"queueId": "q1", "expectedRevision": 1}, {"queueId": "q2", "expectedRevision": 1}]}})).unwrap();
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// r274 round 3 (Director decisions 1, 3, 4; S4/S5)

impl Fixture {
    /// Apply the pending-creation plan the route's first write uses.
    fn first(&self, id: &str, step: Step, strategy: UserInputStrategy) -> Vec<Transition> {
        self.first_observed(id, step, strategy, false)
    }

    fn first_observed(&self, id: &str, step: Step, strategy: UserInputStrategy, idle: bool) -> Vec<Transition> {
        assert!(self.record(id).is_none(), "{id}: pending records do not exist yet");
        let create = new_input(strategy, &[json!({"type": "text", "text": format!("hello {id}")})]);
        pending_plan(&create, &step, idle)
            .into_iter()
            .map(|next| self.ledger.transition("s1", id, Expect::ANY, next).unwrap_or_else(|error| panic!("{id} {step:?}: {error}")))
            .collect()
    }
}

/// Decision 3: `sending` is written before the native call; an explicit
/// refusal cutex re-queues is `sending -> waiting{reason}`; the surface stays
/// the timeline (never back to nextUp) and the resend goes `sending` again.
#[test]
fn sending_before_native_and_a_refusal_waits_again_on_the_same_surface() {
    let f = Fixture::new();
    let steps = vec![
        f.first("a1", Step::Sending { steer: false }, Auto),
        f.step("a1", Step::Waiting(waiting_reason(Some("turnFinishing"), false)), Auto),
        f.step("a1", Step::Sending { steer: false }, Auto),
        f.step("a1", Step::Started { turn_id: Some("t2".into()) }, Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "sending", "waiting{turnFinishing}", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["timeline"]);
    // A queue item refused at flush / steer: back to its own reason, still nextUp.
    let steps = vec![
        f.step("q1", Step::Received { surface: Surface::NextUp }, Queue),
        f.step("q1", Step::Waiting("explicitQueue"), Queue),
        f.step("q1", Step::Sending { steer: false }, Auto),
        f.step("q1", Step::Waiting("explicitQueue"), Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "waiting{explicitQueue}", "sending", "waiting{explicitQueue}"]);
    assert_eq!(surfaces, ["nextUp"]);
    // A refused send handed to the admission worker (allowHeld): waiting, not held.
    f.step("h1", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("h1", Step::Sending { steer: false }, Auto);
    let held = f.step("h1", Step::Held, Auto);
    assert_eq!((held[0].record.state, held[0].record.reason.as_deref()), (InputState::Waiting, Some("hostBusy")));
    // `sending` on a settled record changes nothing.
    assert!(f.step("a1", Step::Sending { steer: false }, Auto).is_empty());
    // Accepted / unknown after the pre-written `sending` add only their result.
    f.step("s1", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("s1", Step::Sending { steer: true }, Auto);
    assert_eq!(f.step("s1", Step::Steered { turn_id: Some("t".into()), stage: None, generation: None }, Auto).len(), 1);
    f.step("u1", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("u1", Step::Sending { steer: false }, Auto);
    assert_eq!(f.step("u1", Step::Uncertain("outcomeUnknown"), Auto).len(), 1);
}

/// Decision 1: an owner `auto` that cannot get the host fence (no
/// `allowHeld`) waits `waiting{hostBusy}` on nextUp and moves to the
/// timeline exactly once, when it is read (r274.2: every unread first step
/// creates it on nextUp).
#[test]
fn running_turn_host_busy_waits_in_next_up_and_moves_to_the_timeline_once() {
    let f = Fixture::new();
    let steps = vec![
        f.first("b1", Step::Waiting("hostBusy"), Auto),
        // The admission retry meets a busy fence again: nothing visible changes.
        f.step("b1", Step::Waiting("hostBusy"), Auto).into_iter().filter(|t| t.changed).collect(),
        f.step("b1", Step::Sending { steer: false }, Auto),
        f.step("b1", Step::Started { turn_id: Some("t1".into()) }, Auto),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "waiting{hostBusy}", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    // r274.2 §9.4: only a first step that starts the turn with the input (a
    // direct start, `started`) creates it on the timeline; anything else
    // (unread, failed or unknown before any turn started) on nextUp.
    let owner_reasons = ["turnRunning", "turnFinishing", "adapterQueued", "rideAlongAhead", "delegationLease", "eventLease",
        "ownerQueueAhead", "hostBusy", "paused", "stopHold", "agentOffline"];
    let mut steps: Vec<(String, Step, Surface)> = owner_reasons
        .into_iter()
        .map(|reason| (format!("w-{reason}"), Step::Waiting(reason), Surface::NextUp))
        .collect();
    steps.extend([
        ("t1".to_string(), Step::Sending { steer: false }, Surface::Timeline),
        ("t2".to_string(), Step::Sending { steer: true }, Surface::NextUp),
        ("t3".to_string(), Step::Held, Surface::NextUp),
        ("t4".to_string(), Step::Failed("runtimeRefused"), Surface::NextUp),
        ("t5".to_string(), Step::Steered { turn_id: Some("t".into()), stage: None, generation: None }, Surface::NextUp),
        ("t6".to_string(), Step::Steered { turn_id: Some("t".into()), stage: Some(Stage::Registered), generation: None }, Surface::NextUp),
        ("t7".to_string(), Step::Started { turn_id: Some("t".into()) }, Surface::Timeline),
        ("t8".to_string(), Step::Uncertain("outcomeUnknown"), Surface::NextUp),
    ]);
    for (id, step, surface) in steps {
        let created = f.first(&id, step.clone(), Auto);
        assert_eq!(created[0].record.surface, surface, "{id} {step:?}");
    }
    // An explicit `interrupt` follows the same rule (§9.4): nextUp until the
    // new turn starts with it; an idle interrupt is a direct start.
    for (id, step, surface) in [
        ("i1", Step::Waiting("hostBusy"), Surface::NextUp),
        ("i2", Step::Waiting("turnFinishing"), Surface::NextUp),
        ("i3", Step::Held, Surface::NextUp),
        ("i4", Step::Sending { steer: false }, Surface::Timeline),
        ("i5", Step::Failed("interrupt_timeout"), Surface::NextUp),
    ] {
        let created = f.first(id, step.clone(), Interrupt);
        assert_eq!(created[0].record.surface, surface, "{id} {step:?}");
    }
    // A step that never creates leaves nothing behind.
    let create = new_input(Auto, &[]);
    assert!(pending_plan(&create, &Step::ClientIdEcho, false).is_empty());
}

#[test]
fn idle_host_busy_auto_and_interrupt_stay_on_timeline_with_or_without_allow_held() {
    for strategy in [Auto, Interrupt] {
        for first in [Step::Held, Step::Waiting("hostBusy")] {
            let f = Fixture::new();
            let steps = vec![
                f.first_observed("a", first.clone(), strategy, true),
                f.step("a", Step::Sending { steer: false }, strategy),
                f.step("a", Step::Started { turn_id: Some("t".into()) }, strategy),
            ];
            assert_eq!(f.trace(&steps).1, ["timeline"], "{strategy:?} {first:?}");
            // Queue and unrelated reasons retain their existing rules.
            assert_eq!(f.first_observed("q", first, Queue, true)[0].record.surface, Surface::NextUp);
            assert_eq!(f.first_observed("p", Step::Waiting("paused"), strategy, true)[0].record.surface, Surface::NextUp);
        }
    }
}

#[test]
fn idle_host_busy_failure_retry_and_dismiss_keep_the_bubble_in_place() {
    let f = Fixture::new();
    f.first_observed("a", Step::Held, Auto, true);
    let failed = f.step("a", Step::Failed("admission_timeout"), Auto);
    assert_eq!(failed[0].record.surface, Surface::Timeline);
    assert_eq!(failed[0].record.actions, [ledger::Action::Retry, ledger::Action::Dismiss]);
    let retry = f.ledger.transition("s1", "a", Expect::ANY, Next::waiting("ownerRetry")).unwrap();
    assert_eq!((retry.record.surface, retry.record.attempt), (Surface::Timeline, 2));
    f.step("a", Step::Failed("session_offline"), Auto);
    let dismissed = f.ledger.transition("s1", "a", Expect::ANY, Next::discarded("ownerDiscarded")).unwrap();
    assert_eq!(dismissed.record.surface, Surface::None);
    let reopened = InputLedger::open(f.root.join(DATABASE_FILE));
    assert_eq!(reopened.get("s1", &["a".into()]).unwrap().records[0].surface, Surface::None);
}

/// S4: the Claude steer reply means the adapter registered the input
/// (`accepted[registered]`); a stage the adapter sent before the reply (C3
/// writes it on `sending`) is never lowered.
#[test]
fn claude_steer_reply_is_registered_and_never_lowers_an_earlier_stage() {
    let f = Fixture::new();
    f.first("c1", Step::Sending { steer: true }, Auto);
    let accepted = f.step("c1", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    assert_eq!((accepted[0].record.state, accepted[0].record.stage), (InputState::Accepted, Some(Stage::Registered)));
    // The stage arrived first: sending -> accepted[offered] (C3), then the reply.
    f.first("c2", Step::Sending { steer: true }, Auto);
    f.ledger.transition("s1", "c2", Expect::ANY, Next::accepted().stage(Stage::Offered)).unwrap();
    f.step("c2", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    let record = f.record("c2").unwrap();
    assert_eq!((record.stage, record.turn_id.as_deref()), (Some(Stage::Offered), Some("t1")));
    // Codex steers have no stage.
    f.first("x1", Step::Sending { steer: true }, Auto);
    let codex = f.step("x1", Step::Steered { turn_id: None, stage: None, generation: None }, Auto);
    assert_eq!(codex[0].record.stage, None);
}

#[test]
fn refusals_that_requeue_map_to_their_waiting_reason() {
    let refused = |reason: &str, unknown: bool| UserInputExecutionError {
        stage: "queue".into(),
        code: "steer_unavailable".into(),
        message: String::new(),
        retryable: true,
        details: json!({"reason": reason}),
        outcome_unknown: unknown,
    };
    assert_eq!(refusal_reason(&refused("turnFinishing", false)), Some("turnFinishing"));
    assert_eq!(refusal_reason(&refused("admissionBusy", false)), Some("hostBusy"));
    assert_eq!(refusal_reason(&refused("agentPaused", false)), Some("paused"));
    assert_eq!(refusal_reason(&refused("runtimeRefused", false)), None);
    assert_eq!(refusal_reason(&refused("turnFinishing", true)), None);
    for reason in ["turnFinishing", "admissionBusy", "agentPaused", "adapterQueued"] {
        let mapped = refusal_reason(&refused(reason, false)).unwrap();
        assert!(ledger::WAITING_REASONS.contains(&mapped), "{reason}: {mapped}");
    }
}

/// Decision 4: the merge is ONE batch: one transaction, one stateRevision
/// for the survivor's update and every merged-away `superseded`.
#[test]
fn queue_merge_is_one_batch_with_one_state_revision() {
    let f = Fixture::new();
    for id in ["a", "b", "c"] {
        f.step(id, Step::Received { surface: Surface::NextUp }, Queue);
        f.step(id, Step::Waiting("explicitQueue"), Queue);
    }
    let records = f.ledger.get("s1", &["a".into(), "b".into(), "c".into(), "unknown".into()]).unwrap().records;
    let batch = merge_batch(&records, "a", &["b".into(), "c".into(), "unknown".into()]);
    assert_eq!(batch.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"], "unknown records are left out");
    let transitions = f.ledger.transition_batch("s1", batch).unwrap();
    let revisions: std::collections::BTreeSet<u64> = transitions.iter().map(|t| t.state_revision).collect();
    assert_eq!(revisions.len(), 1, "one stateRevision for the merge");
    assert!(transitions.iter().all(|t| t.changed));
    let survivor = f.record("a").unwrap();
    assert_eq!((survivor.state, survivor.reason.as_deref(), survivor.surface), (InputState::Waiting, Some("explicitQueue"), Surface::NextUp));
    assert_eq!(survivor.details["mergedFrom"], json!(["b", "c"]));
    for id in ["b", "c"] {
        let record = f.record(id).unwrap();
        assert_eq!((record.state, record.reason.as_deref(), record.surface, record.superseded_by.as_deref()),
            (InputState::Superseded, Some("mergedInsert"), Surface::None, Some("a")));
        assert_eq!(record.state_revision, survivor.state_revision);
    }
    // Merged again (nothing left open to absorb): no batch.
    let records = f.ledger.get("s1", &["a".into(), "b".into()]).unwrap().records;
    assert!(merge_batch(&records, "a", &["b".into()]).is_empty());
}

#[test]
fn route_context_stays_empty_with_the_switch_off() {
    let pin = ledger::pin_enabled(Some(false));
    let params = UserInputSubmitParams {
        client_user_message_id: "off-1".into(),
        origin: super::super::user_input::UserInputOrigin {
            kind: super::super::user_input::UserInputOriginKind::Backend,
            client_id: "c".into(),
        },
        strategy: Auto,
        input: vec![json!({"type": "text", "text": "hi"})],
        expected_turn_id: None,
        join_delegation: None,
    };
    route_accepted("s-off", &params);
    native_sending("s-off", &params, true);
    native_refused("s-off", "off-1", Some("hostBusy"));
    materialize("s-off", "off-1");
    offline_waiting("s-off", "off-1");
    assert!(queue_merged("s-off", "off-1", &["off-2".into()]).is_empty());
    CONTEXT.with(|context| {
        let context = context.borrow();
        assert!(context.pending.is_none() && context.sending_from.is_none(), "switch off: nothing remembered");
    });
    drop(pin);
}

// ---------------------------------------------------------------------------
// r274.1: the native user item (054 §3.2 `timelineItemId`; the desktop also
// joins owner items exactly on the clientId).

#[test]
fn the_codex_echo_names_the_native_user_item() {
    let f = Fixture::new();
    f.step("m1", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("m1", Step::Steered { turn_id: Some("t9".into()), stage: None, generation: None }, Auto);
    let current = f.record("m1");
    for next in with_item(plan(current.as_ref(), &Step::ClientIdEcho, None), Some("native-item-1")) {
        f.ledger.transition("s1", "m1", Expect::ANY, next).unwrap();
    }
    let record = f.record("m1").unwrap();
    assert_eq!((record.state, record.evidence), (InputState::InContext, Some(Evidence::ClientIdEcho)));
    assert_eq!(record.timeline_item_id.as_deref(), Some("native-item-1"));
    // Only the inContext transition carries it.
    let nexts = with_item(plan(None, &Step::Started { turn_id: Some("t1".into()) }, Some(&new_input(Auto, &[]))), Some("x"));
    assert!(nexts.iter().all(|next| (next.timeline_item_id.is_some()) == (next.state == InputState::InContext)));
    // A turn/start input already in context is bound once by a later echo
    // (metadata only), never rebound.
    f.step("m2", Step::Received { surface: Surface::Timeline }, Auto);
    f.step("m2", Step::Started { turn_id: Some("t2".into()) }, Auto);
    let started = f.record("m2").unwrap();
    assert_eq!(started.timeline_item_id, None);
    let bound = f.ledger.bind_timeline_item("s1", "m2", Expect::rev(started.rev), "native-item-2", None).unwrap();
    assert!(bound.changed);
    assert_eq!((bound.record.state, bound.record.evidence), (InputState::InContext, Some(Evidence::TurnStartInput)));
    assert!(f.ledger.bind_timeline_item("s1", "m2", Expect::ANY, "native-item-3", None).is_err());
    // An early echo keeps its item until the acceptance is recorded.
    let mut early = VecDeque::new();
    early.push_back(("s1".to_string(), "m3".to_string(), Some("native-item-3".to_string()), Some("t3".to_string())));
    assert_eq!(take_echo(&mut early, "s1", "m3"), Some((Some("native-item-3".to_string()), Some("t3".to_string()))));
    assert_eq!(take_echo(&mut early, "s1", "m3"), None);
}

// ---------------------------------------------------------------------------
// r274.2 (owner decision 2026-11-03): an owner insert stays in nextUp until read

/// Codex busy steer: `sending{steer}` and `accepted` are nextUp; the echo
/// (the drain at the next step boundary) moves it once, bound to its item.
#[test]
fn codex_steer_stays_in_next_up_until_its_echo() {
    let f = Fixture::new();
    let sending = f.first("x1", Step::Sending { steer: true }, Auto);
    let accepted = f.step("x1", Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, Auto);
    assert_eq!(accepted[0].record.surface, Surface::NextUp, "accepted is not read yet");
    let echo: Vec<Transition> = with_item(plan(f.record("x1").as_ref(), &Step::ClientIdEcho, None), Some("item-x1"))
        .into_iter()
        .map(|next| f.ledger.transition("s1", "x1", Expect::ANY, next).unwrap())
        .collect();
    let (labels, surfaces) = f.trace(&[sending, accepted, echo]);
    assert_eq!(labels, ["received", "sending", "accepted", "inContext(clientIdEcho)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    let record = f.record("x1").unwrap();
    assert_eq!(record.timeline_item_id.as_deref(), Some("item-x1"));
    // The early echo (seen before the steer reply): accepted then inContext
    // at once, still bound.
    f.first("x2", Step::Sending { steer: true }, Auto);
    for next in with_item(plan(f.record("x2").as_ref(), &Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, None), Some("item-x2"))
        .into_iter()
        .chain(with_item(vec![Next::in_context(Evidence::ClientIdEcho)], Some("item-x2")))
    {
        f.ledger.transition("s1", "x2", Expect::ANY, next).unwrap();
    }
    let record = f.record("x2").unwrap();
    assert_eq!((record.surface, record.timeline_item_id.as_deref()), (Surface::Timeline, Some("item-x2")));
}

/// Claude: `accepted[registered]` keeps nextUp; `offered` moves once; a later
/// `contextPersisted` keeps the timeline. A steer that slips to the next
/// turn moves at `contextPersisted` with the consuming turn.
#[test]
fn claude_steer_moves_at_offered_once() {
    let f = Fixture::new();
    f.first("c1", Step::Sending { steer: true }, Auto);
    let registered = f.step("c1", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    assert_eq!((registered[0].record.stage, registered[0].record.surface), (Some(Stage::Registered), Surface::NextUp));
    let offered = f.ledger.transition("s1", "c1", Expect::ANY, Next::accepted().stage(Stage::Offered)).unwrap();
    assert_eq!(offered.record.surface, Surface::Timeline);
    let persisted = f.ledger.transition("s1", "c1", Expect::ANY, Next::in_context(Evidence::ContextPersisted)).unwrap();
    assert_eq!(persisted.record.surface, Surface::Timeline);
    f.first("c2", Step::Sending { steer: true }, Auto);
    f.step("c2", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    let slipped = f.ledger.transition("s1", "c2", Expect::ANY, Next::in_context(Evidence::ContextPersisted).turn("t2")).unwrap();
    assert_eq!((slipped.record.surface, slipped.record.turn_id.as_deref()), (Surface::Timeline, Some("t2")));
}

/// r274.2 §9.1: the Claude steer echo moves an `accepted` record to the
/// timeline, still `accepted` (`readSignal: steerEcho`), bound to
/// `steer-<clientId>`; a later `offered` only advances the stage, a repeated
/// echo changes nothing, and `inContext` comes with `contextPersisted`.
/// Echo after `offered`: only the id binds. A slipped steer takes the turn of
/// its echo and drops the earlier turn's receipt deadline.
#[test]
fn claude_steer_echo_moves_to_the_timeline_as_accepted() {
    let f = Fixture::new();
    let echo = |turn: &str, item: &str| Step::SteerEcho { turn_id: Some(turn.into()), item: Some(item.into()) };
    f.first("e1", Step::Sending { steer: true }, Auto);
    f.step("e1", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    let moved = f.step_no_create("e1", echo("t1", "steer-e1"));
    let record = &moved[0].record;
    assert_eq!((record.state, record.stage, record.surface), (InputState::Accepted, Some(Stage::Registered), Surface::Timeline));
    assert_eq!((record.timeline_item_id.as_deref(), record.turn_id.as_deref()), (Some("steer-e1"), Some("t1")));
    assert_eq!(record.details[ledger::READ_SIGNAL], "steerEcho");
    assert!(!record.actions.contains(&ledger::Action::InterruptSend));
    assert!(!f.step_no_create("e1", echo("t1", "steer-e1"))[0].changed, "a repeated echo is a no-op");
    let offered = f.ledger.transition("s1", "e1", Expect::ANY, Next::accepted().stage(Stage::Offered).timeline_item("steer-e1")).unwrap();
    assert_eq!((offered.record.surface, offered.record.details[ledger::READ_SIGNAL].as_str()), (Surface::Timeline, Some("steerEcho")));
    let persisted = f.ledger.transition("s1", "e1", Expect::ANY, Next::in_context(Evidence::ContextPersisted)).unwrap();
    assert_eq!((persisted.record.state, persisted.record.surface), (InputState::InContext, Surface::Timeline));

    // offered first (it moved the record): the echo is a no-op.
    f.first("e2", Step::Sending { steer: true }, Auto);
    f.step("e2", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    f.ledger.transition("s1", "e2", Expect::ANY, Next::accepted().stage(Stage::Offered).timeline_item("steer-e2")).unwrap();
    assert!(!f.step_no_create("e2", echo("t1", "steer-e2"))[0].changed);

    // A steer that slipped to turn t2: its echo there.
    f.first("e3", Step::Sending { steer: true }, Auto);
    f.step("e3", Step::Steered { turn_id: Some("t1".into()), stage: Some(Stage::Registered), generation: None }, Auto);
    let graced = f.ledger.transition("s1", "e3", Expect::ANY, Next::accepted().deadline(Some(chrono::Utc::now()))).unwrap();
    assert!(graced.record.deadline.is_some());
    let slipped = f.step_no_create("e3", echo("t2", "user-item-t2"));
    let record = &slipped[0].record;
    assert_eq!((record.surface, record.turn_id.as_deref(), record.timeline_item_id.as_deref()), (Surface::Timeline, Some("t2"), Some("user-item-t2")));
    assert_eq!(record.deadline, None);
    // r275: the next turn started with it, so it is that turn's start input at
    // once (never `registered` until the receipt).
    let settled = &slipped.last().unwrap().record;
    assert_eq!((settled.state, settled.evidence, settled.surface, settled.turn_id.as_deref()),
        (InputState::InContext, Some(Evidence::TurnStartInput), Surface::Timeline, Some("t2")));
    assert_eq!(settled.timeline_item_id.as_deref(), Some("user-item-t2"));
    assert!(!f.step_no_create("e3", echo("t2", "user-item-t2")).iter().any(|r| r.changed), "a repeated echo is a no-op");

    // Not a receipt: nothing for records that are not `accepted`.
    f.first("e4", Step::Sending { steer: true }, Auto);
    assert!(f.step_no_create("e4", echo("t1", "steer-e4")).is_empty());
    assert!(plan(None, &echo("t1", "steer-e5"), None).is_empty());
}

/// Pre-context failures and unknown outcomes stay on nextUp with their
/// actions; the owner's retry goes (back) to nextUp.
#[test]
fn unread_failures_stay_in_next_up_and_a_retry_goes_to_next_up() {
    let f = Fixture::new();
    f.first("u1", Step::Sending { steer: true }, Auto);
    f.step("u1", Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, Auto);
    let lost = f.ledger.transition("s1", "u1", Expect::ANY, Next::uncertain("turnInterrupted")).unwrap();
    assert_eq!((lost.record.surface, lost.record.actions.clone()), (Surface::NextUp, vec![ledger::Action::Retry, ledger::Action::Dismiss]));
    f.first("f1", Step::Sending { steer: true }, Auto);
    f.step("f1", Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, Auto);
    let failed = f.ledger.transition("s1", "f1", Expect::ANY, Next::failed("notDelivered")).unwrap();
    assert_eq!((failed.record.surface, failed.record.actions.clone()), (Surface::NextUp, vec![ledger::Action::Retry, ledger::Action::Dismiss]));
    // A direct start that is lost on the timeline: the retry goes to nextUp.
    f.first("d1", Step::Sending { steer: false }, Auto);
    f.step("d1", Step::Uncertain("outcomeUnknown"), Auto);
    let retry = f.step("d1", Step::Sending { steer: false }, Auto);
    assert_eq!((retry[0].record.state, retry[0].record.surface), (InputState::Waiting, Surface::NextUp));
    let started = f.step("d1", Step::Started { turn_id: Some("t2".into()) }, Auto);
    assert_eq!(started.last().unwrap().record.surface, Surface::Timeline);
}

/// r274.2 §9.4 (owner rule 2026-11-03): only a submission that starts the
/// agent's turn at once goes to the timeline at submit; an explicit
/// `interrupt` and a send into a paused session wait on nextUp and move
/// once, in the turn-start write itself (`inContext{turnStartInput}`).
#[test]
fn interrupt_and_paused_sends_move_at_the_turn_start() {
    let f = Fixture::new();
    // `interrupt` while a turn runs: `materialize` creates it on nextUp; the
    // stop is accepted, the new turn starts with it.
    let steps = vec![
        f.step("i1", Step::Received { surface: Surface::NextUp }, Interrupt),
        f.step("i1", Step::Sending { steer: false }, Interrupt),
        f.step("i1", Step::Started { turn_id: Some("t2".into()) }, Interrupt),
    ];
    let (labels, surfaces) = f.trace(&steps);
    assert_eq!(labels, ["received", "sending", "inContext(turnStartInput)"]);
    assert_eq!(surfaces, ["nextUp", "timeline"]);
    let moved = steps.last().unwrap().last().unwrap();
    assert_eq!((moved.record.surface, moved.record.turn_id.as_deref()), (Surface::Timeline, Some("t2")));
    // The stop timed out: failed on nextUp with its actions; a refused start
    // after the stop waits on nextUp too.
    f.step("i2", Step::Received { surface: Surface::NextUp }, Interrupt);
    let failed = f.step("i2", Step::Failed("interrupt_timeout"), Interrupt);
    assert_eq!((failed[0].record.surface, failed[0].record.actions.clone()), (Surface::NextUp, vec![ledger::Action::Retry, ledger::Action::Dismiss]));
    f.step("i3", Step::Received { surface: Surface::NextUp }, Interrupt);
    f.step("i3", Step::Sending { steer: false }, Interrupt);
    assert_eq!(f.step("i3", Step::Waiting("adapterQueued"), Interrupt)[0].record.surface, Surface::NextUp);
    // A session held `stopHold` (Esc): the owner's `auto` lifts it and starts
    // at once — created on nextUp at the route, moved by the start itself.
    let steps = vec![
        f.step("p1", Step::Received { surface: Surface::NextUp }, Auto),
        f.step("p1", Step::Sending { steer: false }, Auto),
        f.step("p1", Step::Started { turn_id: Some("t3".into()) }, Auto),
    ];
    assert_eq!(f.trace(&steps).1, ["nextUp", "timeline"]);
    // An agent / project pause starts nothing: it stays in nextUp.
    let steps = vec![
        f.step("p2", Step::Received { surface: Surface::NextUp }, Auto),
        f.step("p2", Step::Waiting("paused"), Auto),
    ];
    assert_eq!(f.trace(&steps), (vec!["received".to_string(), "waiting{paused}".to_string()], vec!["nextUp"]));
}

/// r274.2 §9.4: an owner resend refused before any native IO (the runtime is
/// still away) is failed again with its actions, never left waiting.
#[test]
fn a_resend_refused_before_native_io_fails_again() {
    let f = Fixture::new();
    f.first("r1", Step::Sending { steer: true }, Auto);
    f.step("r1", Step::Steered { turn_id: Some("t1".into()), stage: None, generation: None }, Auto);
    f.ledger.transition("s1", "r1", Expect::ANY, Next::uncertain("runtimeLost")).unwrap();
    f.ledger.transition("s1", "r1", Expect::ANY, Next::waiting("ownerRetry")).unwrap();
    let failed = f.step_no_create("r1", Step::Failed("session_offline"));
    let record = &failed[0].record;
    assert_eq!((record.state, record.reason.as_deref(), record.surface), (InputState::Failed, Some("session_offline"), Surface::NextUp));
    assert_eq!(record.actions, vec![ledger::Action::Retry, ledger::Action::Dismiss]);
    // Any other waiting owner record (its queue item stays) is not failed.
    f.step("q1", Step::Received { surface: Surface::NextUp }, Queue);
    f.step("q1", Step::Waiting("explicitQueue"), Queue);
    assert!(f.step_no_create("q1", Step::Failed("session_offline")).is_empty());
}
