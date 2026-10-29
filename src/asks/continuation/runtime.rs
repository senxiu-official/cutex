//! AskState/observer/scheduler wiring for bounded continuation. Runtime IO and
//! actual attempt/lease ownership stay in the existing asks scheduler.
use super::*;
use crate::asks::model::{
    self, AskRecord, AttemptState, ExecutionState, LeaseHolder, QuestionState, ResumeReason,
    ResumeRecord,
};
use crate::asks::scheduler::{AskRuntime, SchedulerWorld, TargetStatus};
use crate::asks::store::{AskError, AskState, AskStore};
use serde_json::{json, Value};

// These describe the authenticated bridges implemented by this daemon.
// Native child presentation events do not yet prove immutable terminal facts.
pub const WAKE_KINDS: &[&str] = &["after", "children"];
pub const CHILD_KINDS: &[&str] = &["delegation"];

pub fn ask_error(error: Error) -> AskError {
    AskError::new(
        error.code(),
        format!("bounded continuation: {}", error.code()),
    )
}

pub fn current_director(ask: &AskRecord) -> Option<String> {
    let snapshot = crate::asks::directory::Directory::director_snapshot().ok()?;
    let project = crate::agent_management::ProjectId::new(ask.project_id.clone()?).ok()?;
    snapshot
        .projects
        .get(&project)
        .map(|authority| authority.authorized_director_session.as_str().to_owned())
}

pub fn attempt_identity(ask: &AskRecord, attempt: u32) -> Option<AttemptIdentity> {
    let record = ask.attempts.iter().find(|record| record.n == attempt)?;
    Some(AttemptIdentity {
        delegation_id: ask.ask_id.clone(),
        attempt_id: format!("{}#{}", ask.ask_id, attempt),
        owner_session_id: ask.target_cutex_session_id.clone(),
        runtime_generation: record.runtime_generation,
        parent_turn_id: record.turn_id.clone()?,
    })
}

fn exact_lease(state: &AskState, context: &AttemptIdentity) -> bool {
    if state
        .presence
        .get(&context.owner_session_id)
        .and_then(|p| p.runtime_generation)
        .is_some_and(|generation| generation != context.runtime_generation)
    {
        return false;
    }
    state.leases.get(&context.owner_session_id).is_some_and(|lease| {
        lease.runtime_generation == context.runtime_generation && lease.turn_id.as_deref() == Some(&context.parent_turn_id)
            && matches!(&lease.holder, LeaseHolder::Ask { ask_id, attempt }
                if ask_id == &context.delegation_id && format!("{ask_id}#{attempt}") == context.attempt_id)
    })
}

pub fn projection(data: &ContinuationData) -> Value {
    let Some(record) = &data.current else {
        return Value::Null;
    };
    json!({"state": record.state, "roundNumber": record.round_number,
        "budgetEpoch": record.budget_epoch, "startedAt": record.started_at,
        "deadlineAt": record.deadline_at, "maxRounds": MAX_ROUNDS,
        "maxElapsedSeconds": MAX_ELAPSED_SECONDS, "waitingFor": record.declaration.wake,
        "nextWakeAt": record.next_wake_at, "sourceAttemptId": record.declaration.source.attempt_id,
        "revision": record.revision, "limitReason": record.limit_reason})
}

/// Tool/HTTP entry. Caller and guards are authenticated server values; caller
/// cannot name another handler or supply a context/generation/turn in JSON.
/// Existing progress text/ack path calls this only when continuation is present.
pub fn declare_current(
    state: &mut AskState,
    caller: &str,
    ask_id: &str,
    request: &DeclareRequest,
    mut guards: Guards,
    now: DateTime<Utc>,
    current_director: Option<&str>,
) -> Result<Value, AskError> {
    let ask = state
        .asks
        .get(ask_id)
        .ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    if ask.remote.is_some() || ask.target_cutex_session_id != caller {
        return Err(ask_error(Error::NotCurrentHandler));
    }
    // Lost-response replay may arrive after the original turn ended. It may
    // return only its own immutable receipt, never mutate the new occurrence.
    let actor = Actor::Agent {
        session_id: caller.into(),
    };
    let action = ActionRequest::Declare {
        request: request.clone(),
    };
    if let Some(receipt) = ask
        .bounded_continuation
        .replay(&actor, &request.action_id, &action)
        .map_err(ask_error)?
    {
        return Ok(
            json!({"accepted": true, "continuation": projection_record(&receipt.continuation), "replayed": true}),
        );
    }
    if !ask.is_open() {
        return Err(ask_error(Error::NotCurrentHandler));
    }
    let context = ask
        .current_attempt()
        .and_then(|attempt| {
            matches!(
                attempt.state,
                AttemptState::Admitted | AttemptState::Running
            )
            .then(|| attempt_identity(ask, attempt.n))
            .flatten()
        })
        .ok_or_else(|| ask_error(Error::NotCurrentHandler))?;
    if !exact_lease(state, &context) || request.expected_attempt_id != context.attempt_id {
        return Err(ask_error(Error::NotCurrentHandler));
    }
    guards.stopped |= ask
        .current_attempt()
        .is_some_and(|attempt| attempt.stop_requested)
        || ancestor_stopped(state, ask);
    guards.running_turn = true;
    let awaiting = ask
        .awaiting
        .as_ref()
        .is_some_and(|wait| ask.current_attempt().is_some_and(|a| a.n == wait.attempt));
    let child_facts=super::delegation_children::collect(state,&context,request.wake.as_ref()).map_err(ask_error)?;
    let receipt = state
        .update_ask(ask_id, now, |ask| {
            let mut data=ask.bounded_continuation.clone();
            super::delegation_children::apply(&mut data,child_facts)?;
            let result=data.declare(
                &actor,
                &context,
                request,
                awaiting,
                guards,
                ConditionSupport{native_children:false,delegation_children:true},
                now,
            )?;
            ask.bounded_continuation=data;
            Ok(result)
        })
        .ok_or_else(|| ask_error(Error::NotCurrentHandler))?
        .map_err(ask_error)?;
    changed_and_notice(state, ask_id, current_director, now);
    Ok(
        json!({"accepted": true, "continuation": projection_record(&receipt.continuation), "replayed": receipt.replayed}),
    )
}

fn projection_record(record: &ContinuationRecord) -> Value {
    projection(&ContinuationData {
        current: Some(record.clone()),
        ..ContinuationData::default()
    })
}

/// Parent authenticates actor/current Director and supplies its authority fence.
pub fn resume_limited(
    state: &mut AskState,
    ask_id: &str,
    authority: Authority<'_>,
    request: &ResetRequest,
    mut guards: Guards,
    now: DateTime<Utc>,
) -> Result<Value, AskError> {
    let ask = state
        .asks
        .get(ask_id)
        .ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    guards.stopped |= !ask.is_open()
        || ancestor_stopped(state, ask)
        || ask.current_attempt().is_some_and(|a| a.stop_requested);
    guards.paused |= state.pauses.contains_key(&ask.target_cutex_session_id)
        || crate::asks::agent_pause::is_paused(state, &ask.target_cutex_session_id);
    guards.running_turn |= state.leases.contains_key(&ask.target_cutex_session_id);
    let director = authority.current_director.map(str::to_owned);
    let receipt = state
        .update_ask(ask_id, now, |ask| {
            ask.bounded_continuation
                .reset_budget(authority, request, guards, now)
        })
        .ok_or_else(|| ask_error(Error::NotLimited))?
        .map_err(ask_error)?;
    changed_and_notice(state, ask_id, director.as_deref(), now);
    Ok(
        json!({"accepted": true, "continuation": projection_record(&receipt.continuation), "replayed": receipt.replayed}),
    )
}

/// Called only after observe's exact occurrence recheck under the write lock.
/// Returns true if the existing answer finalizer must stop (stage/no-op).
pub fn finish_turn(
    state: &mut AskState,
    context: &AttemptIdentity,
    end: EndKind,
    artifact: Option<String>,
    preview: Option<String>,
    now: DateTime<Utc>,
    director: Option<&str>,
) -> bool {
    let Some(ask) = state.asks.get(&context.delegation_id) else {
        return false;
    };
    if ask.bounded_continuation.current.is_none() {
        return false;
    }
    if !exact_lease(state, context)
        || ask
            .current_attempt()
            .is_none_or(|attempt| format!("{}#{}", ask.ask_id, attempt.n) != context.attempt_id)
    {
        return false;
    }
    let stopped = !ask.is_open()
        || ancestor_stopped(state, ask)
        || ask.current_attempt().is_some_and(|a| a.stop_requested);
    let guards = Guards {
        stopped,
        occurrence_valid: true,
        ..Guards::default()
    };
    let disposition = state
        .update_ask(&context.delegation_id, now, |ask| {
            ask.bounded_continuation
                .end_turn(context, end, artifact.clone(), guards, now, director)
        })
        .unwrap_or(EndDisposition::DefaultAnswer);
    if disposition == EndDisposition::StageNote {
        state.update_ask(&context.delegation_id, now, |ask| {
            if let Some(attempt) = ask.current_attempt_mut() {
                attempt.state = AttemptState::Yielded;
                attempt.ended_at = Some(crate::asks::now_rfc3339(now));
                attempt.stage_note = preview;
                attempt.compaction_reminder_pending = false;
                if artifact.is_some() {
                    attempt.last_message_artifact = artifact;
                }
            }
            ask.execution = ExecutionState::Running;
            ask.hold = None;
            ask.awaiting = None;
            ask.resume = None;
        });
        for question in state
            .questions
            .values_mut()
            .filter(|q| q.ask_id == context.delegation_id && q.state == QuestionState::Open)
        {
            if format!("{}#{}", question.ask_id, question.attempt) == context.attempt_id {
                question.state = QuestionState::Stale;
            }
        }
        // exact_lease was checked in this transaction; never release another
        // delegation's new lease in response to this old turn.
        state.leases.remove(&context.owner_session_id);
    }
    changed_and_notice(state, &context.delegation_id, director, now);
    matches!(
        disposition,
        EndDisposition::StageNote | EndDisposition::AlreadyStaged
    )
}

pub fn ancestor_stopped(state: &AskState, ask: &AskRecord) -> bool {
    if crate::asks::lineage::check_admission(state,&crate::asks::routing_policy::integration::local_host_id(),&ask.ask_id).is_err(){return true;}
    let mut parent = ask.parent_ask.as_deref();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = parent {
        if !seen.insert(id) {
            return true;
        }
        let Some(record) = state.asks.get(id) else {
            return true;
        };
        if record.closed == Some(model::CloseOutcome::Stopped)
            || record.current_attempt().is_some_and(|a| a.stop_requested)
        {
            return true;
        }
        parent = record.parent_ask.as_deref();
    }
    false
}

pub fn guards_for(
    state: &AskState,
    ask: &AskRecord,
    status: &TargetStatus,
    archived: bool,
    permitted: bool,
) -> Guards {
    Guards {
        stopped: !ask.is_open()
            || ask.current_attempt().is_some_and(|a| a.stop_requested)
            || ancestor_stopped(state, ask),
        archived,
        paused: state.pauses.contains_key(&ask.target_cutex_session_id)
            || crate::asks::agent_pause::is_paused(state, &ask.target_cutex_session_id)
            || status.plan_mode
            || status.profile_switch_pending
            || status.blocked.is_some()
            || !status.exclusive_admission,
        online: status.online,
        permission_valid: permitted,
        occurrence_valid: ask.current_attempt().is_some(),
        running_turn: status.active_turn_id.is_some()
            || state.leases.contains_key(&ask.target_cutex_session_id),
        submission_unknown: ask
            .current_attempt()
            .is_some_and(|a| a.state == AttemptState::Submitting),
        restarting: !status.online && status.reconnectable,
    }
}

/// Run before the existing target queue scan. Ready continuations enter the
/// existing ResumeRecord lane; they never launch an offline runtime.
pub fn tick_due(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let candidates: Vec<AskRecord> = store.inspect(|state| {
        state
            .asks
            .values()
            .filter(|ask| {
                ask.remote.is_none()
                    && ask
                        .bounded_continuation
                        .current
                        .as_ref()
                        .is_some_and(|r| r.state != Phase::Cancelled)
            })
            .cloned()
            .collect()
    })?;
    if candidates.is_empty() {
        return Ok(());
    }
    let directory = world.directory().ok();
    for snapshot in candidates {
        let status = runtime.target_status(&snapshot.target_cutex_session_id);
        let director = world.continuation_director(&snapshot);
        let archived = directory.as_ref().is_some_and(|d| {
            d.get(&snapshot.target_cutex_session_id)
                .is_none_or(|a| a.archived)
        });
        let permitted = admission_permission(world, directory.as_ref(), &snapshot);
        store.with_state(|state| {
            let Some(ask) = state.asks.get(&snapshot.ask_id) else {
                return;
            };
            if ask.revision != snapshot.revision {
                return;
            }
            let guards = guards_for(state, ask, &status, archived, permitted);
            let before = ask.bounded_continuation.clone();
            let facts=ask.bounded_continuation.current.as_ref().map(|record|
                super::delegation_children::collect(state,&record.declaration.source,record.declaration.wake.as_ref()));
            let eligibility = state.update_ask(&snapshot.ask_id, now, |ask| {
                if let Some(Ok(facts))=facts {
                    // Keep old trustworthy facts on a later unavailable read.
                    let mut next=ask.bounded_continuation.clone();
                    if super::delegation_children::apply(&mut next,facts).is_ok(){ask.bounded_continuation=next;}
                }
                ask.bounded_continuation
                    .evaluate(guards, now, director.as_deref())
            });
            state.update_ask(&snapshot.ask_id, now, |ask| match eligibility {
                Some(Eligibility::Ready) if ask.resume.is_none() => {
                    let source = &ask
                        .bounded_continuation
                        .current
                        .as_ref()
                        .expect("ready record")
                        .declaration;
                    let from_attempt = ask
                        .attempts
                        .iter()
                        .find(|a| format!("{}#{}", ask.ask_id, a.n) == source.source.attempt_id)
                        .map(|a| a.n);
                    if let Some(from_attempt) = from_attempt {
                        ask.resume = Some(ResumeRecord {
                            reason: ResumeReason::Resumed,
                            since: crate::asks::now_rfc3339(now),
                            from_attempt,
                            jobs: vec![],
                            by: None,
                            note: Some(source.note.clone()),
                        });
                        ask.hold = None;
                    }
                }
                Some(Eligibility::Limited | Eligibility::Cancelled) => {
                    ask.resume = None;
                }
                _ => {}
            });
            if state
                .asks
                .get(&snapshot.ask_id)
                .is_some_and(|ask| ask.bounded_continuation != before)
            {
                changed_and_notice(state, &snapshot.ask_id, director.as_deref(), now);
            }
        })?;
    }
    Ok(())
}

/// Continuations recheck the same boundary and capability snapshot as normal
/// admission. An earlier approval covers only the unchanged snapshot.
pub fn admission_permission(
    world: &dyn SchedulerWorld,
    directory: Option<&crate::asks::directory::Directory>,
    ask: &AskRecord,
) -> bool {
    let Some(directory) = directory else {
        return false;
    };
    let Ok(controller) = ask.controller() else {
        return false;
    };
    if controller
        .as_deref()
        .is_some_and(|caller| !directory.may_interact(caller, &ask.target_cutex_session_id))
    {
        return false;
    }
    let Ok(inputs) = crate::peers::asks::gate_inputs(world, ask) else {
        return false;
    };
    let evaluated = crate::asks::gate::evaluate(&crate::asks::gate::ask_facts(ask), &inputs);
    !evaluated.required
        || ask.gate.as_ref().is_some_and(|old| {
            old.admitted_with_approval && old.snapshot_sha256 == evaluated.snapshot_sha256
        })
}

pub fn bounded_wake(ask: &AskRecord) -> Option<String> {
    let current = ask.bounded_continuation.current.as_ref()?;
    (current.state == Phase::Ready)
        .then(|| current.wake.as_ref().map(|w| w.id.clone()))
        .flatten()
}

pub fn accepted(ask: &mut AskRecord, attempt_n: u32, turn_id: &str) {
    let Some(record) = ask.attempts.iter().find(|a| a.n == attempt_n) else {
        return;
    };
    let Some(wake_id) = record
        .continuation
        .as_ref()
        .and_then(|c| c.bounded_wake_id.clone())
    else {
        return;
    };
    let message = record.native_message_id.clone();
    let generation = record.runtime_generation;
    let context = AttemptIdentity {
        delegation_id: ask.ask_id.clone(),
        attempt_id: format!("{}#{attempt_n}", ask.ask_id),
        owner_session_id: ask.target_cutex_session_id.clone(),
        runtime_generation: generation,
        parent_turn_id: turn_id.into(),
    };
    let _ = ask
        .bounded_continuation
        .admission_accepted(&wake_id, &context, &message);
}

pub fn refused(ask: &mut AskRecord, attempt_n: u32) {
    let Some(record) = ask.attempts.iter().find(|a| a.n == attempt_n) else {
        return;
    };
    let Some(wake_id) = record
        .continuation
        .as_ref()
        .and_then(|c| c.bounded_wake_id.clone())
    else {
        return;
    };
    let message = record.native_message_id.clone();
    let _ = ask
        .bounded_continuation
        .admission_definitely_refused(&wake_id, &message);
}

pub fn unknown(ask: &mut AskRecord) {
    if let Some(wake) = ask
        .bounded_continuation
        .current
        .as_ref()
        .and_then(|r| r.wake.as_ref())
        .map(|w| w.id.clone())
    {
        let _ = ask.bounded_continuation.submission_unknown(&wake);
    }
}

pub fn changed_and_notice(
    state: &mut AskState,
    ask_id: &str,
    director: Option<&str>,
    now: DateTime<Utc>,
) {
    let notice = state
        .update_ask(ask_id, now, |ask| {
            ask.bounded_continuation.enforce_limits(now, director);
            ask.bounded_continuation.enqueue_limit_notice()
        })
        .flatten();
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return;
    };
    state.notice(
        "cutex/delegation/continuationChanged",
        AskState::ask_sessions(&ask),
        json!({"projectId": ask.project_id, "resourceId": ask_id, "revision": ask.revision}),
    );
    if let Some(notice) = notice {
        let mut sessions = vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()];
        if let Some(director) = &notice.director_session_id {
            if !sessions.contains(director) {
                sessions.push(director.clone());
            }
        }
        // A notification is an invalidation, not a tree read. Child identities
        // require the recipient-specific visibility projection on a later GET.
        let mut summary=projection(&ask.bounded_continuation);
        summary["waitingFor"]=Value::Null;
        state.notice("cutex/delegation/continuationLimited", sessions,
            json!({"noticeId": notice.id, "projectId": ask.project_id, "resourceId": ask_id,
                "budgetEpoch": notice.budget_epoch, "reason": notice.reason, "continuation": summary}));
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
