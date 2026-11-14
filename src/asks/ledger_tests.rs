//! 054 §2.9 rows of block C4: every row as a state sequence + surface on a
//! private ledger. The store hook is exercised through [`store_jobs`] on the
//! same row diff the AskStore commits; the scheduler facts through their job
//! builders. No default ledger and no switch: nothing here touches shared
//! state.

use super::*;
use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};
use crate::management::v2::input_ledger::{InputLedger, Surface};

struct Run {
    root: std::path::PathBuf,
    ledger: InputLedger,
    state: AskState,
    /// Labels of every applied transition, per input id.
    paths: std::collections::BTreeMap<String, Vec<String>>,
    transitions: usize,
}

fn label(record: &InputRecord) -> String {
    let mut out = record.state.as_str().to_string();
    if let Some(reason) = &record.reason {
        out.push_str(&format!("{{{reason}}}"));
    }
    if let Some(evidence) = record.evidence {
        out.push_str(&format!("({evidence})"));
    }
    out
}

impl Run {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-ledger-asks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let ledger = InputLedger::open(root.join(il::DATABASE_FILE));
        Self { root, ledger, state: AskState::default(), paths: Default::default(), transitions: 0 }
    }

    fn apply(&mut self, jobs: Vec<Job>) {
        for job in &jobs {
            for transition in apply_job(&self.ledger, job) {
                self.transitions += 1;
                let path = self.paths.entry(transition.record.input_id.clone()).or_default();
                let label = label(&transition.record);
                if path.last() != Some(&label) {
                    path.push(label);
                }
            }
        }
    }

    /// One store write: the hook's jobs for exactly the rows it changed.
    fn write(&mut self, mutate: impl FnOnce(&mut AskState)) {
        let jobs = self.commit(mutate);
        self.apply(jobs);
    }

    /// One store write whose jobs are returned, not applied (a writer thread
    /// that has not run them yet). Bumps `revision` like the store does.
    fn commit(&mut self, mutate: impl FnOnce(&mut AskState)) -> Vec<Job> {
        let before = self.state.clone();
        mutate(&mut self.state);
        self.state.revision += 1;
        let changes = crate::asks::store::ask_state_changes(&before, &self.state).unwrap();
        store_jobs(&before, &self.state, &changes)
    }

    fn record(&self, session: &str, id: &str) -> InputRecord {
        self.ledger.get(session, &[id.to_string()]).unwrap().records.pop().expect("record")
    }

    fn path(&self, id: &str) -> Vec<&str> {
        self.paths.get(id).map(|path| path.iter().map(String::as_str).collect()).unwrap_or_default()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn create(run: &mut Run, action: &str) -> String {
    let mut ask_id = String::new();
    run.write(|state| ask_id = state.create_ask(new_ask(action, "Summarise the logs"), now()).unwrap().0.ask_id);
    ask_id
}

fn blocked_owner_notice(run: &mut Run, action: &str) -> (String, String) {
    let ask = create(run, action);
    let mut delivery = String::new();
    run.write(|state| {
        crate::asks::wake::record_owner_action(state, TARGET, "ownerStopped", None, now());
        delivery = state.deliveries.values().find(|d| d.ask_id == ask).unwrap().delivery_id.clone();
    });
    (ask, delivery)
}

#[test]
fn stale_blocked_notice_retires_on_resume_start_or_end_without_losing_results() {
    for action in ["online", "admitted", "running", "answered", "interrupted", "failed", "cancelled"] {
        let mut run = Run::new();
        let (ask, notice) = blocked_owner_notice(&mut run, action);
        assert_eq!(run.record(REQUESTER, &notice).surface, Surface::NextUp);
        run.write(|state| {
            // An owner hold must not keep an obsolete fact alive.
            state.deliveries.get_mut(&notice).unwrap().owner_hold = true;
            match action {
                "online" => crate::asks::wake::clear_owner_action(state, TARGET, now()),
                _ => { state.update_ask(&ask, now(), |ask| {
                    ask.execution = match action {
                        "admitted" => ExecutionState::Admitted,
                        "running" => ExecutionState::Running,
                        "answered" => ExecutionState::Answered,
                        "interrupted" => ExecutionState::Interrupted,
                        "failed" => ExecutionState::Failed,
                        _ => ExecutionState::Cancelled,
                    };
                }); }
            }
        });
        let record = run.record(REQUESTER, &notice);
        assert_eq!(label(&record), "discarded{superseded}", "{action}");
        assert_eq!(record.surface, Surface::None);
        run.write(|state| {
            let ask = state.asks[&ask].clone();
            crate::asks::delivery::record_answer_delivery(state, &ask, "ans-final", now());
            crate::asks::delivery::record_question_delivery(state, &ask, "question", now());
        });
        let pending = crate::asks::delivery::claim_for_wait(&mut run.state, REQUESTER, &[ask], "read", now());
        assert_eq!(pending.len(), 2, "only the answer and question remain");
        assert!(pending.iter().all(|d| d.delivery_id != notice));
    }
}

#[test]
fn stale_blocked_notice_preserves_frozen_and_read_history_but_retires_proven_refusal() {
    for outcome in ["frozen", "unknown", "read", "refused", "legacy"] {
        let mut run = Run::new();
        let (ask, notice) = blocked_owner_notice(&mut run, outcome);
        let mut event = String::new();
        run.write(|state| {
            if outcome == "legacy" {
                let mut encoded = serde_json::to_value(&state.deliveries[&notice]).unwrap();
                encoded.as_object_mut().unwrap().remove("unsubmitted");
                state.deliveries.insert(notice.clone(), serde_json::from_value(encoded).unwrap());
            } else {
                event = crate::asks::delivery::coalesce_into_event(state, REQUESTER, "frozen-message", now()).unwrap();
                if outcome == "read" { crate::asks::delivery::acknowledge_event(state, &event, "read-turn", now()); }
            }
        });
        let before = run.state.deliveries[&notice].clone();
        run.write(|state| {
            state.update_ask(&ask, now(), |ask| ask.execution = ExecutionState::Running);
            crate::asks::wake::clear_owner_action(state, TARGET, now());
        });
        assert_eq!(run.state.deliveries[&notice], before, "{outcome}: reserved/read/unproven source retained");
        if outcome == "refused" {
            run.write(|state| crate::asks::delivery::release_event(state, &event, now()));
            assert_eq!(label(&run.record(REQUESTER, &notice)), "discarded{superseded}");
            assert_eq!(run.record(REQUESTER, &notice).surface, Surface::None);
        } else if outcome == "unknown" {
            run.write(|state| { crate::asks::delivery::release_event_unknown(state, &event, now()); });
            run.write(|state| crate::asks::wake::clear_owner_action(state, TARGET, now()));
            let record = &run.state.deliveries[&notice];
            assert_eq!(record.state, DeliveryState::Pending);
            assert!(!record.unsubmitted);
            assert!(record.settled_by.is_none());
        } else if outcome == "read" {
            let record = run.record(REQUESTER, &notice);
            assert_eq!(record.state, InputState::InContext);
            assert_eq!(record.surface, Surface::Timeline);
        }
    }
}

#[test]
fn stale_blocked_notice_settlement_wins_over_delayed_waiting_projection() {
    let mut run = Run::new();
    let ask = create(&mut run, "late-projection");
    let old = run.commit(|state| {
        crate::asks::wake::record_owner_action(state, TARGET, "ownerStopped", None, now());
    });
    let notice = run.state.deliveries.values().find(|d| d.ask_id == ask).unwrap().delivery_id.clone();
    // The source retirement commits before the background ledger writer got
    // to the original creation job. Apply in source order, then replay old IO.
    let retired = run.commit(|state| { state.update_ask(&ask, now(), |ask| ask.execution = ExecutionState::Admitted); });
    run.apply(retired);
    run.apply(old);
    assert_eq!(label(&run.record(REQUESTER, &notice)), "discarded{superseded}");
    assert_eq!(run.record(REQUESTER, &notice).surface, Surface::None);
}

/// Created already in review (one store write, like a gated create).
fn create_in_review(run: &mut Run, action: &str) -> String {
    let mut ask_id = String::new();
    run.write(|state| {
        ask_id = state.create_ask(new_ask(action, "Summarise the logs"), now()).unwrap().0.ask_id;
        state.update_ask(&ask_id, now(), |ask| ask.execution = ExecutionState::PendingReview);
    });
    ask_id
}

fn set(run: &mut Run, ask_id: &str, change: impl FnOnce(&mut AskRecord)) {
    run.write(|state| {
        state.update_ask(ask_id, now(), change);
    });
}

fn attempt(state: AttemptState, turn: Option<&str>) -> AttemptRecord {
    let mut record = AttemptRecord::submitting(1, "cutex-ask-msg-1".into(), 1, now_text());
    record.state = state;
    record.turn_id = turn.map(str::to_string);
    record
}

fn now_text() -> String {
    super::super::now_rfc3339(now())
}

// ---------------------------------------------------------------------------
// Delegation request (target side)

#[test]
fn request_review_queue_admit_run_moves_once_from_next_up_to_the_timeline() {
    let mut run = Run::new();
    let ask = create_in_review(&mut run, "a");
    let record = run.record(TARGET, &ask);
    assert_eq!((record.kind, record.surface), (InputKind::DelegationRequest, Surface::NextUp));
    assert_eq!(record.sender.cutex_session_id.as_deref(), Some(REQUESTER));
    assert_eq!(record.ask_id.as_deref(), Some(ask.as_str()));
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Queued;
        ask.hold = Some(Hold::TargetBusy { position: Some(2) });
    });
    // The queue position moving is not a client-visible change.
    let before = run.transitions;
    set(&mut run, &ask, |ask| ask.hold = Some(Hold::TargetBusy { position: Some(1) }));
    assert_eq!(run.transitions, before);
    // Lease and attempt are written before the native submit: no change.
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.hold = None;
        ask.attempts.push(attempt(AttemptState::Submitting, None));
    });
    assert_eq!(run.record(TARGET, &ask).state, InputState::Waiting);
    // The receipt: native IO happened.
    set(&mut run, &ask, |ask| ask.attempts[0] = attempt(AttemptState::Admitted, Some("turn-1")));
    assert_eq!(run.record(TARGET, &ask).surface, Surface::NextUp);
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Running;
        ask.attempts[0].state = AttemptState::Running;
        ask.attempts[0].started_at = Some(now_text());
    });
    assert_eq!(
        run.path(&ask),
        ["waiting{reviewPending}", "waiting{targetBusy}", "sending", "inContext(turnStartInput)"]
    );
    let record = run.record(TARGET, &ask);
    assert_eq!((record.surface, record.turn_id.as_deref()), (Surface::Timeline, Some("turn-1")));
    // The answer and later attempts do not touch the settled record.
    let before = run.transitions;
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Answered;
        ask.attempts[0].state = AttemptState::Answered;
    });
    assert_eq!(run.transitions, before);
}

#[test]
fn queued_reason_names_the_hold_and_the_lease_holder() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    let cases: Vec<(Option<Hold>, Option<LeaseHolder>, &str)> = vec![
        (None, Some(LeaseHolder::Ask { ask_id: "other".into(), attempt: 1 }), "waiting{delegationLease}"),
        (Some(Hold::TargetBusy { position: None }), Some(LeaseHolder::Event { event_id: "aev-x".into() }), "waiting{eventLease}"),
        (Some(Hold::Paused { by: PausedBy::Owner }), None, "waiting{paused}"),
        (Some(Hold::Paused { by: PausedBy::PlanMode }), None, "waiting{nativeHeld}"),
        (Some(Hold::HeldByRuntime { native: Some(HOLD_RECONNECTING.into()) }), None, "waiting{runtimeDisconnected}"),
        (Some(Hold::Blocked { by: BlockedReason::RuntimeDown }), None, "waiting{agentOffline}"),
        (Some(Hold::Blocked { by: BlockedReason::Quota }), None, "waiting{autoRetry}"),
        (Some(Hold::WaitingForApproval), None, "waiting{reviewPending}"),
        (Some(Hold::TargetBusy { position: None }), None, "waiting{targetBusy}"),
    ];
    for (hold, holder, expected) in cases {
        run.write(|state| {
            match &holder {
                Some(holder) => {
                    state.leases.insert(TARGET.into(), AdmissionLease {
                        cutex_session_id: TARGET.into(), holder: holder.clone(), native_message_id: "m".into(),
                        runtime_generation: 1, turn_id: None, since: now_text(),
                    });
                }
                None => {
                    state.leases.remove(TARGET);
                }
            }
            state.update_ask(&ask, now(), |ask| {
                ask.execution = ExecutionState::Queued;
                ask.hold = hold.clone();
                // Force the hook (the lease alone does not touch the ask).
                ask.updated_at = format!("{}-{expected}", now_text());
            });
        });
        assert_eq!(run.path(&ask).last().copied(), Some(expected), "{hold:?} {holder:?}");
    }
    let record = run.record(TARGET, &ask);
    assert_eq!(record.surface, Surface::NextUp, "reason changes never move the row");
}

#[test]
fn request_refused_before_native_io_never_flaps() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    set(&mut run, &ask, |ask| ask.execution = ExecutionState::Queued);
    for _ in 0..3 {
        // Admitted (lease + attempt, no IO), then refused `authority_busy`.
        set(&mut run, &ask, |ask| {
            ask.execution = ExecutionState::Admitted;
            ask.attempts = vec![attempt(AttemptState::Submitting, None)];
        });
        set(&mut run, &ask, |ask| {
            ask.execution = ExecutionState::Queued;
            ask.attempts[0].state = AttemptState::Refused;
            ask.hold = Some(Hold::TargetBusy { position: Some(1) });
        });
    }
    assert_eq!(run.path(&ask), ["waiting{targetBusy}"], "RC5: no sending ↔ waiting");
    // After native IO (outcome unknown) a native refusal puts it back.
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.attempts = vec![attempt(AttemptState::Submitting, None)];
        ask.submit_retry = Some(SubmitRetry { failures: 1, not_before: now_text(), last_error: "rpc".into() });
    });
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Queued;
        ask.attempts[0].state = AttemptState::Refused;
        ask.submit_retry = None;
    });
    assert_eq!(run.path(&ask), ["waiting{targetBusy}", "sending", "waiting{targetBusy}"]);
}

#[test]
fn request_rejected_and_cancelled_leave_next_up() {
    let mut run = Run::new();
    let rejected = create_in_review(&mut run, "r");
    set(&mut run, &rejected, |ask| ask.execution = ExecutionState::Rejected);
    assert_eq!(run.path(&rejected), ["waiting{reviewPending}", "rejected{askRejected}"]);
    assert_eq!(run.record(TARGET, &rejected).surface, Surface::None);

    let cancelled = create(&mut run, "c");
    set(&mut run, &cancelled, |ask| ask.execution = ExecutionState::Queued);
    set(&mut run, &cancelled, |ask| {
        ask.execution = ExecutionState::Cancelled;
        ask.closed = Some(CloseOutcome::Stopped);
    });
    assert_eq!(run.path(&cancelled), ["waiting{targetBusy}", "discarded{askCancelled}"]);
    assert_eq!(run.record(TARGET, &cancelled).surface, Surface::None);
}

#[test]
fn request_outcome_unknown_disposal_closes_stopped_without_retry() {
    let mut run = Run::new();
    let ask = create(&mut run, "u");
    set(&mut run, &ask, |ask| ask.execution = ExecutionState::Queued);
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.attempts = vec![attempt(AttemptState::Submitting, None)];
        ask.submit_retry = Some(SubmitRetry { failures: 3, not_before: now_text(), last_error: "status".into() });
    });
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Interrupted;
        ask.attempts[0].state = AttemptState::Interrupted;
        ask.attempts[0].interrupt_reason = Some(InterruptReason::OutcomeUnknown);
    });
    assert_eq!(run.path(&ask), ["waiting{targetBusy}", "sending", "uncertain{outcomeUnknown}"]);
    let record = run.record(TARGET, &ask);
    assert!(il::dismissible(&record));
    assert_eq!(record.deadline, None, "uncertain never expires (decision 9)");

    // Owner decision 9: close the interrupted unknown-outcome request without
    // issuing native stop or inventing a retry.
    let (store, dir) = temp_store("ledger-dismiss");
    let state = run.state.clone();
    store.with_state(|stored| *stored = state).unwrap();
    dismiss(&store, &record, now()).unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(after.asks[&ask].closed,Some(CloseOutcome::Stopped));
    assert_eq!(after.asks[&ask].execution, ExecutionState::Interrupted, "no retry");
    assert_eq!(after.asks[&ask].attempts.len(), 1);
    assert!(after.commands.is_empty(), "no native stop");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn request_to_another_host_is_recorded_there() {
    let mut run = Run::new();
    let ask = create(&mut run, "remote");
    use crate::peers::model as peer;
    let mut remote = run.state.asks[&ask].clone();
    let request = peer::Create {
        requester: peer::Requester { cutex_session_id: REQUESTER.into(), name: None, caps: Default::default() },
        target: TARGET.into(), title: "x".into(), text: "x".into(),
        options: remote.options.clone(), hops: 0, request_sha256: String::new(),
    };
    remote.remote = Some(peer::Remote {
        peer_id: "peer".into(), label: "Peer".into(), target_name: "worker".into(),
        request, remote_state: None, late_answer: None,
        sync: peer::Sync {
            local_terminal: false, reconcile_at: None, interrupt_reason: None,
            state: "queued".into(), last_remote_revision: 0, pending_ops: vec![],
            last_error: None, next_attempt_at: None,
            last_received_at: now_text(), failures: 0, accepted: false,
        },
    });
    remote.execution = ExecutionState::Running;
    assert!(request_job(&run.state, &run.state, None, &remote).is_none());
}

// ---------------------------------------------------------------------------
// Delegation results (initiator side)

fn answered(run: &mut Run, action: &str) -> (String, String) {
    let ask = create(run, action);
    let mut delivery = None;
    run.write(|state| {
        let record = state.asks[&ask].clone();
        state.answers.insert(format!("ans-{action}"), AnswerRecord {
            answer_id: format!("ans-{action}"), ask_id: ask.clone(), attempt: 1, revision: 1,
            status: AnswerStatus::Completed, artifact_id: None, bytes: 0, metadata: Default::default(),
            created_at: now_text(), preview: Some("All logs summarised".into()),
        });
        delivery = crate::asks::delivery::record_answer_delivery(state, &record, &format!("ans-{action}"), now());
    });
    (ask, delivery.expect("delivery"))
}

fn coalesce(run: &mut Run, message: &str) -> String {
    let mut event = None;
    run.write(|state| event = crate::asks::delivery::coalesce_into_event(state, REQUESTER, message, now()));
    event.expect("event")
}

#[test]
fn result_coalesces_and_is_sent_without_flapping_while_the_host_is_busy() {
    let mut run = Run::new();
    let (_, first) = answered(&mut run, "a");
    let (_, second) = answered(&mut run, "b");
    let record = run.record(REQUESTER, &first);
    assert_eq!((record.kind, record.surface), (InputKind::DelegationResult, Surface::NextUp));
    assert_eq!(record.preview, "All logs summarised");
    // Frozen, then refused before native IO (authority busy) twice: no change.
    for message in ["m1", "m2"] {
        let event = coalesce(&mut run, message);
        run.write(|state| crate::asks::delivery::release_event(state, &event, now()));
    }
    let event = coalesce(&mut run, "m3");
    // The native submit started a turn.
    let jobs = submitted_jobs(&run.state, REQUESTER, &[first.clone(), second.clone()], &event, Some("turn-9"));
    run.apply(jobs);
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-9", now()));
    for id in [&first, &second] {
        assert_eq!(run.path(id), ["waiting{resultCoalescing}", "sending", "accepted", "inContext(turnStartInput)"], "{id}");
        let record = run.record(REQUESTER, id);
        assert_eq!(record.batch_id.as_deref(), Some(event.as_str()), "batchId follows the event, the id stays");
        assert_eq!((record.surface, record.turn_id.as_deref()), (Surface::Timeline, Some("turn-9")));
    }
}

#[test]
fn result_absent_after_native_io_returns_and_is_resent_under_the_same_id() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "a");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, None));
    run.apply(returned_jobs(&run.state, REQUESTER, &[id.clone()]));
    let returned_since = run.record(REQUESTER, &id).since;
    run.write(|state| crate::asks::delivery::release_event(state, &event, now()));
    let pending = run.record(REQUESTER, &id);
    assert_eq!(pending.reason.as_deref(), Some("returned"), "source release cannot decide the native outcome");
    assert_eq!(pending.details["ownerHold"], false, "proven pending input can be held again");
    assert_eq!(pending.since, returned_since);
    let next = coalesce(&mut run, "m2");
    let frozen = run.record(REQUESTER, &id);
    assert_eq!(frozen.reason.as_deref(), Some("returned"), "freezing only removes hold actions, not the scheduler's reason");
    assert!(frozen.details.get("ownerHold").is_none());
    assert_eq!(frozen.since, returned_since);
    assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "sending", "waiting{returned}"]);
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &next, Some("turn-2")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &next, "turn-2", now()));
    assert_eq!(
        run.path(&id),
        ["waiting{resultCoalescing}", "sending", "waiting{returned}", "sending", "accepted", "inContext(turnStartInput)"]
    );
    assert_eq!(run.record(REQUESTER, &id).batch_id.as_deref(), Some(next.as_str()));
}

#[test]
fn owner_unsent_release_preserves_returned_in_either_writer_order_and_explicit_holds_win() {
    for source_first in [false, true] {
        let mut run = Run::new();
        let (_, id) = answered(&mut run, "owner-unsent");
        let event = coalesce(&mut run, "owner-note");
        run.write(|state| state.deliveries.get_mut(&id).unwrap().owner_input_id = Some("owner-input".into()));
        run.apply(submitted_jobs_in(&run.state, REQUESTER, &[id.clone()], &event,
            Some("owner-note"), None, false));
        let released = run.commit(|state| crate::asks::delivery::release_event(state, &event, now()));
        assert!(run.state.deliveries[&id].owner_input_id.is_none());
        if source_first {
            run.apply(released.clone());
            assert_eq!(run.record(REQUESTER, &id).state, InputState::Sending,
                "source release alone never guesses native refusal");
        }
        // Models CLI release_unsent: after BOTH source releases are proven,
        // its same-lane child transition restores the captured unheld action
        // metadata, including when the source job ran first during Sending.
        let mut returned = returned_jobs(&run.state, REQUESTER, &[id.clone()]);
        for job in &mut returned {
            for step in &mut job.steps {
                for next in &mut step.alts {
                    if next.state == InputState::Waiting {
                        next.details = Some(json!({"ownerHold": false}));
                    }
                }
            }
        }
        run.apply(returned);
        let returned_since = run.record(REQUESTER, &id).since;
        if !source_first {
            run.apply(released.clone());
            assert_eq!(run.record(REQUESTER, &id).details["ownerHold"], false);
        }
        let returned = run.record(REQUESTER, &id);
        assert_eq!(returned.reason.as_deref(), Some("returned"));
        assert_eq!(returned.details["ownerHold"], false, "caller restores hold actions in both writer orders");
        assert_eq!(returned.since, returned_since);
        assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "sending", "waiting{returned}"]);
        // An explicit newer source hold may change the waiting reason. A
        // delayed old release job must neither clear it nor restore coalescing.
        run.write(|state| state.deliveries.get_mut(&id).unwrap().owner_hold = true);
        run.apply(released.clone());
        let held = run.record(REQUESTER, &id);
        assert_eq!(held.reason.as_deref(), Some("ownerPause"));
        assert_eq!(held.details["ownerHold"], true);
        run.write(|state| state.deliveries.get_mut(&id).unwrap().owner_hold = false);
        run.apply(released);
        let unheld = run.record(REQUESTER, &id);
        assert_eq!(unheld.reason.as_deref(), Some("resultCoalescing"));
        assert_eq!(unheld.details["ownerHold"], false);
    }
}

#[test]
fn result_unprovable_is_uncertain_and_accepted_never_returns_to_waiting() {
    let mut run = Run::new();
    let (_, unknown) = answered(&mut run, "u");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[unknown.clone()], &event, None));
    run.apply(unknown_jobs(&run.state, REQUESTER, &[unknown.clone()]));
    assert_eq!(run.path(&unknown), ["waiting{resultCoalescing}", "sending", "uncertain{outcomeUnknown}"]);
    // Made passive by the store afterwards: the uncertain record stays.
    run.write(|state| {
        let delivery = state.deliveries.get_mut(&unknown).unwrap();
        delivery.state = DeliveryState::Pending;
        delivery.passive = true;
        delivery.event_id = None;
    });
    assert_eq!(run.record(REQUESTER, &unknown).state, InputState::Uncertain);

    let (_, accepted) = answered(&mut run, "a");
    let event = coalesce(&mut run, "m2");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[accepted.clone()], &event, Some("turn-1")));
    run.apply(returned_jobs(&run.state, REQUESTER, &[accepted.clone()]));
    assert_eq!(run.path(&accepted), ["waiting{resultCoalescing}", "sending", "accepted", "uncertain{runtimeLost}"]);
    // A late receipt still proves it (the only upgrade of uncertain).
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-1", now()));
    assert_eq!(run.path(&accepted).last().copied(), Some("inContext(turnStartInput)"));
}

#[test]
fn abandoned_carry_drift_preserves_prior_reason_and_excludes_active_reservations() {
    for shape in ["abandoned","no-owner","has-event","normal-reserved","lease-released"] {
        let mut run=Run::new();
        let (_,id)=answered(&mut run,shape);
        let event=coalesce(&mut run,"native-carry");
        run.write(|state|state.deliveries.get_mut(&id).unwrap().owner_input_id=Some("owner".into()));
        run.apply(submitted_jobs_in(&run.state,REQUESTER,&[id.clone()],&event,Some("native-carry"),Some("turn"),false));
        if shape=="lease-released" {
            run.apply(release_jobs(REQUESTER,vec![(id.clone(),false,delivery_input(&run.state,&run.state.deliveries[&id]))],
                Evidence::HistoryRead,None,None));
        }
        let original=run.record(REQUESTER,&id);
        let _lost_writer=run.commit(|state| {
            let delivery=state.deliveries.get_mut(&id).unwrap();
            if shape!="normal-reserved" { delivery.state=DeliveryState::Pending; delivery.passive=true; }
            if shape!="has-event" && shape!="normal-reserved" { delivery.event_id=None; }
            if shape=="no-owner" { delivery.owner_input_id=None; }
        });
        let job=drift_job(&run.state,&original);
        if let Some(job)=job {
            run.apply(vec![job.clone()]);
            let settled=run.record(REQUESTER,&id);
            assert!(apply_job(&run.ledger,&job).is_empty());
            if shape=="abandoned" {
                assert_eq!(settled.state,InputState::Uncertain);
                assert_eq!(settled.reason.as_deref(),Some("outcomeUnknown"));
            } else { assert_eq!(settled,original,"{shape}"); }
        } else { assert_ne!(shape,"abandoned"); }
    }
}

#[test]
fn delayed_abandoned_carry_projection_cannot_replace_a_new_source_attempt() {
    let mut run=Run::new();
    let (_,id)=answered(&mut run,"carry-source-version");
    let event=coalesce(&mut run,"native-old");
    run.write(|state|state.deliveries.get_mut(&id).unwrap().owner_input_id=Some("owner-old".into()));
    run.apply(submitted_jobs_in(&run.state,REQUESTER,&[id.clone()],&event,Some("native-old"),Some("old-turn"),false));
    let stale=run.commit(|state| {
        let delivery=state.deliveries.get_mut(&id).unwrap();
        delivery.state=DeliveryState::Pending;delivery.passive=true;delivery.event_id=None;
    });
    // Explicit retry already wrote its waiting row before the source mutation.
    run.ledger.transition(REQUESTER,&id,Expect::ANY,Next::uncertain("outcomeUnknown")).unwrap();
    run.ledger.transition(REQUESTER,&id,Expect::ANY,Next::waiting("ownerRetry")).unwrap();
    run.write(|state| { retry_result(state,REQUESTER,&id,now()).unwrap(); });
    let fresh=coalesce(&mut run,"native-new");
    run.apply(submitted_jobs_in(&run.state,REQUESTER,&[id.clone()],&fresh,Some("native-new"),Some("new-turn"),false));
    let record=run.record(REQUESTER,&id);
    run.apply(stale);
    assert_eq!(run.record(REQUESTER,&id),record);
    assert_eq!(record.state,InputState::Accepted);
    assert_eq!(record.batch_id.as_deref(),Some(fresh.as_str()));
}

#[test]
fn result_claimed_by_wait_is_in_context_with_no_surface() {
    let mut run = Run::new();
    let (ask, id) = answered(&mut run, "w");
    run.write(|state| {
        crate::asks::delivery::claim_for_wait(state, REQUESTER, &[ask.clone()], "wait-1", now());
    });
    assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "inContext(claimedByWait)"]);
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::None);
}

#[test]
fn result_read_or_closed_by_the_requester() {
    let mut run = Run::new();
    let (read_ask, read) = answered(&mut run, "r");
    run.write(|state| {
        crate::asks::delivery::settle_for_ask(state, REQUESTER, &read_ask, None, "read", now());
    });
    assert_eq!(run.path(&read), ["waiting{resultCoalescing}", "inContext(readByTool)"]);
    assert_eq!(run.record(REQUESTER, &read).surface, Surface::None);
    let (closed_ask, closed) = answered(&mut run, "c");
    run.write(|state| {
        crate::asks::delivery::settle_for_ask(state, REQUESTER, &closed_ask, None, "closed", now());
    });
    assert_eq!(run.path(&closed), ["waiting{resultCoalescing}", "discarded{settlementClosed}"]);
}

#[test]
fn result_creation_reason_follows_the_recipient_and_passive_creates_no_record() {
    let mut run = Run::new();
    run.write(|state| {
        state.presence.insert(REQUESTER.into(), crate::asks::store::Presence { online: true, busy: true, ..Default::default() });
    });
    let (_, busy) = answered(&mut run, "busy");
    assert_eq!(run.path(&busy), ["waiting{afterTurn}"]);
    let ask = create(&mut run, "passive");
    let mut id = None;
    run.write(|state| {
        let mut record = state.asks[&ask].clone();
        record.options.notify = Notify::Passive;
        id = crate::asks::delivery::record_state_delivery(state, &record, "interrupted", now());
    });
    let id = id.expect("state delivery");
    // r274.2: a passive item is not certain to reach the agent: no record.
    assert!(run.state.deliveries[&id].passive);
    assert!(no_record(&run, &id));
}

fn no_record(run: &Run, id: &str) -> bool {
    run.ledger.get(REQUESTER, &[id.to_string()]).unwrap().records.is_empty()
}

#[test]
fn result_dismissed_by_the_owner_is_settled_and_never_sent() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "d");
    let record = run.record(REQUESTER, &id);
    assert!(il::dismissible(&record));
    let (store, dir) = temp_store("ledger-dismiss-result");
    let state = run.state.clone();
    store.with_state(|stored| *stored = state).unwrap();
    dismiss(&store, &record, now()).unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(after.deliveries[&id].state, DeliveryState::Acknowledged);
    assert_eq!(after.deliveries[&id].settled_by.as_deref(), Some(DISMISSED));
    assert!(crate::asks::delivery::requesters_with_pending(&after).is_empty());
    // The durable source repairs the ledger even if the route did not finish.
    let before = run.state.clone();
    let changes = crate::asks::store::ask_state_changes(&before, &after).unwrap();
    run.apply(store_jobs(&before, &after, &changes));
    assert_eq!(run.record(REQUESTER, &id).state, InputState::Discarded);
    assert_eq!(run.record(REQUESTER, &id).reason.as_deref(), Some("ownerDiscarded"));
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Leases (§2.6 view, §2.14 release)

#[test]
fn lease_view_and_release_need_a_stale_unchanged_lease() {
    let mut run = Run::new();
    let (_, read) = answered(&mut run, "a");
    let (_, unread) = answered(&mut run, "b");
    let event = coalesce(&mut run, "m1");
    run.write(|state| {
        state.deliveries.get_mut(&read).unwrap().state = DeliveryState::Acknowledged;
        state.leases.insert(REQUESTER.into(), AdmissionLease {
            cutex_session_id: REQUESTER.into(), holder: LeaseHolder::Event { event_id: event.clone() },
            native_message_id: "m1".into(), runtime_generation: 7, turn_id: Some("turn-1".into()), since: now_text(),
        });
    });
    let (store, dir) = temp_store("ledger-lease");
    let state = run.state.clone();
    store.with_state(|stored| *stored = state).unwrap();
    let view = lease_view(&store, REQUESTER).unwrap().expect("lease");
    assert_eq!(view["holder"], "event");
    assert_eq!(view["turnId"], "turn-1");
    assert_eq!(view["runtimeGeneration"], 7);
    assert_eq!(view["reconcile"]["state"], "waitingTurn");
    let mut ids: Vec<String> = serde_json::from_value(view["inputIds"].clone()).unwrap();
    ids.sort();
    let mut expected = vec![read.clone(), unread.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(lease_view(&store, "nobody").unwrap().is_none());

    let refused = lease_release(&store, REQUESTER, &view, now()).unwrap_err();
    assert_eq!(refused.code, "lease_not_stale");
    let lease = store.snapshot().unwrap().leases[REQUESTER].clone();
    crate::asks::lease_reconcile::force_stale(store.dir(), &lease);
    let stale = lease_view(&store, REQUESTER).unwrap().unwrap();
    assert_eq!(stale["reconcile"]["state"], "stale");
    let mut other = stale.clone();
    other["turnId"] = json!("turn-2");
    assert_eq!(lease_release(&store, REQUESTER, &other, now()).unwrap_err().code, "lease_changed");

    let released = lease_release(&store, REQUESTER, &stale, now()).unwrap();
    assert_eq!(released["released"], true);
    assert_eq!(released["lease"]["holder"], "event");
    let after = store.snapshot().unwrap();
    assert!(!after.leases.contains_key(REQUESTER));
    assert_eq!(after.deliveries[&read].state, DeliveryState::Acknowledged);
    let unread_after = &after.deliveries[&unread];
    assert!(unread_after.state == DeliveryState::Pending && unread_after.passive, "never sent again");
    assert!(crate::asks::delivery::requesters_with_pending(&after).is_empty());
    assert_eq!(lease_release(&store, REQUESTER, &stale, now()).unwrap_err().code, "lease_changed");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn failed_owner_carry_discard_queued_event_releases_source_and_slot() {
    use crate::asks::model::{AdmissionLease, LeaseHolder};
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "rover-queued-discard");
    let record = run.record(REQUESTER, &id);
    let (store, dir) = temp_store("rover-queued-discard");
    store.with_state(|state| {
        *state = run.state.clone();
        let event = crate::asks::delivery::coalesce_into_event(state, REQUESTER,
            "cutex-ask-owner-rover", now()).unwrap();
        state.next_step_leases.insert(REQUESTER.into(), AdmissionLease {
            cutex_session_id: REQUESTER.into(), holder: LeaseHolder::Event { event_id:event },
            native_message_id:"cutex-ask-owner-rover".into(), turn_id:None,
            runtime_generation:1, since:crate::asks::now_rfc3339(now()),
        });
    }).unwrap();
    dismiss(&store, &record, now()).unwrap();
    let after = store.snapshot().unwrap();
    assert_eq!(after.deliveries[&id].state, DeliveryState::Acknowledged);
    assert_eq!(after.deliveries[&id].settled_by.as_deref(), Some(DISMISSED));
    assert!(after.events.is_empty());
    let changes = crate::asks::store::ask_state_changes(&run.state, &after).unwrap();
    run.apply(store_jobs(&run.state.clone(), &after, &changes));
    assert_eq!(run.record(REQUESTER, &id).state, InputState::Discarded);
    store.with_state(|state| crate::asks::delivery::acknowledge_event(state,
        "old-removed-event", "late-turn", now())).unwrap();
    assert_eq!(store.snapshot().unwrap().deliveries[&id], after.deliveries[&id]);
    assert!(!after.next_step_leases.contains_key(REQUESTER));
    assert_eq!(crate::asks::returns::results_admissible(&after, REQUESTER), Some(None));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn failed_owner_carry_discard_mixed_batch_marks_unknown_and_fences_late_receipts() {
    use crate::asks::model::{AdmissionLease, LeaseHolder};
    let mut run = Run::new();
    let (_, dismissed) = answered(&mut run, "mixed-discard");
    let (_, sibling) = answered(&mut run, "mixed-sibling");
    let record = run.record(REQUESTER, &dismissed);
    let (store, dir) = temp_store("mixed-discard");
    let event_id = store.with_state(|state| {
        *state = run.state.clone();
        let event = crate::asks::delivery::coalesce_into_event(state, REQUESTER, "cutex-ask-owner-mixed", now()).unwrap();
        state.next_step_leases.insert(REQUESTER.into(), AdmissionLease {
            cutex_session_id: REQUESTER.into(), holder: LeaseHolder::Event {event_id:event.clone()},
            native_message_id:"cutex-ask-owner-mixed".into(), turn_id:None, runtime_generation:1,
            since:crate::asks::now_rfc3339(now()),
        });
        event
    }).unwrap();
    let artifact = store.put_artifact(b"original frozen batch", "text/plain", &[], None, now()).unwrap();
    store.with_state(|state| {
        let event = state.events.get_mut(&event_id).unwrap();
        event.event_artifact = Some(artifact);
        event.view = Some(serde_json::json!({"schema":"cutex.delegation-results.v1","data":{
            "items":[{"kind":"answer","askId":"mixed-a"},{"kind":"answer","askId":"mixed-b"}]}}));
    }).unwrap();
    let before = store.snapshot().unwrap();
    let original = crate::asks::returns::event_submission(&before,&before.events[&event_id],"original frozen batch".into());
    dismiss(&store, &record, now()).unwrap();
    let after = store.snapshot().unwrap();
    assert!(!after.events.contains_key(&event_id));
    assert_eq!(store.read_artifact(before.events[&event_id].event_artifact.as_deref().unwrap(),None).unwrap().unwrap().1,
        b"original frozen batch");
    assert_eq!(original.message_id,"cutex-ask-owner-mixed");
    assert_eq!(after.deliveries[&sibling].state,DeliveryState::Pending);
    assert!(after.deliveries[&sibling].passive,"unknown sibling must never be resent");
    assert_eq!(after.deliveries[&sibling].settled_by.as_deref(),Some(crate::asks::RESULTS_OUTCOME_UNKNOWN));
    assert!(!after.next_step_leases.contains_key(REQUESTER));
    let changes = crate::asks::store::ask_state_changes(&before, &after).unwrap();
    run.apply(store_jobs(&before, &after, &changes));
    assert_eq!(run.record(REQUESTER, &dismissed).state, InputState::Discarded);
    let replacement = AdmissionLease {
        cutex_session_id:REQUESTER.into(), holder:LeaseHolder::Event {event_id:"new-event".into()},
        native_message_id:"cutex-ask-owner-new-message".into(), turn_id:None, runtime_generation:2,
        since:crate::asks::now_rfc3339(now()),
    };
    store.with_state(|state| {
        let mut new_event = before.events[&event_id].clone();
        new_event.event_id = "new-event".into();
        new_event.native_message_id = replacement.native_message_id.clone();
        new_event.delivery_ids.clear();
        state.events.insert(new_event.event_id.clone(), new_event);
        state.next_step_leases.insert(REQUESTER.into(),replacement.clone());
        crate::asks::delivery::acknowledge_event(state,&event_id,"late-old-turn",now());
        crate::asks::returns::remove_event_lease(state,REQUESTER,&event_id);
    }).unwrap();
    let late = store.snapshot().unwrap();
    assert_eq!(late.deliveries[&dismissed], after.deliveries[&dismissed]);
    assert_eq!(late.deliveries[&sibling],after.deliveries[&sibling]);
    assert_eq!(late.next_step_leases[REQUESTER], replacement);
    let changes = crate::asks::store::ask_state_changes(&after, &late).unwrap();
    run.apply(store_jobs(&after, &late, &changes));
    assert_eq!(run.record(REQUESTER, &dismissed).state, InputState::Discarded);
    assert_eq!(run.record(REQUESTER, &sibling).state, InputState::Uncertain);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn released_lease_records_acked_in_context_others_lease_released() {
    let mut run = Run::new();
    let (_, read) = answered(&mut run, "a");
    let (_, unread) = answered(&mut run, "b");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[read.clone(), unread.clone()], &event, Some("turn-1")));
    let input = |id: &str| delivery_input(&run.state, &run.state.deliveries[id]);
    let jobs = release_jobs(
        REQUESTER,
        vec![(read.clone(), true, input(&read)), (unread.clone(), false, input(&unread))],
        Evidence::TurnStartInput,
        Some("turn-1".into()),
        None,
    );
    run.apply(jobs);
    assert_eq!(run.path(&read).last().copied(), Some("inContext(turnStartInput)"));
    assert_eq!(run.path(&unread).last().copied(), Some("uncertain{leaseReleased}"));
    assert_eq!(run.record(REQUESTER, &unread).surface, Surface::Timeline, "uncertain stays where it was");
}

#[test]
fn backfill_is_off_with_the_switch_off_and_jobs_skip_settled_records() {
    // Default test config: the switch is off, nothing is queued.
    if !il::enabled() {
        let (store, dir) = temp_store("ledger-backfill");
        assert_eq!(backfill_outcome_unknown(&store).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
    // A guarded step on a settled record is skipped, not an error.
    let mut run = Run::new();
    let (ask, id) = answered(&mut run, "w");
    run.write(|state| {
        crate::asks::delivery::claim_for_wait(state, REQUESTER, &[ask.clone()], "wait", now());
    });
    let before = run.transitions;
    run.apply(returned_jobs(&run.state, REQUESTER, &[id.clone()]));
    run.apply(unknown_jobs(&run.state, REQUESTER, &[id]));
    assert_eq!(run.transitions, before);
}

#[test]
fn results_accepted_by_an_interrupted_turn_are_uncertain_until_a_receipt() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "n");
    let event = coalesce(&mut run, "cutex-ask-soon-1");
    // Steered into the running turn T (next step): accepted{T}.
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, Some("turn-T")));
    // Another turn ending does not touch it.
    run.apply(turn_ended_jobs(&run.state, REQUESTER, "turn-other", "turnInterrupted"));
    assert_eq!(run.path(&id).last().copied(), Some("accepted"));
    run.apply(turn_ended_jobs(&run.state, REQUESTER, "turn-T", "turnInterrupted"));
    assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "sending", "accepted", "uncertain{turnInterrupted}"]);
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::Timeline, "never back to nextUp");
    // The one status check later shows it persisted.
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-T", now()));
    assert_eq!(run.path(&id).last().copied(), Some("inContext(externalInputPersisted)"));
}

/// r274.2 §9.1: a Claude `soon` result sent without a turn (`sending`) moves
/// to the timeline as `accepted` when the adapter shows its event item; an
/// interrupted turn then makes it `uncertain`, a receipt `inContext`.
#[test]
fn a_shown_result_event_item_moves_its_deliveries_to_the_timeline() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "s");
    let event = coalesce(&mut run, "cutex-ask-soon-2");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, None));
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::NextUp);
    // Another session's or another item's row moves nothing.
    assert!(shown_jobs(&run.state, TARGET, "cutex-ask-soon-2", "turn-T").is_empty());
    assert!(shown_jobs(&run.state, REQUESTER, "cutex-ask-other", "turn-T").is_empty());
    run.apply(shown_jobs(&run.state, REQUESTER, "cutex-ask-soon-2", "turn-T"));
    let shown = run.record(REQUESTER, &id);
    assert_eq!((shown.state, shown.surface, shown.turn_id.as_deref(), shown.timeline_item_id.as_deref()),
        (InputState::Accepted, Surface::Timeline, Some("turn-T"), Some("cutex-ask-soon-2")));
    assert_eq!(shown.details[il::READ_SIGNAL], "nativeItem");
    // item/completed after item/started: nothing more.
    let before = run.transitions;
    run.apply(shown_jobs(&run.state, REQUESTER, "cutex-ask-soon-2", "turn-T"));
    assert_eq!(run.transitions, before);
    run.apply(turn_ended_jobs(&run.state, REQUESTER, "turn-T", "turnInterrupted"));
    assert_eq!(run.path(&id).last().copied(), Some("uncertain{turnInterrupted}"));
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::Timeline);
}

#[test]
fn result_retry_resends_the_same_delivery_in_a_new_event() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "r");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, Some("turn-1")));
    run.apply(turn_ended_jobs(&run.state, REQUESTER, "turn-1", "turnInterrupted"));
    let uncertain = run.record(REQUESTER, &id);
    assert_eq!((uncertain.state, uncertain.details["mayArriveTwice"].clone()), (InputState::Uncertain, json!(true)));
    // Only the recipient's own, unread items can go again.
    assert!(result_redeliverable(&run.state, REQUESTER, &id).is_ok());
    assert_eq!(result_redeliverable(&run.state, TARGET, &id).unwrap_err().0, "delivery_not_found");
    assert_eq!(result_redeliverable(&run.state, REQUESTER, "dlv-nope").unwrap_err().0, "delivery_not_found");
    // The owner's retry: the ledger first (cutex/input/retry), then the store.
    let retried = run.ledger.transition(REQUESTER, &id, Expect::rev(uncertain.rev), Next::waiting("ownerRetry")).unwrap();
    assert_eq!(retried.record.attempt, 2);
    // Include the explicit ledger retry in the observed state path before
    // checking that the following source job changes metadata only.
    run.paths.entry(id.clone()).or_default().push(label(&retried.record));
    let path_before = run.paths[&id].clone();
    let before = run.transitions;
    run.write(|state| retry_result(state, REQUESTER, &id, now()).unwrap());
    assert_eq!(run.transitions, before + 1, "the source write restores hold actions in exactly one metadata revision");
    let restored = run.record(REQUESTER, &id);
    assert_eq!(restored.rev, retried.record.rev + 1);
    assert_eq!((restored.state, restored.reason.as_deref()), (InputState::Waiting, Some("ownerRetry")));
    assert_eq!((restored.attempt, &restored.since, restored.surface),
        (retried.record.attempt, &retried.record.since, retried.record.surface));
    assert_eq!(restored.details["ownerHold"], false);
    assert!(il::record_actions(&restored, None).contains(&il::Action::Hold));
    assert_eq!(run.paths[&id], path_before, "restoring hold actions never changes the state path");
    let delivery = &run.state.deliveries[&id];
    assert_eq!((delivery.state.clone(), delivery.event_id.clone(), delivery.passive), (DeliveryState::Pending, None, false));
    assert!(!run.state.events.contains_key(&event), "empty old event retired");
    // The scheduler sends it again under the same deliveryId.
    let next = coalesce(&mut run, "m2");
    assert_ne!(next, event);
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &next, Some("turn-2")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &next, "turn-2", now()));
    let done = run.record(REQUESTER, &id);
    assert_eq!((done.state, done.attempt, done.batch_id.as_deref()), (InputState::InContext, 2, Some(next.as_str())));
    // Read: no further retry.
    assert_eq!(result_redeliverable(&run.state, REQUESTER, &id).unwrap_err().0, "delivery_not_redeliverable");
    let mut state = run.state.clone();
    assert!(retry_result(&mut state, REQUESTER, &id, now()).is_err());
}

// ---------------------------------------------------------------------------
// Several processes write the AskStore (management, Agent Bus); each applies
// its own jobs on its own ledger connection, with no order between them.

/// A second process's ledger on the same file.
fn other_process(run: &Run) -> InputLedger {
    InputLedger::open(run.root.join(il::DATABASE_FILE))
}

#[test]
fn an_older_store_state_applied_late_by_another_process_never_moves_the_record_back() {
    let mut run = Run::new();
    // Agent Bus: the delegation is created in review; its writer is slow.
    let mut ask_id = String::new();
    let review = run.commit(|state| {
        ask_id = state.create_ask(new_ask("a", "Summarise the logs"), now()).unwrap().0.ask_id;
        state.update_ask(&ask_id, now(), |ask| ask.execution = ExecutionState::PendingReview);
    });
    // Management: the owner approves; its writer applies first.
    let approved = run.commit(|state| {
        state.update_ask(&ask_id, now(), |ask| {
            ask.execution = ExecutionState::Queued;
            ask.hold = Some(Hold::TargetBusy { position: Some(1) });
        });
    });
    let management = other_process(&run);
    let applied: Vec<_> = approved.iter().flat_map(|job| apply_job(&management, job)).collect();
    assert_eq!(applied.len(), 1);
    // The bus process applies its older review state now: skipped.
    let bus = &run.ledger;
    let late: Vec<_> = review.iter().flat_map(|job| apply_job(bus, job)).collect();
    assert!(late.is_empty(), "no backward transition published: {late:?}");
    let record = run.record(TARGET, &ask_id);
    assert_eq!((record.state, record.reason.as_deref(), record.rev), (InputState::Waiting, Some("targetBusy"), 1));
    assert_eq!(record.details[SOURCE_VERSION], json!(run.state.revision));
}

#[test]
fn a_late_queued_state_never_takes_a_sent_request_back_to_waiting() {
    let mut run = Run::new();
    let ask = create_in_review(&mut run, "a");
    // sending -> waiting is a legal edge (a native refusal), so only the
    // source version tells a late queued state from a real refusal.
    let queued = run.commit(|state| {
        state.update_ask(&ask, now(), |ask| {
            ask.execution = ExecutionState::Queued;
            ask.hold = Some(Hold::TargetBusy { position: None });
        });
    });
    let admitted = run.commit(|state| {
        state.update_ask(&ask, now(), |ask| {
            ask.execution = ExecutionState::Admitted;
            ask.hold = None;
            ask.attempts.push(attempt(AttemptState::Admitted, Some("turn-1")));
        });
    });
    let management = other_process(&run);
    for job in &admitted {
        apply_job(&management, job);
    }
    assert_eq!(run.record(TARGET, &ask).state, InputState::Sending);
    let late: Vec<_> = queued.iter().flat_map(|job| apply_job(&run.ledger, job)).collect();
    assert!(late.is_empty(), "no backward transition published: {late:?}");
    assert_eq!(run.record(TARGET, &ask).state, InputState::Sending);
    // A real refusal later (a newer store state) still puts it back.
    let refused = run.commit(|state| {
        state.update_ask(&ask, now(), |ask| {
            ask.execution = ExecutionState::Queued;
            ask.hold = Some(Hold::TargetBusy { position: None });
            ask.attempts[0].state = AttemptState::Refused;
        });
    });
    let back: Vec<_> = refused.iter().flat_map(|job| apply_job(&run.ledger, job)).collect();
    assert_eq!(back.len(), 1);
    assert_eq!(run.record(TARGET, &ask).state, InputState::Waiting);
    // The record carries the newest store revision applied to it.
    let record = run.record(TARGET, &ask);
    assert_eq!(record.details[SOURCE_VERSION], json!(run.state.revision));
}

#[test]
fn two_writers_racing_on_one_record_end_at_the_newest_store_state() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    // Alternating holds: every write changes the record's waiting reason.
    let holds = [Hold::WaitingForApproval, Hold::Paused { by: PausedBy::PlanMode }];
    let mut jobs = Vec::new();
    for round in 0..40 {
        let hold = holds[round % 2].clone();
        jobs.push(run.commit(|state| {
            state.update_ask(&ask, now(), |ask| {
                ask.execution = ExecutionState::Queued;
                ask.hold = Some(hold);
            });
        }));
    }
    let newest = jobs.last().unwrap()[0].clone();
    // Process A applies the even writes newest first, process B the odd ones
    // oldest first, at the same time.
    let (even, odd): (Vec<_>, Vec<_>) = jobs.into_iter().enumerate().partition(|(index, _)| index % 2 == 0);
    let path = run.root.join(il::DATABASE_FILE);
    let spawn = |mut jobs: Vec<(usize, Vec<Job>)>, reverse: bool| {
        let path = path.clone();
        if reverse {
            jobs.reverse();
        }
        std::thread::spawn(move || {
            let ledger = InputLedger::open(path);
            for (_, batch) in &jobs {
                for job in batch {
                    apply_job(&ledger, job);
                }
            }
        })
    };
    let a = spawn(even, true);
    let b = spawn(odd, false);
    a.join().unwrap();
    b.join().unwrap();
    let record = run.record(TARGET, &ask);
    assert_eq!(record.details[SOURCE_VERSION], json!(newest.source_version.unwrap()));
    assert_eq!(record.reason.as_deref(), Some("nativeHeld"), "the last write held it for plan mode");
}

// ---------------------------------------------------------------------------
// r274.1: the native timeline item (054 §3.2 `timelineItemId`). A results
// event is ONE native item whose id is the event's native message id; every
// record it carried binds that id.

fn item(run: &Run, id: &str) -> Option<String> {
    run.record(REQUESTER, id).timeline_item_id
}

/// A state item (interrupted) of a new delegation, sent like a result.
fn status_delivery(run: &mut Run, action: &str) -> String {
    let ask = create(run, action);
    let mut id = None;
    run.write(|state| {
        let record = state.asks[&ask].clone();
        id = crate::asks::delivery::record_state_delivery(state, &record, "interrupted", now());
        // Sent with the results (not a passive note).
        if let Some(delivery) = id.as_ref().and_then(|id| state.deliveries.get_mut(id)) {
            delivery.passive = false;
        }
    });
    id.expect("state delivery")
}

#[test]
fn exclusive_result_turn_binds_every_coalesced_record_to_the_event_item() {
    let mut run = Run::new();
    let (_, first) = answered(&mut run, "a");
    let (_, second) = answered(&mut run, "b");
    // A status item rides in the same event (same writer).
    let status = status_delivery(&mut run, "s");
    let event = coalesce(&mut run, "cutex-ask-event-1");
    assert_eq!(item(&run, &first), None, "not in a native item before the submit");
    let ids = [first.clone(), second.clone(), status.clone()];
    run.apply(submitted_jobs_with(&run.state, REQUESTER, &ids, &event, Some("cutex-ask-event-1"), Some("turn-9")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-9", now()));
    for id in &ids {
        let record = run.record(REQUESTER, id);
        assert_eq!(label(&record), "inContext(turnStartInput)", "{id}");
        assert_eq!(record.surface, Surface::Timeline);
        assert_eq!(record.timeline_item_id.as_deref(), Some("cutex-ask-event-1"), "{id}");
        assert_eq!(record.batch_id.as_deref(), Some(event.as_str()));
    }
    assert_eq!(run.record(REQUESTER, &status).kind, InputKind::DelegationStatus);
}

#[test]
fn question_rides_the_same_writer_and_binds_the_event_item() {
    let mut run = Run::new();
    let ask = create(&mut run, "q");
    let mut id = None;
    run.write(|state| {
        let record = state.asks[&ask].clone();
        id = crate::asks::delivery::record_question_delivery(state, &record, "q-1", now());
    });
    let id = id.expect("question delivery");
    let event = coalesce(&mut run, "cutex-ask-event-q");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, Some("turn-q")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-q", now()));
    let record = run.record(REQUESTER, &id);
    assert_eq!((record.kind, record.state), (InputKind::DelegationQuestion, InputState::InContext));
    assert_eq!(record.timeline_item_id.as_deref(), Some("cutex-ask-event-q"));
}

#[test]
fn owner_ride_along_result_binds_the_owner_note_item_when_read() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "o");
    let event = coalesce(&mut run, "cutex-ask-owner-1");
    // stage_owner_results: sent beside the owner's turn/start, no turn yet.
    run.apply(submitted_jobs_with(&run.state, REQUESTER, &[id.clone()], &event, Some("cutex-ask-owner-1"), None));
    assert_eq!((run.record(REQUESTER, &id).state, item(&run, &id)), (InputState::Sending, None));
    // The reconcile finds it persisted in the owner's turn.
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "owner-turn", now()));
    let record = run.record(REQUESTER, &id);
    assert_eq!(label(&record), "inContext(externalInputPersisted)");
    assert_eq!((record.surface, record.turn_id.as_deref()), (Surface::Timeline, Some("owner-turn")));
    assert_eq!(record.timeline_item_id.as_deref(), Some("cutex-ask-owner-1"));
}

#[test]
fn next_step_result_is_bound_once_accepted_and_a_lost_one_binds_its_new_event() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "n");
    let old = coalesce(&mut run, "cutex-ask-soon-1");
    // Steered into the running turn: bound to its item, but only queued
    // there (r274.2 §9.4: accepted + turnId is not read evidence), so it
    // stays in nextUp until its native item or receipt.
    run.apply(submitted_jobs_in(&run.state, REQUESTER, &[id.clone()], &old, Some("cutex-ask-soon-1"), Some("turn-T"), false));
    let accepted = run.record(REQUESTER, &id);
    assert_eq!((accepted.state, accepted.surface), (InputState::Accepted, Surface::NextUp));
    assert_eq!(accepted.timeline_item_id.as_deref(), Some("cutex-ask-soon-1"));
    // The same fact of an exclusive (turn-start) submit is read evidence.
    let (_, started) = answered(&mut run, "x");
    let exclusive = coalesce(&mut run, "cutex-ask-event-x");
    run.apply(submitted_jobs_in(&run.state, REQUESTER, &[started.clone()], &exclusive, Some("cutex-ask-event-x"), Some("turn-X"), true));
    let record = run.record(REQUESTER, &started);
    assert_eq!((record.state, record.surface, record.turn_id.as_deref()), (InputState::Accepted, Surface::Timeline, Some("turn-X")));
    // The runtime was lost (reconnect, another generation): runtimeLost; the
    // event is put back and the result goes out in a new event.
    run.apply(returned_jobs(&run.state, REQUESTER, &[id.clone()]));
    assert_eq!(label(&run.record(REQUESTER, &id)), "uncertain{runtimeLost}");
    run.write(|state| {
        crate::asks::delivery::release_event_unknown(state, &old, now());
    });
    let new = coalesce(&mut run, "cutex-ask-soon-2");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &new, None));
    // The receipt of the new event proves it and binds ITS item.
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &new, "turn-U", now()));
    let done = run.record(REQUESTER, &id);
    assert_eq!(label(&done), "inContext(externalInputPersisted)");
    assert_eq!((done.timeline_item_id.as_deref(), done.batch_id.as_deref()), (Some("cutex-ask-soon-2"), Some(new.as_str())));
    // A late duplicate fact of the OLD event changes nothing.
    let before = run.record(REQUESTER, &id);
    run.apply(submitted_jobs_with(&run.state, REQUESTER, &[id.clone()], &old, Some("cutex-ask-soon-1"), Some("turn-T")));
    assert_eq!(run.record(REQUESTER, &id), before);
}

#[test]
fn a_stale_submit_fact_never_moves_or_rebinds_a_newer_attempt() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "s");
    let old = coalesce(&mut run, "cutex-ask-event-old");
    let stale = submitted_jobs(&run.state, REQUESTER, &[id.clone()], &old, Some("turn-old"));
    // The old event was refused before its fact ran; a new one is in flight.
    run.write(|state| crate::asks::delivery::release_event(state, &old, now()));
    let new = coalesce(&mut run, "cutex-ask-event-new");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &new, None));
    let sending = run.record(REQUESTER, &id);
    assert_eq!((sending.state, sending.batch_id.as_deref()), (InputState::Sending, Some(new.as_str())));
    run.apply(stale);
    assert_eq!(run.record(REQUESTER, &id), sending, "the old event's fact is skipped whole");
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &new, "turn-new", now()));
    assert_eq!(item(&run, &id).as_deref(), Some("cutex-ask-event-new"));
}

#[test]
fn lost_reply_returned_then_resent_binds_the_new_event_only() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "l");
    let first = coalesce(&mut run, "cutex-ask-event-1");
    // Outcome unknown (the reply was lost): sending, no item yet.
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &first, None));
    // Absent at native: back to waiting, attempt 2.
    run.apply(returned_jobs(&run.state, REQUESTER, &[id.clone()]));
    let returned = run.record(REQUESTER, &id);
    assert_eq!((label(&returned).as_str(), returned.attempt, returned.timeline_item_id.clone()), ("waiting{returned}", 2, None));
    run.write(|state| crate::asks::delivery::release_event(state, &first, now()));
    let second = coalesce(&mut run, "cutex-ask-event-2");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &second, Some("turn-2")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &second, "turn-2", now()));
    assert_eq!(item(&run, &id).as_deref(), Some("cutex-ask-event-2"));
}

#[test]
fn reconcile_and_rebuild_replays_bind_the_acknowledged_event_item() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "r");
    let event = coalesce(&mut run, "cutex-ask-event-r");
    // The store got the receipt (a reconcile / bootstrap acknowledge) while
    // this ledger never saw a write: the drift / rebuild replay from no record.
    let mut state = run.state.clone();
    crate::asks::delivery::acknowledge_event(&mut state, &event, "turn-r", now());
    let fresh = Run::new();
    let job = delivery_job(&state, &state, None, &state.deliveries[&id]).expect("replay job");
    let applied = apply_job(&fresh.ledger, &job);
    let record = &applied.last().expect("replayed").record;
    assert_eq!((record.state, record.surface), (InputState::InContext, Surface::Timeline));
    assert_eq!(record.timeline_item_id.as_deref(), Some("cutex-ask-event-r"));
    assert_eq!(record.batch_id.as_deref(), Some(event.as_str()));
    // The drift job of the same record changes nothing more.
    let probe = fresh.record(REQUESTER, &id);
    if let Some(job) = drift_job(&state, &probe) {
        assert!(apply_job(&fresh.ledger, &job).iter().all(|transition| !transition.changed));
    }
}

#[test]
fn released_event_lease_binds_its_acknowledged_records() {
    let mut run = Run::new();
    let (_, read) = answered(&mut run, "a");
    let (_, unread) = answered(&mut run, "b");
    let event = coalesce(&mut run, "cutex-ask-event-l");
    run.apply(submitted_jobs_with(&run.state, REQUESTER, &[read.clone(), unread.clone()], &event, None, Some("turn-1")));
    let input = |id: &str| delivery_input(&run.state, &run.state.deliveries[id]);
    let jobs = release_jobs(
        REQUESTER,
        vec![(read.clone(), true, input(&read)), (unread.clone(), false, input(&unread))],
        Evidence::TurnStartInput,
        Some("turn-1".into()),
        Some((event.as_str(), "cutex-ask-event-l")),
    );
    run.apply(jobs);
    assert_eq!(item(&run, &read).as_deref(), Some("cutex-ask-event-l"));
    assert_eq!(item(&run, &unread), None, "an unproven item is not in a native item");
}

#[test]
fn backfill_item_derives_only_from_the_acknowledged_event_of_the_record() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "b");
    let event = coalesce(&mut run, "cutex-ask-event-b");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, Some("turn-b")));
    run.write(|state| crate::asks::delivery::acknowledge_event(state, &event, "turn-b", now()));
    // An r274 record: in context, bound to nothing.
    let mut record = run.record(REQUESTER, &id);
    record.timeline_item_id = None;
    assert_eq!(backfill_item(&run.state, &record), Some(("cutex-ask-event-b".into(), event.clone())));
    // Another batch (a newer attempt) is never bound to this event.
    let mut other = record.clone();
    other.batch_id = Some("aev-other".into());
    assert_eq!(backfill_item(&run.state, &other), None);
    // Another session's record, or one read by a tool: not derivable.
    let mut elsewhere = record.clone();
    elsewhere.cutex_session_id = "cutex.someone-else".into();
    assert_eq!(backfill_item(&run.state, &elsewhere), None);
    let mut read = record.clone();
    read.evidence = Some(Evidence::ReadByTool);
    assert_eq!(backfill_item(&run.state, &read), None);
    // The event is gone (archived): not derivable.
    let mut state = run.state.clone();
    state.events.remove(&event);
    assert_eq!(backfill_item(&state, &record), None);
    // Settled by a read (no event): not derivable.
    let (ask, settled) = answered(&mut run, "w");
    run.write(|state| {
        crate::asks::delivery::settle_for_ask(state, REQUESTER, &ask, None, "read", now());
    });
    let mut settled = run.record(REQUESTER, &settled);
    settled.surface = Surface::Timeline;
    settled.evidence = Some(Evidence::TurnStartInput);
    assert_eq!(backfill_item(&run.state, &settled), None);
}

// r274.1: a delegation request's native item on its target is its attempt's
// message (`cutex-ask-msg-*`), never the askId the desktop could join on.

fn request_attempt(n: u32, state: AttemptState, turn: Option<&str>) -> AttemptRecord {
    let mut record = AttemptRecord::submitting(n, format!("cutex-ask-msg-{n}"), 1, now_text());
    record.state = state;
    record.turn_id = turn.map(str::to_string);
    if matches!(state, AttemptState::Running | AttemptState::Answered) {
        record.started_at = Some(now_text());
    }
    record
}

/// The ask runs its attempt `n` in `turn` (receipt, then turn start).
fn run_attempt(run: &mut Run, ask: &str, n: u32, turn: &str) {
    set(run, ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.hold = None;
        ask.attempts.push(request_attempt(n, AttemptState::Admitted, Some(turn)));
    });
    set(run, ask, |ask| {
        ask.execution = ExecutionState::Running;
        *ask.attempts.last_mut().unwrap() = request_attempt(n, AttemptState::Running, Some(turn));
    });
}

#[test]
fn request_in_context_binds_its_attempt_message_not_the_ask_id() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    run_attempt(&mut run, &ask, 1, "turn-1");
    let record = run.record(TARGET, &ask);
    assert_eq!(record.state, InputState::InContext);
    assert_eq!((record.turn_id.as_deref(), record.timeline_item_id.as_deref()), (Some("turn-1"), Some("cutex-ask-msg-1")));
    assert_ne!(record.timeline_item_id.as_deref(), Some(ask.as_str()));
    // Only `inContext` names the item: a sent request is in no item yet.
    let mut run = Run::new();
    let ask = create(&mut run, "b");
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.attempts.push(request_attempt(1, AttemptState::Admitted, Some("turn-1")));
    });
    let record = run.record(TARGET, &ask);
    assert_eq!((record.state, record.timeline_item_id.as_deref()), (InputState::Sending, None));
    // A request that ran and ended in one store write binds the same item.
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Answered;
        ask.attempts[0] = request_attempt(1, AttemptState::Answered, Some("turn-1"));
    });
    assert_eq!(run.record(TARGET, &ask).timeline_item_id.as_deref(), Some("cutex-ask-msg-1"));
    // The drift / rebuild replay of the store binds it too.
    let mut lost = run.record(TARGET, &ask);
    lost.state = InputState::Sending;
    let job = drift_job(&run.state, &lost).expect("a replay");
    let in_context = job.steps.iter().flat_map(|step| &step.alts).find(|next| next.state == InputState::InContext).unwrap();
    assert_eq!(in_context.timeline_item_id.as_deref(), Some("cutex-ask-msg-1"));
}

#[test]
fn a_stale_request_attempt_never_rebinds_a_newer_one() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.attempts.push(request_attempt(1, AttemptState::Admitted, Some("turn-1")));
    });
    // Attempt 1's turn start, not applied yet (a writer thread behind).
    let stale = run.commit(|state| {
        state.update_ask(&ask, now(), |ask| {
            ask.execution = ExecutionState::Running;
            ask.attempts[0] = request_attempt(1, AttemptState::Running, Some("turn-1"));
        });
    });
    // It was refused after all; attempt 2 runs and is in context.
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Queued;
        ask.attempts[0].state = AttemptState::Refused;
        ask.hold = Some(Hold::TargetBusy { position: None });
    });
    run_attempt(&mut run, &ask, 2, "turn-2");
    let record = run.record(TARGET, &ask);
    assert_eq!((record.turn_id.as_deref(), record.timeline_item_id.as_deref()), (Some("turn-2"), Some("cutex-ask-msg-2")));
    // The late attempt-1 fact changes nothing.
    let late: Vec<_> = stale.iter().flat_map(|job| apply_job(&run.ledger, job)).collect();
    assert!(late.is_empty(), "{late:?}");
    assert_eq!(run.record(TARGET, &ask), record);
}

#[test]
fn backfill_request_item_derives_only_from_the_attempt_of_the_record_turn() {
    let mut run = Run::new();
    let ask = create(&mut run, "a");
    run_attempt(&mut run, &ask, 1, "turn-1");
    // An r274 record: in context, bound to nothing.
    let mut record = run.record(TARGET, &ask);
    record.timeline_item_id = None;
    assert_eq!(backfill_request_item(&run.state, &record), Some("cutex-ask-msg-1".into()));
    // A turn no attempt started in, or no turn at all: not derivable.
    let mut other = record.clone();
    other.turn_id = Some("turn-9".into());
    assert_eq!(backfill_request_item(&run.state, &other), None);
    let mut no_turn = record.clone();
    no_turn.turn_id = None;
    assert_eq!(backfill_request_item(&run.state, &no_turn), None);
    // Another session's record, or a record not in context: not derivable.
    let mut elsewhere = record.clone();
    elsewhere.cutex_session_id = "cutex.someone-else".into();
    assert_eq!(backfill_request_item(&run.state, &elsewhere), None);
    let mut sending = record.clone();
    sending.state = InputState::Sending;
    assert_eq!(backfill_request_item(&run.state, &sending), None);
    // An attempt submitted in that turn but never started: not derivable.
    let mut state = run.state.clone();
    state.update_ask(&ask, now(), |ask| {
        ask.attempts[0].state = AttemptState::Admitted;
        ask.attempts[0].started_at = None;
    });
    assert_eq!(backfill_request_item(&state, &record), None);
    // Two attempts naming the turn: ambiguous, not derivable.
    let mut state = run.state.clone();
    state.update_ask(&ask, now(), |ask| ask.attempts.push(request_attempt(2, AttemptState::Running, Some("turn-1"))));
    assert_eq!(backfill_request_item(&state, &record), None);
    // The ask is gone (archived): not derivable.
    let mut state = run.state.clone();
    state.asks.remove(&ask);
    assert_eq!(backfill_request_item(&state, &record), None);
}

// ---------------------------------------------------------------------------
// Passive items (r274.2, owner 2026-11-03): only an input certain to reach
// the agent's context enters nextUp / pendingNotes.

/// A delegation held by its runtime and its passive `held@<revision>` item
/// (the host-b case before r274.2: `waiting{passive}` in pendingNotes,
/// dismiss only, never delivered).
fn held_notice(run: &mut Run, action: &str) -> (String, String) {
    let ask = create(run, action);
    let mut id = None;
    run.write(|state| {
        state.update_ask(&ask, now(), |ask| ask.hold = Some(Hold::HeldByRuntime { native: Some("busy".into()) }));
        let record = state.asks[&ask].clone();
        id = crate::asks::delivery::record_state_delivery(state, &record, "held", now());
    });
    (ask, id.expect("held item"))
}

/// Every transition the jobs of one store write apply, in order.
fn write_transitions(run: &mut Run, mutate: impl FnOnce(&mut AskState)) -> Vec<Transition> {
    let jobs = run.commit(mutate);
    jobs.iter().flat_map(|job| apply_job(&run.ledger, job)).collect()
}

#[test]
fn passive_status_result_and_handover_items_create_no_ledger_record() {
    let mut run = Run::new();
    let (ask, held) = held_notice(&mut run, "held");
    assert!(run.state.deliveries[&held].passive && run.state.deliveries[&held].state == DeliveryState::Pending);
    assert!(no_record(&run, &held), "a passive status notice is not a ledger record");
    // The asks item is kept: delegation_wait / agent_list still read it.
    assert!(crate::asks::delivery::actionable(&run.state, &run.state.deliveries[&held]));
    // A notify: passive result and a passive handover summary: no record.
    let passive_ask = create(&mut run, "passive-result");
    let mut result = None;
    let mut handover = None;
    run.write(|state| {
        state.update_ask(&passive_ask, now(), |ask| ask.options.notify = Notify::Passive);
        let record = state.asks[&passive_ask].clone();
        state.answers.insert("ans-p".into(), AnswerRecord {
            answer_id: "ans-p".into(), ask_id: passive_ask.clone(), attempt: 1, revision: 1,
            status: AnswerStatus::Completed, artifact_id: None, bytes: 0, metadata: Default::default(),
            created_at: now_text(), preview: Some("done".into()),
        });
        result = crate::asks::delivery::record_answer_delivery(state, &record, "ans-p", now());
        handover = crate::asks::delivery::record_handover_delivery(state, &record, 2, true, now());
    });
    for id in [result.expect("result"), handover.expect("handover")] {
        assert!(run.state.deliveries[&id].passive);
        assert!(no_record(&run, &id), "{id}");
    }
    // The write that lifts the hold creates nothing either.
    set(&mut run, &ask, |ask| ask.hold = None);
    assert!(no_record(&run, &held));
    // A rebuild / drift replay from "no record" creates none.
    assert_eq!(delivery_job(&run.state, &run.state, None, &run.state.deliveries[&held]), None);
}

#[test]
fn a_passive_item_read_by_a_tool_is_recorded_off_the_panels() {
    let mut run = Run::new();
    let (ask, _) = held_notice(&mut run, "wait");
    let transitions = write_transitions(&mut run, |state| {
        crate::asks::delivery::claim_for_wait(state, REQUESTER, &[ask.clone()], "wait-1", now());
    });
    let labels: Vec<String> = transitions.iter().map(|t| label(&t.record)).collect();
    assert_eq!(labels, ["waiting{passive}", "inContext(claimedByWait)"]);
    assert!(transitions.iter().all(|t| t.record.surface == Surface::None), "never on nextUp / pendingNotes");
    let (read_ask, _) = held_notice(&mut run, "read");
    let transitions = write_transitions(&mut run, |state| {
        crate::asks::delivery::settle_for_ask(state, REQUESTER, &read_ask, None, "read", now());
    });
    assert_eq!(transitions.last().map(|t| label(&t.record)).as_deref(), Some("inContext(readByTool)"));
    assert!(transitions.iter().all(|t| t.record.surface == Surface::None));
    // Closed before anyone read it: nothing to settle in the ledger.
    let (closed_ask, closed) = held_notice(&mut run, "closed");
    run.write(|state| {
        crate::asks::delivery::settle_for_ask(state, REQUESTER, &closed_ask, None, "closed", now());
    });
    assert!(no_record(&run, &closed));
}

#[test]
fn an_item_made_passive_leaves_the_panel_as_discarded_superseded() {
    let mut run = Run::new();
    let (ask, id) = answered(&mut run, "budget");
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::NextUp);
    // Over budget: the pending result goes passive (read by a tool only).
    run.write(|state| {
        assert_eq!(crate::asks::delivery::degrade_to_passive(state, REQUESTER, now()), 1);
    });
    assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "discarded{superseded}"]);
    assert_eq!(run.record(REQUESTER, &id).surface, Surface::None);
    // The asks item stays readable by delegation_wait; the claim leaves the
    // terminal record alone.
    run.write(|state| {
        crate::asks::delivery::claim_for_wait(state, REQUESTER, &[ask.clone()], "wait-1", now());
    });
    assert_eq!(run.state.deliveries[&id].state, DeliveryState::ClaimedByWait);
    assert_eq!(run.path(&id), ["waiting{resultCoalescing}", "discarded{superseded}"]);
    // An item in flight that goes passive is not touched (its sender writes it).
    let (_, sent) = answered(&mut run, "sent");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[sent.clone()], &event, None));
    run.write(|state| {
        let delivery = state.deliveries.get_mut(&sent).unwrap();
        delivery.state = DeliveryState::Pending;
        delivery.passive = true;
        delivery.event_id = None;
    });
    assert_eq!(run.record(REQUESTER, &sent).state, InputState::Sending);
}

/// A `waiting{passive}` record as cutex wrote it before r274.2.
fn legacy_passive_record(run: &Run, id: &str) {
    let input = delivery_input(&run.state, &run.state.deliveries[id]).expect("input");
    let record = run.ledger.transition(REQUESTER, id, Expect::rev(0), Next::waiting("passive").create(input)).unwrap().record;
    assert_eq!((label(&record).as_str(), record.surface), ("waiting{passive}", Surface::PendingNotes));
}

#[test]
fn startup_discards_existing_passive_records_once_and_keeps_the_asks_item() {
    use crate::management::v2::input_ledger_asks::PassiveNotices;
    let mut run = Run::new();
    let (_, held) = held_notice(&mut run, "held");
    legacy_passive_record(&run, &held);
    // A pending result that will be delivered (resultCoalescing) stays.
    let (_, live) = answered(&mut run, "live");
    let mut sweep = PassiveNotices::start();
    while !sweep.done() {
        sweep.step(&run.ledger);
    }
    assert_eq!((sweep.found, sweep.settled), (1, 1));
    let record = run.record(REQUESTER, &held);
    assert_eq!((label(&record).as_str(), record.surface), ("discarded{superseded}", Surface::None));
    assert!(record.actions.is_empty(), "no dismiss action left");
    assert_eq!(label(&run.record(REQUESTER, &live)), "waiting{resultCoalescing}");
    // Ledger only: the asks item is still pending for delegation_wait.
    assert_eq!(run.state.deliveries[&held].state, DeliveryState::Pending);
    // Idempotent: the next engine start finds nothing.
    let rev = record.rev;
    let mut again = PassiveNotices::start();
    while !again.done() {
        again.step(&run.ledger);
    }
    assert_eq!((again.found, again.settled), (0, 0));
    assert_eq!(run.record(REQUESTER, &held).rev, rev);
}


#[test]
fn notices_read_retires_pending_input_without_fabricating_context_receipt() {
    for progress in 0..4 {
        let mut run = Run::new();
        let (_, id) = answered(&mut run, "notice-read");
        for next in [Next::sending(), Next::accepted(), Next::uncertain("outcomeUnknown")].into_iter().take(progress) {
            run.ledger.transition(REQUESTER, &id, Expect::ANY, next).unwrap();
        }
        run.write(|state| {
            let delivery = state.deliveries.get_mut(&id).unwrap();
            delivery.state = DeliveryState::Acknowledged;
            delivery.settled_by = Some("notices_read".into());
        });
        let retired = run.record(REQUESTER, &id);
        assert_eq!(retired.state, InputState::Discarded);
        assert_eq!(retired.reason.as_deref(), Some("settlementClosed"));
        assert!(retired.turn_id.is_none());
        assert!(retired.timeline_item_id.is_none());
    }
}

#[path = "ledger_request_evidence_tests.rs"]
mod request_evidence_tests;


#[test]
fn dismissed_result_recovers_after_late_retry_metadata_and_on_rebuild() {
    let mut run = Run::new();
    let (_, id) = answered(&mut run, "dismiss-retry-order");
    let event = coalesce(&mut run, "m1");
    run.apply(submitted_jobs(&run.state, REQUESTER, &[id.clone()], &event, Some("turn-1")));
    run.apply(turn_ended_jobs(&run.state, REQUESTER, "turn-1", "turnInterrupted"));
    run.ledger.transition(REQUESTER, &id, Expect::ANY, Next::waiting("ownerRetry")).unwrap();
    let delayed = run.commit(|state| retry_result(state, REQUESTER, &id, now()).unwrap());
    let dismissed = run.commit(|state| {
        let delivery = state.deliveries.get_mut(&id).unwrap();
        delivery.state = DeliveryState::Acknowledged;
        delivery.settled_by = Some(DISMISSED.into());
    });
    run.apply(delayed.clone());
    let waiting = run.record(REQUESTER, &id);
    assert_eq!(waiting.state, InputState::Waiting);
    // A restart only has the committed source and persisted waiting row.
    let rebuilt = drift_job(&run.state, &waiting).expect("dismissed source repairs drift");
    run.apply(vec![rebuilt]);
    let done = run.record(REQUESTER, &id);
    assert_eq!((done.state, done.reason.as_deref()), (InputState::Discarded, Some("ownerDiscarded")));
    run.apply(dismissed);
    run.apply(delayed);
    assert_eq!(run.record(REQUESTER, &id), done, "replay cannot resurrect the dismissed attempt");
}

#[test]
fn disposed_request_occurrence_stays_closed_but_explicit_new_work_enters_normally() {
    let mut run=Run::new();
    let ask_id=create(&mut run,"occurrence");
    let old=run.record(TARGET,&ask_id);
    crate::management::v2::input_disposal::request_at(&run.root.join(il::DATABASE_FILE),TARGET,&old.input_id,Utc::now()).unwrap();
    let discarded=run.ledger.transition(TARGET,&old.input_id,Expect::ANY,Next::discarded("ownerDiscarded")).unwrap();
    assert_eq!(discarded.record.state,InputState::Discarded);
    set(&mut run,&ask_id,|ask| {ask.execution=ExecutionState::Interrupted;});
    let mut newer=None;
    run.write(|state| {newer=Some(super::super::lifecycle::retry(state,&ask_id,None,Utc::now()).unwrap());});
    let newer=newer.unwrap();
    assert_ne!(newer.request_input_id(),old.input_id);
    assert_eq!(newer.ask_id,ask_id);
    let row=run.record(TARGET,&newer.request_input_id());
    assert_eq!(row.state,InputState::Waiting);
    assert_eq!(row.ask_id.as_deref(),Some(ask_id.as_str()));
    let old_row=run.record(TARGET,&old.input_id);
    assert_eq!(old_row.state,InputState::Discarded);
    // A stale source job for occurrence zero cannot reopen either identity.
    let before=AskState::default();
    let old_ask=AskState::provisional(&new_ask("stale","unused"),Utc::now());
    let mut old_ask=old_ask;old_ask.ask_id=ask_id.clone();
    if let Some(job)=request_job(&before,&run.state,None,&old_ask){run.apply(vec![job]);}
    assert_eq!(run.record(TARGET,&old.input_id).state,InputState::Discarded);
    assert_eq!(run.record(TARGET,&newer.request_input_id()).state,InputState::Waiting);
}

#[test]
fn delivery_recipient_transfer_retires_old_tracking_without_acknowledging_new_recipient() {
    let mut run=Run::new();
    let (ask,id)=answered(&mut run,"transfer");
    assert_eq!(run.record(REQUESTER,&id).state,InputState::Waiting);
    run.write(|state| {state.deliveries.get_mut(&id).unwrap().routed_to=Some(TARGET.into());});
    let old=run.record(REQUESTER,&id);
    assert_eq!((old.state,old.surface),(InputState::Discarded,Surface::None));
    assert_eq!(run.record(TARGET,&id).state,InputState::Waiting);
    let (store,_dir)=temp_store("recipient-scope");
    store.with_state(|state|*state=run.state.clone()).unwrap();
    dismiss(&store,&old,now()).unwrap();
    let after=store.snapshot().unwrap();
    assert_eq!(after.deliveries[&id].state,DeliveryState::Pending);
    assert_eq!(after.deliveries[&id].routed_to.as_deref(),Some(TARGET));
    assert_eq!(after.asks[&ask].closed,run.state.asks[&ask].closed);
}

#[test]
fn r47_unknown_terminal_request_and_result_disposal_keep_stopped_semantics() {
    for failed in [false,true] {
        for raw in [false,true] {
            for result in [false,true] {
                let mut run=Run::new();
                let (ask,id)=if result {answered(&mut run,"unknown-result")} else {
                    let ask=create(&mut run,"unknown-request");(ask.clone(),ask)
                };
                set(&mut run,&ask,|ask| {
                    ask.execution=if failed {ExecutionState::Failed} else {ExecutionState::Interrupted};
                    let mut unknown=attempt(AttemptState::Interrupted,None);
                    unknown.interrupt_reason=Some(InterruptReason::OutcomeUnknown);ask.attempts=vec![unknown];
                });
                let session=if result {REQUESTER} else {TARGET};
                let record=run.record(session,&id);
                let (store,_dir)=temp_store("decision9-restored");
                store.with_state(|state|*state=run.state.clone()).unwrap();
                if raw {assert!(dismiss_input_id(&store,session,&id,now()).unwrap());}
                else {dismiss(&store,&record,now()).unwrap();}
                let source=store.snapshot().unwrap();
                assert_eq!(source.asks[&ask].closed,Some(CloseOutcome::Stopped));
                assert_eq!(source.asks[&ask].execution,if failed {ExecutionState::Failed} else {ExecutionState::Interrupted});
                assert!(source.commands.is_empty());assert_eq!(source.asks[&ask].attempts.len(),1);
                if result {assert_eq!(source.deliveries[&id].settled_by.as_deref(),Some(DISMISSED));}
            }
        }
    }
}

#[test]
fn r47_disposal_does_not_close_running_work_or_a_different_request_occurrence() {
    for raw in [false,true] {
        let mut run=Run::new();let (ask,id)=answered(&mut run,"new-running-work");
        set(&mut run,&ask,|ask| {
            ask.execution=ExecutionState::Running;
            let mut next=attempt(AttemptState::Submitting,None);next.admission_occurrence=1;
            ask.admission_occurrence=1;ask.attempts=vec![next];
        });
        let record=run.record(REQUESTER,&id);
        let (store,_dir)=temp_store("running-not-stopped");
        store.with_state(|state|*state=run.state.clone()).unwrap();
        if raw {dismiss_input_id(&store,REQUESTER,&id,now()).unwrap();} else {dismiss(&store,&record,now()).unwrap();}
        let source=store.snapshot().unwrap();
        assert_eq!(source.asks[&ask].execution,ExecutionState::Running);assert_eq!(source.asks[&ask].closed,None);

        let mut run=Run::new();let ask=create(&mut run,"old-request-occurrence");
        let old=run.record(TARGET,&ask);
        set(&mut run,&ask,|ask| {
            ask.execution=ExecutionState::Interrupted;ask.admission_occurrence=1;
            let mut next=attempt(AttemptState::Interrupted,None);next.admission_occurrence=1;
            next.interrupt_reason=Some(InterruptReason::OutcomeUnknown);ask.attempts=vec![next];
        });
        let (store,_dir)=temp_store("old-occurrence-not-stopped");
        store.with_state(|state|*state=run.state.clone()).unwrap();
        if raw {assert!(!dismiss_input_id(&store,TARGET,&old.input_id,now()).unwrap());} else {dismiss(&store,&old,now()).unwrap();}
        let source=store.snapshot().unwrap();assert_eq!(source.asks[&ask].closed,None);
        assert_eq!(source.asks[&ask].admission_occurrence,1);
    }
}

#[test]
fn r1_current_terminal_requests_close_for_restart_or_peer_removal_but_results_do_not() {
    for reason in [InterruptReason::RuntimeRestart,InterruptReason::PeerRemoved] {
        for failed in [false,true] {
            for raw in [false,true] {
                for result in [false,true] {
                    let mut run=Run::new();
                    let (ask,id)=if result {answered(&mut run,"known-interruption-result")} else {
                        let ask=create(&mut run,"known-interruption-request");(ask.clone(),ask)
                    };
                    if !result {
                        // Reproduce submitted but never ran: admission receipt
                        // first, then interruption without started_at/turn.
                        set(&mut run,&ask,|ask| {
                            ask.execution=ExecutionState::Admitted;
                            ask.attempts=vec![attempt(AttemptState::Admitted,None)];
                        });
                        assert_eq!(run.record(TARGET,&id).state,InputState::Sending);
                    }
                    set(&mut run,&ask,|ask| {
                        ask.execution=if failed {ExecutionState::Failed} else {ExecutionState::Interrupted};
                        let mut ended=attempt(AttemptState::Interrupted,None);
                        ended.interrupt_reason=Some(reason);ask.attempts=vec![ended];
                    });
                    let session=if result {REQUESTER} else {TARGET};
                    let record=run.record(session,&id);
                    if !result && !failed {
                        assert_eq!(record.state,InputState::Uncertain);
                        assert_eq!(record.reason.as_deref(),Some("turnInterrupted"));
                        assert!(il::dismissible(&record));
                    }
                    let (store,_dir)=temp_store("terminal-request-reason");
                    store.with_state(|state|*state=run.state.clone()).unwrap();
                    if raw {assert!(dismiss_input_id(&store,session,&id,now()).unwrap());}
                    else {dismiss(&store,&record,now()).unwrap();}
                    let source=store.snapshot().unwrap();
                    assert_eq!(source.asks[&ask].closed,if result {None} else {Some(CloseOutcome::Stopped)});
                    assert_eq!(source.asks[&ask].execution,if failed {ExecutionState::Failed} else {ExecutionState::Interrupted});
                    assert!(source.commands.is_empty());assert_eq!(source.asks[&ask].attempts.len(),1);
                    if result {assert_eq!(source.deliveries[&id].settled_by.as_deref(),Some(DISMISSED));}
                }
            }
        }
    }
}
