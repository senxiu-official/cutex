//! Ordered, original owner inputs across one interrupt. Native must acknowledge
//! ownerInputBatch:1 before any interruption; ownerInputs are a prefix of the
//! final carrier's input, not ExternalInput envelopes or copied composer text.
use super::*;
use cutex::management::v2::input_interrupt_send::{InterruptSendMany, ManyOutcome};

const MARK: &str = "interruptBatch";

/// Absence requires a complete stopped-turn scan. Keep positive proof if a
/// later page fails, but never treat a truncated history as permission to resend.
pub(super) fn check_turn_history(
    ids: &[String],
    mut request: impl FnMut(Option<&str>) -> Option<Value>,
) -> Vec<TurnCheck> {
    let mut checks = vec![TurnCheck::Unavailable; ids.len()];
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    // Bound one shared scan (not one scan per input). Beyond this boundary,
    // unresolved IDs remain uncertain rather than holding the input lane forever.
    for _ in 0..64 {
        if checks.iter().all(|check| *check == TurnCheck::Found) {
            return checks;
        }
        let Some(page) = request(cursor.as_deref()) else {
            return checks;
        };
        if !page.get("data").is_some_and(Value::is_array) {
            return checks;
        }
        for (id, check) in ids.iter().zip(&mut checks) {
            if matches!(
                cutex::management::v2::input_ledger_settle::parse_turn_items(&page, id),
                cutex::management::v2::input_ledger_settle::Check::Persisted(_)
            ) {
                *check = TurnCheck::Found;
            }
        }
        match page.get("nextCursor") {
            None | Some(Value::Null) => {
                for check in &mut checks {
                    if *check != TurnCheck::Found {
                        *check = TurnCheck::Absent;
                    }
                }
                return checks;
            }
            Some(Value::String(next)) if !next.is_empty() && seen.insert(next.clone()) => {
                cursor = Some(next.clone());
            }
            _ => return checks,
        }
    }
    checks
}

fn message_for_new_turn(
    params: &cutex::management::v2::user_input::UserInputSubmitParams,
) -> cutex::management::v2::user_input::UserInputSubmitParams {
    let mut message = params.clone();
    // These fence the old operation/turn, not the logical message. Its durable
    // batch mark owns replay; retaining them on a row's recovery payload would
    // replay the stale batch/join (and possibly interrupt a second time).
    message.interrupt_send = None;
    message.join_delegation = None;
    message.expected_turn_id = None;
    message.strategy = UserInputStrategy::Auto;
    message
}

fn not_submitted_reason(error: &UserInputExecutionError) -> Option<&str> {
    error
        .details
        .pointer("/nativeError/data/ownerInputBatchNotSubmitted")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
}

/// A lost/settled member ends automatic custody for every remaining member
/// at once. Never let the worker drip the survivors through singleton routing.
fn block_incomplete_group(
    ops: &dyn Ops,
    session: &str,
    ids: &[String],
    identity: &Value,
) -> Result<bool, UserInputExecutionError> {
    let ledger = ops
        .ledger()
        .map_err(|e| execution_error("route", "event_persistence_unavailable", e, true, false))?;
    let records = ledger.get(session, ids).map_err(ledger_failure)?.records;
    let retained = |r: &&InputRecord| {
        r.state == InputState::Held
            && r.details[MARK]["identity"] == *identity
            && r.details[MARK]["deferred"] == true
            && r.details[MARK]["attempt"].as_u64() == Some(r.attempt)
    };
    let survivors = records.iter().filter(retained).collect::<Vec<_>>();
    if survivors.len() == ids.len() {
        return Ok(false);
    }
    let incomplete = ids
        .iter()
        .filter(|id| !survivors.iter().any(|r| &r.input_id == *id))
        .collect::<Vec<_>>();
    let batch = survivors
        .iter()
        .map(|record| {
            let mut details = record.details.clone();
            details[MARK]["deferred"] = json!(false);
            details[MARK]["blocked"] = json!(true);
            details[MARK]["notSubmittedReason"] = json!("groupIncomplete");
            details[MARK]["incompleteIds"] = json!(incomplete);
            (
                record.input_id.clone(),
                Expect::rev(record.rev),
                Next::waiting("nativeHeld").deadline(None).details(details),
            )
        })
        .collect::<Vec<_>>();
    if !batch.is_empty() {
        for transition in ledger
            .transition_batch(session, batch)
            .map_err(ledger_failure)?
        {
            ops.publish(&transition);
        }
    }
    for record in survivors {
        ops.held_finish(session, &record.input_id)
            .map_err(queue_persistence_error)?;
    }
    Ok(true)
}

/// These identities are proved not sent. One held group (not independent
/// singleton retries) preserves their order and the original operation mark.
fn defer_known(
    ops: &dyn Ops,
    session: &str,
    thread: &str,
    inputs: &[(InputRecord, Input)],
    reason: &str,
    automatic: bool,
    native_error: Option<&Value>,
) -> Result<(), UserInputExecutionError> {
    let ledger = ops
        .ledger()
        .map_err(|e| execution_error("route", "event_persistence_unavailable", e, true, false))?;
    let ids = inputs
        .iter()
        .map(|(r, _)| r.input_id.clone())
        .collect::<Vec<_>>();
    for (record, input) in inputs {
        let current = latest(ledger, session, &record.input_id)?;
        if current.state == InputState::InContext {
            continue;
        }
        let prior = ops
            .repository()
            .delivery(session, &record.input_id)
            .map_err(queue_persistence_error)?;
        ops.repository()
            .set_delivery(session, &record.input_id, prior.as_ref(), None)
            .map_err(queue_persistence_error)?;
        let mut details = current.details.clone();
        details["interruptSend"] =
            json!({"interruptedTurnId":details[MARK]["identity"]["request"]["expectedTurnId"]});
        details[MARK]["deferredIds"] = json!(ids);
        details[MARK]["deferred"] = json!(automatic);
        details[MARK]["notSubmittedReason"] = json!(reason);
        details[MARK]["carrierInputId"] = json!(ids.last());
        if let Some(error) = native_error {
            details[MARK]["nativeError"] = error.clone();
        }
        let mut next = if automatic {
            let mut params = message_for_new_turn(&input.params);
            params.strategy = UserInputStrategy::Interrupt;
            ops.held_park(
                session,
                thread,
                &input.request_id,
                cutex::management::v2::input_ledger_settle::owner_submit_payload(&params),
            )
            .map_err(queue_persistence_error)?;
            let mut next = Next::held("hostBusy");
            if let Some(held) = ops
                .held_find(session, &record.input_id)
                .map_err(queue_persistence_error)?
            {
                if let Ok(deadline) = chrono::DateTime::parse_from_rfc3339(&held.deadline) {
                    next = next.deadline(Some(deadline.with_timezone(&Utc)));
                }
            }
            next
        } else {
            ops.held_finish(session, &record.input_id)
                .map_err(queue_persistence_error)?;
            details[MARK]["blocked"] = json!(true);
            Next::waiting("nativeHeld").deadline(None)
        }
        .details(details);
        next.carries = Some(Vec::new());
        let transition = ledger
            .transition(session, &record.input_id, Expect::rev(current.rev), next)
            .map_err(ledger_failure)?;
        ops.publish(&transition);
    }
    Ok(())
}

fn unsupported() -> UserInputExecutionError {
    execution_error_message(
        "route",
        "interrupt_send_many_unsupported",
        "runtime lacks ownerInputBatch:1; no turn was interrupted and nothing sent",
        false,
        false,
    )
}

fn latest(
    ledger: &InputLedger,
    session: &str,
    id: &str,
) -> Result<InputRecord, UserInputExecutionError> {
    ledger
        .get(session, &[id.into()])
        .map_err(ledger_failure)?
        .records
        .pop()
        .ok_or_else(|| {
            isend::error(
                "input_not_found",
                format!("no input {id}"),
                json!({"inputId":id}),
            )
        })
}

fn replay(
    ledger: &InputLedger,
    session: &str,
    ids: &[String],
    identity: &Value,
    turn: &str,
) -> Result<Option<(ManyOutcome, Option<UserInputSubmitExecution>)>, UserInputExecutionError> {
    let records = ledger.get(session, ids).map_err(ledger_failure)?.records;
    if !records.iter().any(|r| {
        r.details
            .get(MARK)
            .is_some_and(|m| &m["identity"] == identity)
    }) {
        return Ok(None);
    }
    if records.len() != ids.len()
        || records.iter().any(|r| {
            r.details
                .get(MARK)
                .is_none_or(|m| &m["identity"] != identity)
        })
    {
        return Err(isend::error(
            "revision_conflict",
            "interrupt batch identity changed",
            json!({}),
        ));
    }
    let carrier = records
        .iter()
        .filter(|r| !(r.state == InputState::InContext && r.turn_id.as_deref() == Some(turn)))
        .find_map(|r| r.details[MARK]["carrierInputId"].as_str())
        .map(str::to_owned);
    let accepted = ids
        .iter()
        .filter(|id| {
            records.iter().any(|r| {
                &r.input_id == *id
                    && r.state == InputState::InContext
                    && r.turn_id.as_deref() != Some(turn)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let read = records
        .iter()
        .filter(|r| r.state == InputState::InContext && r.turn_id.as_deref() == Some(turn))
        .map(|r| r.input_id.clone())
        .collect();
    let deferred = records.iter().any(|r| {
        matches!(r.state, InputState::Held | InputState::Waiting)
            && r.details[MARK].get("deferredIds").is_some()
    });
    let execution = if deferred && records.iter().any(|r| r.details[MARK]["deferred"] == true) {
        Some(super::super::super::owner_admission::held_execution())
    } else {
        carrier
            .as_ref()
            .and_then(|id| records.iter().find(|r| &r.input_id == id))
            .filter(|r| accepted.contains(&r.input_id))
            .map(|r| execution(r.turn_id.clone(), UserInputDisposition::Deduplicated))
    };
    Ok(Some((
        ManyOutcome {
            outcome: if deferred {
                Outcome::Held {
                    turn_id: Some(turn.into()),
                }
            } else {
                Outcome::Interrupted {
                    turn_id: turn.into(),
                    started: execution.is_some(),
                }
            },
            carrier_input_id: carrier,
            already_in_context: read,
            submitted: accepted,
        },
        execution,
    )))
}

fn execution(
    turn_id: Option<String>,
    disposition: UserInputDisposition,
) -> UserInputSubmitExecution {
    UserInputSubmitExecution {
        join_refusal: None,
        queued_reason: None,
        disposition,
        app_server_accepted: true,
        native_request_id: None,
        native_method: Some("turn/start".into()),
        turn_id,
        queue: None,
        queue_paused: false,
        passive_notes: None,
    }
}

/// The session input lane is held by the caller. Every selected record becomes
/// sending in one SQLite transaction before the one interrupt, fencing off the
/// ordinary turn-ended settlement of accepted steers.
fn run_many(
    ops: &dyn Ops,
    session: &str,
    request: &InterruptSendMany,
    draft: Option<&UserInputSubmitCommand>,
) -> Result<(ManyOutcome, Option<UserInputSubmitExecution>), UserInputExecutionError> {
    run_many_mode(ops, session, request, draft, None)
}

fn run_many_mode(
    ops: &dyn Ops,
    session: &str,
    request: &InterruptSendMany,
    draft: Option<&UserInputSubmitCommand>,
    resume_identity: Option<&Value>,
) -> Result<(ManyOutcome, Option<UserInputSubmitExecution>), UserInputExecutionError> {
    let ledger = ops
        .ledger()
        .map_err(|e| execution_error("route", "event_persistence_unavailable", e, true, false))?;
    let mut ids = request.input_ids.clone();
    if let Some(draft) = draft {
        ids.push(draft.params.client_user_message_id.clone());
    }
    if let Some(draft) = draft {
        if matches!(
            ops.repository()
                .register_identity(session, &draft.params)
                .map_err(queue_persistence_error)?,
            cutex::management::v2::user_input::ClientIdentityDecision::Conflict
        ) {
            return Err(isend::error(
                "idempotency_conflict",
                "draft identity changed",
                json!({}),
            ));
        }
    }
    let identity = resume_identity.cloned().unwrap_or_else(||json!({"request":request,"draftInputId":draft.map(|d|d.params.client_user_message_id.as_str())}));
    if resume_identity.is_some() && block_incomplete_group(ops, session, &ids, &identity)? {
        return Ok((
            ManyOutcome {
                outcome: Outcome::Held {
                    turn_id: Some(request.expected_turn_id.clone()),
                },
                carrier_input_id: ids.last().cloned(),
                already_in_context: vec![],
                submitted: vec![],
            },
            None,
        ));
    }
    if resume_identity.is_none() {
        if let Some(replayed) = replay(ledger, session, &ids, &identity, &request.expected_turn_id)?
        {
            return Ok(replayed);
        }
    }
    if !ops.supports_owner_batch(session, request.expected_runtime_generation) {
        return Err(unsupported());
    }
    ops.batch_admission(session, &ids)?;
    let delivery = ops.delivery(session);
    let live = ops
        .live(session)
        .filter(|live| {
            (resume_identity.is_some()
                || live.active_turn.as_deref() == Some(&request.expected_turn_id))
                && live.generation == request.expected_runtime_generation
        })
        .ok_or_else(|| isend::conflict(None, delivery.as_ref()))?;
    let delivery = delivery.ok_or_else(|| isend::conflict(None, None))?;
    let mut inputs = Vec::new();
    for id in &request.input_ids {
        let record = latest(ledger, session, id)?;
        let input = if resume_identity.is_some() {
            if record.state != InputState::Held
                || record.details[MARK]["identity"] != identity
                || record.details[MARK]["deferred"] != true
                || request.expected_revs.get(id) != Some(&record.rev)
            {
                return Err(isend::not_interruptible(&record));
            }
            let payload = ledger
                .payload(session, id)
                .map_err(ledger_failure)?
                .ok_or_else(|| isend::not_interruptible(&record))?;
            Input {
                source: Source::Held,
                params: submit_params(&payload).ok_or_else(|| isend::not_interruptible(&record))?,
                request_id: format!("owner-batch-resume-{}", Uuid::new_v4()),
            }
        } else {
            isend::precheck(Some(&record), Some(&delivery), &request.single(id))?;
            locate(ops, ledger, session, &record)?
        };
        // A held entry may precede the ordinary route's identity registration.
        // Register it before any custody/C-010 mutation, as the worker would.
        if resume_identity.is_none()
            && matches!(input.source, Source::Held)
            && matches!(
                ops.repository()
                    .register_identity(session, &input.params)
                    .map_err(queue_persistence_error)?,
                cutex::management::v2::user_input::ClientIdentityDecision::Conflict
            )
        {
            return Err(isend::error(
                "idempotency_conflict",
                "held owner identity changed",
                json!({"inputId":id}),
            ));
        }
        if record.state == InputState::Accepted {
            ops.verify_resend(
                session,
                id,
                &cutex::management::v2::input_ledger_settle::owner_submit_payload(&input.params),
            )?;
        }
        inputs.push((record, input));
    }
    if let Some(draft) = draft {
        cutex::management::v2::user_input::ledger::materialize(
            session,
            &draft.params.client_user_message_id,
        );
        let record = latest(ledger, session, &draft.params.client_user_message_id)?;
        if record.kind != il::InputKind::Owner
            || !matches!(
                record.state,
                InputState::Received | InputState::Held | InputState::Waiting
            )
        {
            return Err(isend::not_interruptible(&record));
        }
        inputs.push((
            record,
            Input {
                source: Source::Steered,
                params: draft.params.clone(),
                request_id: draft.management_request_id.clone(),
            },
        ));
    }
    let all_prefix = inputs
        .iter()
        .take(inputs.len().saturating_sub(1))
        .map(|(record, input)| {
            json!({
        "clientUserMessageId":record.input_id,"input":input.params.input})
        })
        .collect::<Vec<_>>();
    let total=serde_json::to_vec(&json!({"prefix":all_prefix,"input":inputs.last().expect("validated nonempty batch").1.params.input}))
        .map_err(|e|execution_error("route","invalid_request",e,false,false))?.len();
    if total > 1024 * 1024 {
        return Err(isend::error(
            "invalid_request",
            "owner input batch exceeds 1 MiB; nothing interrupted",
            json!({}),
        ));
    }
    let mark = json!({"identity":identity,"carrierInputId":ids.last()});
    let batch = inputs
        .iter()
        .map(|(record, _)| {
            let mut details = record.details.clone();
            if resume_identity.is_none() {
                details[MARK] = mark.clone();
            } else {
                details[MARK]["carrierInputId"] = json!(ids.last());
            }
            details[MARK]["attempt"] = json!(record.attempt);
            details[MARK]["deferred"] = json!(false);
            let mut next = Next::sending()
                .reason(isend::SENDING_REASON)
                .details(details);
            next.runtime_generation = Some(live.generation);
            (record.input_id.clone(), Expect::rev(record.rev), next)
        })
        .collect();
    let transitions = if live.active_turn.is_some() {
        let (transitions, change) = ledger
            .transition_batch_with_delivery(session, batch, &isend::finishing(&delivery))
            .map_err(|e| match e {
                LedgerError::Conflict { .. } => isend::conflict(None, Some(&delivery)),
                e => ledger_failure(e),
            })?;
        ops.publish_delivery(session, &change);
        transitions
    } else {
        ledger
            .transition_batch(session, batch)
            .map_err(ledger_failure)?
    };
    for transition in transitions {
        ops.publish(&transition);
    }
    let _interrupting = isend::Interrupting::start(session);
    let turn = &request.expected_turn_id;
    let outcome = |started, carrier, read, submitted| ManyOutcome {
        outcome: Outcome::Interrupted {
            turn_id: turn.clone(),
            started,
        },
        carrier_input_id: carrier,
        already_in_context: read,
        submitted,
    };
    let quarantine = |id: &str, reason: &str, refusal: Option<&UserInputExecutionError>| {
        if let Ok(record) = latest(ledger, session, id) {
            if record.state == InputState::InContext {
                return;
            }
            if record.state == InputState::Sending {
                let mut next = Next::uncertain(reason);
                if let Some(error) = refusal {
                    let mut details = record.details.clone();
                    details[MARK]["retryBlocked"] = json!(true);
                    details[MARK]["nativeError"] = error
                        .details
                        .get("nativeError")
                        .cloned()
                        .unwrap_or_else(|| json!({"code":error.code,"message":error.message}));
                    next = next.details(details);
                }
                step(ops, session, id, record.rev, next);
            }
        }
        if let Ok(prior) = ops.repository().delivery(session, id) {
            let _ = ops.repository().set_delivery(
                session,
                id,
                prior.as_ref(),
                Some(InputDelivery::Unknown {
                    thread_id: live.thread_id.clone(),
                    since: Utc::now().to_rfc3339(),
                }),
            );
        }
    };
    let owned_result = (|| {
        // Consume operation routing only after the batch CAS owns these rows.
        // Keep the original identity/body for explicit per-message recovery.
        for (record, input) in &inputs {
            ledger
                .put_payload(
                    session,
                    &record.input_id,
                    &cutex::management::v2::input_ledger_settle::owner_submit_payload(
                        &message_for_new_turn(&input.params),
                    ),
                )
                .map_err(ledger_failure)?;
        }
        // Transfer queue/held custody to the durable batch before interrupt IO.
        // Otherwise a lost reply could let their background workers later send a
        // prefix alone after C-010's grace. Payloads above retain explicit recovery.
        for (_, input) in &inputs {
            match &input.source {
                Source::Queued(item) => {
                    if let Some(removed) = ops
                        .repository()
                        .remove(session, &item.queue_id, item.revision)
                        .map_err(queue_persistence_error)?
                    {
                        let queue_revision = ops
                            .repository()
                            .list(session)
                            .map_err(queue_persistence_error)?
                            .0;
                        append_queue_removed_event(
                            &|session: &str,
                              correlation: EventCorrelation,
                              method: &str,
                              params: Value| {
                                ops.append_event(session, correlation, method, params)
                            },
                            session,
                            &input.request_id,
                            &removed,
                            "removed",
                            queue_revision,
                        )
                        .map_err(queue_persistence_error)?;
                    }
                }
                // Keep held custody (and its original deadline) until outcome.
                Source::Held => {}
                Source::Steered => {}
            }
        }
        if live.active_turn.is_some()
            && ops
                .interrupt_batch(session, &live.thread_id, turn, live.generation)
                .and_then(|()| ops.wait_stopped(session, turn))
                .is_err()
        {
            let mut known = Vec::new();
            for (record, input) in &inputs {
                if record.state == InputState::Accepted {
                    quarantine(&record.input_id, "turnInterrupted", None);
                    let _ = ops.held_finish(session, &record.input_id);
                } else {
                    known.push((record.clone(), input.clone()));
                }
            }
            if !known.is_empty() {
                defer_known(
                    ops,
                    session,
                    &live.thread_id,
                    &known,
                    "interrupt_failed",
                    true,
                    None,
                )?;
                let mut result = outcome(
                    false,
                    known.last().map(|(r, _)| r.input_id.clone()),
                    vec![],
                    vec![],
                );
                result.outcome = Outcome::Held {
                    turn_id: Some(turn.clone()),
                };
                return Ok((
                    result,
                    Some(super::super::super::owner_admission::held_execution()),
                ));
            }
            return Ok((outcome(false, None, vec![], vec![]), None));
        }
        let mut unread = Vec::new();
        let mut read = Vec::new();
        let accepted_ids = inputs
            .iter()
            .filter(|(record, _)| {
                record.state == InputState::Accepted
                    && !draft.is_some_and(|d| d.params.client_user_message_id == record.input_id)
            })
            .map(|(record, _)| record.input_id.clone())
            .collect::<Vec<_>>();
        let checks = accepted_ids
            .iter()
            .cloned()
            .zip(ops.turn_has_batch(
                session,
                &live.thread_id,
                turn,
                &accepted_ids,
                live.generation,
            ))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (record, input) in inputs {
            let is_draft =
                draft.is_some_and(|d| d.params.client_user_message_id == record.input_id);
            let check = if record.state == InputState::Accepted && !is_draft {
                checks
                    .get(&record.input_id)
                    .copied()
                    .unwrap_or(TurnCheck::Unavailable)
            } else {
                TurnCheck::Absent
            };
            match check {
                TurnCheck::Found => {
                    let current = latest(ledger, session, &record.input_id)?;
                    if current.state != InputState::InContext {
                        step(
                            ops,
                            session,
                            &record.input_id,
                            current.rev,
                            Next::in_context(Evidence::HistoryRead).turn(turn.clone()),
                        );
                    }
                    read.push(record.input_id);
                }
                TurnCheck::Unavailable => quarantine(&record.input_id, "turnInterrupted", None),
                TurnCheck::Absent => {
                    let current = latest(ledger, session, &record.input_id)?;
                    if current.state == InputState::InContext {
                        read.push(record.input_id);
                    } else if current.state == InputState::Sending {
                        unread.push((current, input));
                    } else {
                        return Err(isend::error(
                            "request_outcome_unknown",
                            "selected input advanced during interruption",
                            json!({"inputId":record.input_id}),
                        ));
                    }
                }
            }
        }
        if unread.is_empty() {
            return Ok((outcome(false, None, read, vec![]), None));
        }
        let Some(now) = ops.live(session).filter(|now| {
            now.thread_id == live.thread_id
                && now.generation == live.generation
                && now.active_turn.is_none()
        }) else {
            defer_known(
                ops,
                session,
                &live.thread_id,
                &unread,
                "turn_changed",
                true,
                None,
            )?;
            let mut result = outcome(
                false,
                unread.last().map(|(r, _)| r.input_id.clone()),
                read,
                vec![],
            );
            result.outcome = Outcome::Held {
                turn_id: Some(turn.clone()),
            };
            return Ok((
                result,
                Some(super::super::super::owner_admission::held_execution()),
            ));
        };
        let carrier_id = unread.last().expect("nonempty").0.input_id.clone();
        // Persist the actual carrier (the last unread input, or the draft) before IO.
        let batch = unread
            .iter()
            .map(|(record, _)| {
                let mut details = record.details.clone();
                details[MARK]["carrierInputId"] = json!(carrier_id);
                let mut next = Next::sending()
                    .reason(isend::SENDING_REASON)
                    .details(details);
                // The stopped turn proved this owner input unread. Its former
                // external bundle retains separate native/source custody; it
                // was NOT included in ownerInputs of the new turn (057 §6).
                next.carries = Some(Vec::new());
                (record.input_id.clone(), Expect::rev(record.rev), next)
            })
            .collect();
        for transition in ledger
            .transition_batch(session, batch)
            .map_err(ledger_failure)?
        {
            ops.publish(&transition);
        }
        for (record, _) in &unread {
            let prior = ops
                .repository()
                .delivery(session, &record.input_id)
                .map_err(queue_persistence_error)?;
            let sending = InputDelivery::Sending {
                thread_id: now.thread_id.clone(),
                since: Utc::now().to_rfc3339(),
            };
            let reservation = ops.reserve(session, &record.input_id, prior.as_ref(), sending);
            if matches!(reservation, Ok(Reservation::Busy)) {
                defer_known(
                    ops,
                    session,
                    &now.thread_id,
                    &unread,
                    "host_busy",
                    true,
                    None,
                )?;
                let mut result = outcome(false, Some(carrier_id), read, vec![]);
                result.outcome = Outcome::Held {
                    turn_id: Some(turn.clone()),
                };
                return Ok((
                    result,
                    Some(super::super::super::owner_admission::held_execution()),
                ));
            }
            if !matches!(reservation, Ok(Reservation::Reserved)) {
                for (record, _) in &unread {
                    quarantine(&record.input_id, "outcomeUnknown", None);
                }
                return Ok((outcome(false, Some(carrier_id), read, vec![]), None));
            }
        }
        let prefix = unread
            .iter()
            .take(unread.len() - 1)
            .map(|(record, input)| {
                json!({
        "clientUserMessageId":record.input_id,"input":input.params.input})
            })
            .collect::<Vec<_>>();
        let carrier = &unread.last().expect("nonempty").1;
        let command = UserInputSubmitCommand {
            management_request_id: carrier.request_id.clone(),
            cutex_session_id: session.into(),
            thread_id: now.thread_id.clone(),
            params: message_for_new_turn(&carrier.params),
        };
        let result = ops.send_batch(&command, &prefix, now.generation);
        match result {
            Ok(execution)
                if execution.app_server_accepted
                    && execution.turn_id.as_ref().is_some_and(|id| !id.is_empty()) =>
            {
                let mut submitted = Vec::new();
                for (record, input) in &unread {
                    let prior = ops
                        .repository()
                        .delivery(session, &record.input_id)
                        .map_err(queue_persistence_error)?;
                    ops.repository()
                        .set_delivery(
                            session,
                            &record.input_id,
                            prior.as_ref(),
                            Some(InputDelivery::Accepted {
                                thread_id: now.thread_id.clone(),
                                turn_id: execution.turn_id.clone(),
                                accepted_at: Utc::now().to_rfc3339(),
                            }),
                        )
                        .map_err(queue_persistence_error)?;
                    let current = latest(ledger, session, &record.input_id)?;
                    if current.state == InputState::Sending {
                        step(
                            ops,
                            session,
                            &record.input_id,
                            current.rev,
                            Next::in_context(Evidence::TurnStartInput)
                                .turn(execution.turn_id.clone().expect("checked")),
                        );
                    }
                    let mark = json!({"interruptedTurnId":turn});
                    let flow = Flow {
                        ops,
                        session,
                        id: &record.input_id,
                        thread: &now.thread_id,
                        turn,
                        input,
                        mark: &mark,
                    };
                    let individual = UserInputSubmitCommand {
                        params: input.params.clone(),
                        ..command.clone()
                    };
                    flow.sent(&individual, &execution);
                    submitted.push(record.input_id.clone());
                }
                Ok((
                    outcome(true, Some(carrier_id), read, submitted),
                    Some(execution),
                ))
            }
            Err(error) if !error.outcome_unknown && not_submitted_reason(&error).is_some() => {
                let reason = not_submitted_reason(&error).expect("guard checked marker");
                let automatic = matches!(reason, "not_idle" | "pending_trigger_turn" | "plan_mode");
                let native_error = error
                    .details
                    .get("nativeError")
                    .cloned()
                    .unwrap_or_else(|| json!({"code":error.code,"message":error.message}));
                defer_known(
                    ops,
                    session,
                    &now.thread_id,
                    &unread,
                    reason,
                    automatic,
                    Some(&native_error),
                )?;
                let mut result = outcome(false, Some(carrier_id), read, vec![]);
                result.outcome = Outcome::Held {
                    turn_id: Some(turn.clone()),
                };
                Ok((
                    result,
                    automatic.then(super::super::super::owner_admission::held_execution),
                ))
            }
            Err(error) if !error.outcome_unknown => {
                // Refusing this request does not prove an earlier attempt of
                // these IDs was never admitted (Native intent / Claude history).
                // Do not offer manual Retry without positive unsent evidence.
                for (record, _) in &unread {
                    quarantine(&record.input_id, "outcomeUnknown", Some(&error));
                    ops.held_finish(session, &record.input_id)
                        .map_err(queue_persistence_error)?;
                }
                Ok((outcome(false, Some(carrier_id), read, vec![]), None))
            }
            _ => {
                for (record, _) in &unread {
                    quarantine(&record.input_id, "outcomeUnknown", None);
                    let _ = ops.held_finish(session, &record.input_id);
                }
                Ok((outcome(false, Some(carrier_id), read, vec![]), None))
            }
        }
    })();
    match owned_result {
        Ok(result) => Ok(result),
        Err(error) => {
            for id in &ids {
                quarantine(id, "outcomeUnknown", None);
            }
            let mut unknown = execution_error_message(
                "interrupt",
                "request_outcome_unknown",
                &format!(
                    "owner batch custody changed; reconcile original IDs: {}",
                    error.message
                ),
                false,
                true,
            );
            unknown.details = json!({"cause":error.code,"inputIds":ids});
            Err(unknown)
        }
    }
}

pub(crate) fn interrupt_send_many_v2(
    session: &str,
    _thread: &str,
    request: &InterruptSendMany,
) -> Result<ManyOutcome, UserInputExecutionError> {
    cutex::management::v2::input_disposal::check(
        session,
        request.input_ids.iter().map(String::as_str),
    )
    .map_err(|e| execution_error("route", "input_disposed", e, false, false))?;
    let lane = session_input_lock(session)
        .map_err(|e| execution_error("route", "internal_error", e, true, false))?;
    let _lane = lane.lock().map_err(|_| {
        execution_error_message(
            "route",
            "internal_error",
            "input lane poisoned",
            true,
            false,
        )
    })?;
    super::super::super::management_archive::api::ensure_input_allowed(session)
        .map_err(|e| execution_error("route", "archive_blocks_input", e, false, false))?;
    let repository = user_input_repository()
        .map_err(|e| execution_error("route", "event_persistence_unavailable", e, true, false))?;
    run_many(&Live { repository }, session, request, None).map(|(result, _)| result)
}

pub(crate) fn submit_many_v2(
    command: &UserInputSubmitCommand,
    repository: &UserInputRepository,
) -> Result<UserInputSubmitExecution, UserInputExecutionError> {
    let request = command
        .params
        .interrupt_send
        .as_ref()
        .expect("caller selected batch route");
    let (result, execution) = run_many(
        &Live { repository },
        &command.cutex_session_id,
        request,
        Some(command),
    )?;
    if execution.is_none() {
        let ledger = il::default_ledger().map_err(|e| {
            execution_error(
                "post_submit",
                "event_persistence_unavailable",
                e,
                false,
                true,
            )
        })?;
        let record = latest(
            ledger,
            &command.cutex_session_id,
            &command.params.client_user_message_id,
        )?;
        if record.state == InputState::Waiting && record.reason.as_deref() == Some("nativeHeld") {
            return Err(execution_error_message(
                "native_request",
                "owner_batch_not_submitted",
                "runtime did not admit the owner batch; original inputs retained, resolve the blocking condition before retry",
                false,
                false,
            ));
        }
    }
    execution.ok_or_else(|| {
        execution_error_message(
            "interrupt",
            "request_outcome_unknown",
            &format!(
                "owner batch not confirmed started ({})",
                result.outcome.wire()
            ),
            false,
            true,
        )
    })
}

/// Only the authenticated admission worker may resume retained group custody.
/// An HTTP replay keeps using the immutable original selection and does no IO.
pub(crate) fn resume_held_v2(
    command: &UserInputSubmitCommand,
    repository: &UserInputRepository,
) -> Option<Result<UserInputSubmitExecution, UserInputExecutionError>> {
    if !super::super::super::owner_admission::on_worker_thread() {
        return None;
    }
    let ops = Live { repository };
    let ledger = ops.ledger().ok()?;
    let head = latest(
        ledger,
        &command.cutex_session_id,
        &command.params.client_user_message_id,
    )
    .ok()?;
    if head.state != InputState::Held || head.details[MARK]["deferred"] != true {
        return None;
    }
    Some((|| {
        let ids = head.details[MARK]["deferredIds"]
            .as_array()
            .ok_or_else(|| isend::not_interruptible(&head))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| isend::not_interruptible(&head))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Check before live/capability gates too: an incomplete group must not
        // linger forever merely because the runtime is currently disconnected.
        if block_incomplete_group(
            &ops,
            &command.cutex_session_id,
            &ids,
            &head.details[MARK]["identity"],
        )? {
            return Err(execution_error_message(
                "route",
                "owner_batch_not_submitted",
                "owner batch group is incomplete; remaining inputs retained for manual recovery",
                false,
                false,
            ));
        }
        let Some(live) = ops.live(&command.cutex_session_id) else {
            return Ok(super::super::super::owner_admission::held_execution());
        };
        if !ops.supports_owner_batch(&command.cutex_session_id, live.generation) {
            return Ok(super::super::super::owner_admission::held_execution());
        }
        let mut revs = std::collections::BTreeMap::new();
        for id in &ids {
            let record = latest(ledger, &command.cutex_session_id, id)?;
            revs.insert(id.clone(), record.rev);
        }
        let request = InterruptSendMany {
            input_ids: ids,
            expected_revs: revs,
            expected_turn_id: live.active_turn.unwrap_or_else(|| {
                head.details[MARK]["identity"]["request"]["expectedTurnId"]
                    .as_str()
                    .unwrap_or_default()
                    .into()
            }),
            expected_runtime_generation: live.generation,
        };
        let (_, execution) = run_many_mode(
            &ops,
            &command.cutex_session_id,
            &request,
            None,
            Some(&head.details[MARK]["identity"]),
        )?;
        execution.ok_or_else(|| {
            execution_error_message(
                "native_request",
                "owner_batch_not_submitted",
                "owner batch retained without automatic retry; resolve its blocking condition",
                false,
                false,
            )
        })
    })())
}

#[cfg(test)]
#[path = "app_server_interrupt_many_tests.rs"]
mod tests;
