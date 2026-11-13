use super::*;

fn timed_out(run: &mut Run) -> String {
    let ask = create(run, "late-current-evidence");
    set(run, &ask, |ask| {
        ask.execution = ExecutionState::Admitted;
        ask.attempts = vec![attempt(AttemptState::Admitted, Some("turn-old"))];
    });
    let row = run.record(TARGET, &ask);
    run.ledger.transition(TARGET, &ask, Expect::rev(row.rev), Next::uncertain("outcomeUnknown")).unwrap();
    ask
}

#[test]
fn timed_out_request_is_healed_only_by_bound_current_execution() {
    let mut run = Run::new(); let ask = timed_out(&mut run);
    set(&mut run, &ask, |ask| { ask.execution = ExecutionState::Running; ask.attempts[0].state = AttemptState::Running; });
    let row = run.record(TARGET, &ask);
    assert_eq!(row.state, InputState::InContext);
    assert_eq!(row.evidence, Some(Evidence::TurnStartInput));
    assert_eq!(row.turn_id.as_deref(), Some("turn-old"));
    assert_eq!(row.timeline_item_id.as_deref(), Some("cutex-ask-msg-1"));
    assert_eq!(row.details[SOURCE_VERSION], run.state.revision);
}

#[test]
fn periodic_reconcile_also_heals_a_missed_execution_event() {
    let mut run = Run::new(); let ask = timed_out(&mut run);
    let _lost = run.commit(|state| { state.update_ask(&ask, now(), |ask| {
        ask.execution = ExecutionState::Running; ask.attempts[0].state = AttemptState::Running;
    }); });
    let row = run.record(TARGET, &ask);
    assert_eq!(row.state, InputState::Uncertain);
    let job = drift_job(&run.state, &row).unwrap(); run.apply(vec![job]);
    assert_eq!(run.record(TARGET, &ask).state, InputState::InContext);
}

#[test]
fn delayed_old_attempt_cannot_overwrite_a_new_attempt_even_before_it_runs() {
    let mut run = Run::new(); let ask = timed_out(&mut run);
    let old = run.commit(|state| { state.update_ask(&ask, now(), |ask| {
        ask.execution = ExecutionState::Running; ask.attempts[0].state = AttemptState::Running;
    }); });
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Queued;
        let mut new = AttemptRecord::submitting(2, "cutex-ask-msg-new".into(), 2, now_text());
        new.turn_id = Some("turn-new".into()); ask.attempts.push(new);
    });
    let fenced = run.record(TARGET, &ask);
    assert_eq!(fenced.state, InputState::Uncertain);
    assert_eq!(fenced.details[SOURCE_VERSION], run.state.revision);
    run.apply(old.clone());
    assert_eq!(run.record(TARGET, &ask), fenced, "late old proof cannot cross the newer source fence");
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Running;
        ask.attempts.last_mut().unwrap().state = AttemptState::Running;
    });
    let current = run.record(TARGET, &ask);
    assert_eq!(current.state, InputState::InContext);
    assert_eq!(current.turn_id.as_deref(), Some("turn-new"));
    assert_eq!(current.timeline_item_id.as_deref(), Some("cutex-ask-msg-new"));
    run.apply(old); assert_eq!(run.record(TARGET, &ask), current);
}

#[test]
fn unbound_or_old_running_attempt_is_not_current_read_evidence() {
    let mut run = Run::new(); let ask = timed_out(&mut run);
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Running;
        ask.attempts[0].state = AttemptState::Running; ask.attempts[0].turn_id = None;
    });
    assert_eq!(run.record(TARGET, &ask).state, InputState::Uncertain);
    set(&mut run, &ask, |ask| {
        ask.execution = ExecutionState::Failed;
        let mut new = AttemptRecord::submitting(2, "new-unread".into(), 2, now_text());
        new.state = AttemptState::Failed; new.turn_id = Some("not-started".into());
        ask.attempts.push(new);
    });
    assert_eq!(run.record(TARGET, &ask).state, InputState::Uncertain);
}
