//! Delegation await (cutex-desktop `docs/research/delegation-await.md`): a
//! handler yields its turn with `delegation_await` while Job Service jobs
//! bound to the delegation run; their completions wake it into a
//! continuation attempt of the same delegation.
//!
//! - Binding (§2.3): `cutex_job.submit` results seen in the delegation's
//!   own turn ([`observe_item`]) or ids named in `delegation_await.jobs`.
//! - The wait is registered in the running attempt ([`register`]) and takes
//!   effect when that turn ends (`observe::finalize` → [`yield_attempt`]):
//!   the attempt is `yielded`, execution stays `running` with hold
//!   `waitingForEvent`, and the lease is released.
//! - Completions are noted by the bus bridge before it freezes the native
//!   input ([`note_job_completion`]); awaited ones go in `passive`, so the
//!   continuation turn (N1 exclusive, which drains passive items only) is
//!   the one that reads them.
//! - [`wake`] turns a met wait (a failure, all/any, each, timeout) into a
//!   `resume`; the scheduler admits the continuation like a queued ask,
//!   ahead of the target's queue. [`resume_now`] and [`reopen`] do the same
//!   on request of the requester or the owner.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::store::{AskError, AskState, AskStore};
use super::{now_rfc3339, parse_time, truncate_chars};

/// The Job Service MCP server name in agent configs.
pub const JOB_SERVER: &str = "cutex_job";
pub const MIN_TIMEOUT_SECS: u64 = 60;
pub const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;
/// Jobs bound to one delegation.
pub const MAX_JOBS: usize = 64;
/// Stage notes and resume / reopen / changes_requested notes are accepted
/// up to this many characters and kept whole; a longer note is refused
/// (`note_too_long`), never clipped.
pub const NOTE_MAX_CHARS: usize = 16_000;
/// A resume / reopen note up to this many characters reaches the handler
/// inline; a longer one (or one that does not fit the receiver's budget)
/// arrives as a preview plus an explicit `delegation_read` pointer to the
/// full note (an artifact of the delegation, `render::note_part`).
pub const NOTE_INLINE_CHARS: usize = 4000;
/// `delegation_progress` texts and revision acks: kept whole up to this many
/// characters, longer is refused (`note_too_long`).
pub const PROGRESS_MAX_CHARS: usize = 1000;
/// A `stop_jobs` cancel the handler's runtime could not run gives up after
/// this long.
pub const CANCEL_GIVE_UP_HOURS: i64 = 24;

pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

/// The structured result of an MCP tool call item or response:
/// `structuredContent`, else the JSON of its first text block.
pub fn structured_result(result: &Value) -> Option<Value> {
    if let Some(structured) = result.get("structuredContent").filter(|value| !value.is_null()) {
        return Some(structured.clone());
    }
    result.get("content")?.as_array()?.iter().find_map(|block| {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        serde_json::from_str::<Value>(block.get("text")?.as_str()?).ok()
    })
}

/// The job id (and the model's `actionId`) of a successful
/// `cutex_job.submit` thread item (`mcpToolCall`).
pub fn submitted_job(item: &Value) -> Option<(String, Option<String>)> {
    if item.get("type").and_then(Value::as_str) != Some("mcpToolCall")
        || item.get("server").and_then(Value::as_str) != Some(JOB_SERVER)
        || item.get("tool").and_then(Value::as_str) != Some("submit")
        || item.get("status").and_then(Value::as_str).is_some_and(|status| status != "completed")
    {
        return None;
    }
    let structured = structured_result(item.get("result")?)?;
    let job = structured.get("job")?;
    let job_id = job.get("jobId").and_then(Value::as_str).filter(|id| valid_job_id(id))?.to_string();
    let action_id = job
        .pointer("/request/actionId")
        .or_else(|| item.pointer("/arguments/actionId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((job_id, action_id))
}

/// Bind `job_id` to `ask` (no-op when bound). False when the delegation
/// already has [`MAX_JOBS`] jobs.
pub fn bind(ask: &mut AskRecord, job_id: &str, source: JobSource, attempt: u32, action_id: Option<String>, now: DateTime<Utc>) -> bool {
    if ask.job(job_id).is_some() {
        return true;
    }
    if ask.jobs.len() >= MAX_JOBS {
        return false;
    }
    ask.jobs.push(JobBinding {
        job_id: job_id.to_string(),
        source,
        attempt,
        bound_at: now_rfc3339(now),
        action_id,
        terminal: None,
        cancel: None,
    });
    true
}

/// Auto-binding (§2.3): a `cutex_job.submit` result in the leased turn of
/// `attempt` binds its job. Returns the bound job id.
pub fn observe_item(state: &mut AskState, ask_id: &str, attempt: u32, item: &Value, now: DateTime<Utc>) -> Option<String> {
    let (job_id, action_id) = submitted_job(item)?;
    let bound = state
        .update_ask(ask_id, now, |ask| bind(ask, &job_id, JobSource::Auto, attempt, action_id, now))
        .unwrap_or(false);
    if !bound {
        eprintln!("asks: delegation {ask_id} has {MAX_JOBS} jobs; job {job_id} is not bound");
        return None;
    }
    Some(job_id)
}

/// A job's terminal outcome as completion v2 reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobOutcome {
    /// `exited`, `failed`, `cancelled`, `interrupted`, `launchUnknown`.
    pub status: String,
    pub exit_code: Option<i64>,
}

impl JobOutcome {
    pub fn ok(&self) -> bool {
        self.status == "exited" && self.exit_code == Some(0)
    }

    /// From a completion v2 request (`agent_bus::job_completion`).
    pub fn of(completion: &crate::agent_bus::job_completion::CompletionV2) -> Self {
        let status = serde_json::to_value(&completion.terminal_status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        Self { status, exit_code: completion.facts.exit_code.map(i64::from) }
    }
}

/// Record a job's completion on every delegation of `session` it is bound
/// to (once per event) and wake met waits. Returns whether the completion
/// should go to the handler `passive`: an open delegation awaits the job,
/// or its cancellation was requested with `stop_jobs` (§2.4).
pub fn note_job_completion(state: &mut AskState, session: &str, job_id: &str, event_id: &str, outcome: &JobOutcome, now: DateTime<Utc>) -> bool {
    let ids: Vec<String> = state
        .asks
        .values()
        .filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == session) && ask.job(job_id).is_some())
        .map(|ask| ask.ask_id.clone())
        .collect();
    let mut passive = false;
    for ask_id in ids {
        state.update_ask(&ask_id, now, |ask| {
            if let Some(job) = ask.jobs.iter_mut().find(|job| job.job_id == job_id) {
                if job.terminal.is_none() {
                    job.terminal = Some(JobTerminal {
                        status: outcome.status.clone(),
                        exit_code: outcome.exit_code,
                        event_id: event_id.to_string(),
                        at: now_rfc3339(now),
                        ok: outcome.ok(),
                    });
                }
            }
        });
        let Some(ask) = state.asks.get(&ask_id) else {
            continue;
        };
        let awaited = ask.is_open() && ask.awaiting.as_ref().is_some_and(|wait| wait.jobs.iter().any(|id| id == job_id));
        let cancelled = ask.job(job_id).is_some_and(|job| job.cancel.is_some());
        passive |= awaited || cancelled;
        wake(state, &ask_id, now);
    }
    passive
}

/// Whether the wait of a waiting ask is met, and why.
pub fn due(ask: &AskRecord, now: DateTime<Utc>) -> Option<ResumeReason> {
    let wait = ask.awaiting.as_ref()?;
    let done: Vec<&JobBinding> = wait
        .jobs
        .iter()
        .filter_map(|id| ask.job(id))
        .filter(|job| job.is_terminal())
        .collect();
    if done.iter().any(|job| job.terminal.as_ref().is_some_and(|terminal| !terminal.ok)) {
        return Some(ResumeReason::JobFailed);
    }
    let met = match wait.until {
        AwaitUntil::All => done.len() >= wait.jobs.len(),
        AwaitUntil::Any => !done.is_empty(),
    };
    if met || (wait.resume == AwaitResume::Each && !done.is_empty()) {
        return Some(ResumeReason::Jobs);
    }
    if wait.timeout_at.as_deref().and_then(parse_time).is_some_and(|at| now >= at) {
        return Some(ResumeReason::Timeout);
    }
    None
}

fn to_resume(ask: &mut AskRecord, reason: ResumeReason, by: Option<ResumeBy>, note: Option<String>, now: DateTime<Utc>) {
    let wait = ask.awaiting.take();
    let from_attempt = wait
        .as_ref()
        .map(|wait| wait.attempt)
        .or_else(|| ask.current_attempt().map(|attempt| attempt.n))
        .unwrap_or(0);
    let jobs = wait.map(|wait| wait.jobs).unwrap_or_else(|| ask.jobs.iter().map(|job| job.job_id.clone()).collect());
    ask.resume = Some(ResumeRecord { reason, since: now_rfc3339(now), from_attempt, jobs, by, note });
    ask.hold = None;
    // The next attempt gets a fresh submit-retry budget (a reopened
    // `submit_failed` delegation is not failed again at once).
    ask.submit_retry = None;
}

/// Wake a waiting ask whose wait is met. Returns the reason.
pub fn wake(state: &mut AskState, ask_id: &str, now: DateTime<Utc>) -> Option<ResumeReason> {
    let ask = state.asks.get(ask_id)?;
    if !ask.is_waiting() {
        return None;
    }
    let reason = due(ask, now)?;
    state.update_ask(ask_id, now, |ask| to_resume(ask, reason, None, None, now));
    Some(reason)
}

/// Wake every waiting ask whose wait is met (the scheduler's timeouts and
/// completions noted while the ask was still running). Returns the woken.
pub fn wake_due(state: &mut AskState, now: DateTime<Utc>) -> Vec<String> {
    let waiting: Vec<String> = state
        .asks
        .values()
        .filter(|ask| ask.is_waiting() && due(ask, now).is_some())
        .map(|ask| ask.ask_id.clone())
        .collect();
    waiting.into_iter().filter(|ask_id| wake(state, ask_id, now).is_some()).collect()
}

/// Any waiting ask with a due wake (cheap read before a locked write).
pub fn any_due(state: &AskState, now: DateTime<Utc>) -> bool {
    state.asks.values().any(|ask| ask.is_waiting() && due(ask, now).is_some())
}

/// The trimmed note, whole. Over [`NOTE_MAX_CHARS`]: `note_too_long`
/// (nothing is changed); a note is never cut silently.
pub fn note_of(note: Option<&str>) -> Result<Option<String>, AskError> {
    let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) else {
        return Ok(None);
    };
    check_len("note", note, NOTE_MAX_CHARS)?;
    Ok(Some(note.to_string()))
}

/// `note_too_long` when `text` has more than `max` characters.
pub fn check_len(field: &str, text: &str, max: usize) -> Result<(), AskError> {
    let chars = text.chars().count();
    if chars <= max {
        return Ok(());
    }
    Err(AskError::new(
        "note_too_long",
        format!(
            "{field}: {chars} characters, the limit is {max}. Nothing was sent or changed (the text is never cut). Shorten it, or write the details to a file and name its path in a shorter {field}."
        ),
    )
    .with(json!({"field": field, "chars": chars, "limit": max})))
}

/// Manual resume of a waiting delegation (`delegation_resume`,
/// `cutex/ask/resume`). `requester` is the calling agent (`None`: the owner).
pub fn resume_now(state: &mut AskState, ask_id: &str, requester: Option<&str>, note: Option<&str>, now: DateTime<Utc>) -> Result<Value, AskError> {
    let ask = state.asks.get(ask_id).ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    if ask.remote.is_some() { return Err(AskError::new("remote_unsupported","remote resume is unavailable in P0")); }
    if let Some(requester) = requester {
        if ask.requester.session() != Some(requester) {
            return Err(AskError::new("ask_not_found", "no such delegation of yours"));
        }
    }
    if !ask.is_open() {
        return Err(AskError::new("ask_closed", "it is already closed").with(json!({"closed": ask.closed})));
    }
    if ask.resume_pending() || (ask.resume.is_some() && ask.execution == ExecutionState::Running) {
        return Ok(json!({"ok": true, "ask_id": ask_id, "resuming": true, "detail": "It is already resuming."}));
    }
    if !ask.is_waiting() {
        return Err(AskError::new(
            "ask_not_waiting",
            "it is not waiting for jobs (only a delegation whose handler yielded with delegation_await can be resumed)",
        ));
    }
    let by = if requester.is_some() { ResumeBy::Requester } else { ResumeBy::Owner };
    let note = note_of(note)?;
    state.update_ask(ask_id, now, |ask| to_resume(ask, ResumeReason::Resumed, Some(by), note, now));
    Ok(json!({"ok": true, "ask_id": ask_id, "resuming": true,
        "detail": "The handler continues this delegation in a new turn as soon as it can take one; its result then arrives as usual."}))
}

/// Reopen an answered delegation as a new attempt of the same delegation
/// (`delegation_reopen`, `delegation_close changes_requested`,
/// `cutex/ask/reopen`; contract §4). Job bindings stay.
pub fn reopen(state: &mut AskState, ask_id: &str, requester: Option<&str>, note: Option<&str>, now: DateTime<Utc>) -> Result<Value, AskError> {
    let ask = state.asks.get(ask_id).ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    if ask.remote.is_some() { return Err(AskError::new("remote_unsupported","remote reopen is unavailable in P0")); }
    if let Some(requester) = requester {
        if ask.requester.session() != Some(requester) {
            return Err(AskError::new("ask_not_found", "no such delegation of yours"));
        }
    }
    let note = note_of(note)?.ok_or_else(|| AskError::new("invalid_arguments", "note: say what is missing or what to change"))?;
    let answered = matches!(ask.execution, ExecutionState::Answered | ExecutionState::Failed);
    let reopenable = answered
        && (ask.review == Some(ReviewState::AwaitingAcceptance) || (ask.closed == Some(CloseOutcome::Completed) && ask.review.is_none()));
    if !reopenable {
        return Err(AskError::new(
            "ask_not_reopenable",
            "only an answered delegation that awaits acceptance (or was completed without review) can be reopened",
        )
        .with(json!({"execution": ask.execution, "review": ask.review, "closed": ask.closed})));
    }
    let by = if requester.is_some() { ResumeBy::Requester } else { ResumeBy::Owner };
    state.update_ask(ask_id, now, |ask| {
        ask.execution = ExecutionState::Running;
        ask.review = None;
        ask.closed = None;
        ask.closed_at = None;
        to_resume(ask, ResumeReason::Reopened, Some(by), Some(note.clone()), now);
    });
    // The requester decided about the answer: pending items about it are
    // settled, never sent as an event afterwards.
    if let Some(requester) = state.asks.get(ask_id).and_then(|ask| ask.requester.session().map(str::to_string)) {
        super::delivery::settle_for_ask(state, &requester, ask_id, None, "reopened", now);
    }
    Ok(json!({"ok": true, "ask_id": ask_id, "reopened": true,
        "detail": "Reopened: the handler continues this delegation in a new attempt with your note; the new result arrives as usual."}))
}

/// `delegation_await` arguments (validated by the tool).
#[derive(Clone, Debug, Default)]
pub struct AwaitRequest {
    pub delegation: Option<String>,
    pub jobs: Option<Vec<String>>,
    pub until: AwaitUntil,
    pub resume: AwaitResume,
    pub timeout_s: Option<u64>,
    pub note: String,
}

fn job_line(job: &JobBinding) -> Value {
    match &job.terminal {
        Some(terminal) => json!({"jobId": job.job_id, "state": terminal.status, "exitCode": terminal.exit_code, "ok": terminal.ok}),
        None => json!({"jobId": job.job_id, "state": "running"}),
    }
}

/// Register (or extend) the wait of the delegation the caller's current
/// turn handles (§2.1). The caller's turn must hold the ask's lease.
pub fn register(state: &mut AskState, caller: &str, request: &AwaitRequest, now: DateTime<Utc>) -> Result<Value, AskError> {
    let (running, attempt) = match state.leases.get(caller).map(|lease| &lease.holder) {
        Some(LeaseHolder::Ask { ask_id, attempt }) => (ask_id.clone(), *attempt),
        _ => return Err(AskError::new("ask_not_running", "delegation_await works only in the turn that handles a delegation")),
    };
    if request.delegation.as_deref().is_some_and(|wanted| wanted != running) {
        return Err(AskError::new("ask_not_running", "your current turn handles another delegation").with(json!({"handling": running})));
    }
    let ask = state.asks.get(&running).cloned().ok_or_else(|| AskError::new("ask_not_found", "no such delegation"))?;
    if ask.remote.is_some() || ask.target_cutex_session_id != caller || !ask.is_open() {
        return Err(AskError::new("ask_not_running", "this delegation is not open on you"));
    }
    if let Some(timeout) = request.timeout_s {
        if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout) {
            return Err(AskError::new("invalid_arguments", format!("timeout_s: {MIN_TIMEOUT_SECS}..{MAX_TIMEOUT_SECS}")));
        }
    }
    let note = note_of(Some(&request.note))?.ok_or_else(|| AskError::new("invalid_arguments", "note: a short stage note for the requester"))?;
    let named = request.jobs.clone().unwrap_or_default();
    if let Some(bad) = named.iter().find(|id| !valid_job_id(id)) {
        return Err(AskError::new("invalid_arguments", format!("jobs: {} is not a job id", truncate_chars(bad, 80))));
    }
    if ask.jobs.len() + named.iter().filter(|id| ask.job(id).is_none()).count() > MAX_JOBS {
        return Err(AskError::new("too_many_jobs", format!("a delegation binds at most {MAX_JOBS} jobs")));
    }
    // Bind named jobs first; the wait set is the named jobs, or every bound
    // job, minus those already finished.
    state.update_ask(&running, now, |ask| {
        for id in &named {
            bind(ask, id, JobSource::Explicit, attempt, None, now);
        }
    });
    let ask = state.asks.get(&running).cloned().expect("exists");
    let wanted: Vec<&JobBinding> = if named.is_empty() {
        ask.jobs.iter().collect()
    } else {
        named.iter().filter_map(|id| ask.job(id)).collect()
    };
    if wanted.is_empty() {
        return Err(AskError::new(
            "no_jobs",
            "this delegation has no bound jobs to wait for: start them with cutex_job submit in this turn, or name them in jobs. Nothing was registered; your final message of this turn is the result.",
        ));
    }
    let open: Vec<String> = wanted.iter().filter(|job| !job.is_terminal()).map(|job| job.job_id.clone()).collect();
    let lines: Vec<Value> = wanted.iter().map(|job| job_line(job)).collect();
    if open.is_empty() {
        return Ok(json!({"ok": true, "ask_id": running, "waiting": false, "jobs": lines,
            "detail": "Every job you would wait for has already finished (see jobs). Nothing to wait for: continue in this turn; your final message of this turn is the result."}));
    }
    let timeout_at = request.timeout_s.map(|secs| now_rfc3339(now + chrono::Duration::seconds(secs as i64)));
    state.update_ask(&running, now, |ask| {
        let mut jobs = open.clone();
        let mut since = now_rfc3339(now);
        if let Some(previous) = ask.awaiting.as_ref().filter(|wait| wait.attempt == attempt) {
            since = previous.since.clone();
            for id in &previous.jobs {
                if !jobs.contains(id) && ask.job(id).is_some_and(|job| !job.is_terminal()) {
                    jobs.push(id.clone());
                }
            }
        }
        ask.awaiting = Some(AwaitRecord {
            attempt,
            jobs,
            until: request.until,
            resume: request.resume,
            note: note.clone(),
            since,
            timeout_at: timeout_at.clone(),
        });
        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
            record.activity.line = Some(truncate_chars(&note, 300));
            record.progress.push(ProgressNote { text: truncate_chars(&note, 300), at: now_rfc3339(now) });
            let excess = record.progress.len().saturating_sub(PROGRESS_KEEP);
            record.progress.drain(..excess);
        }
    });
    let ask = state.asks.get(&running).cloned().expect("exists");
    state.notice(
        "cutex/ask/progress",
        AskState::ask_sessions(&ask),
        json!({"askId": running, "text": truncate_chars(&note, 300), "at": now_rfc3339(now), "stage": true}),
    );
    let wait = ask.awaiting.clone().expect("registered");
    let when = match (wait.until, wait.resume) {
        (_, AwaitResume::Each) => "after each job that finishes",
        (AwaitUntil::All, AwaitResume::All) => "when all of them have finished",
        (AwaitUntil::Any, AwaitResume::All) => "when the first of them finishes",
    };
    let mut detail = format!(
        "Waiting for {} job(s). End your turn now: your final message of this turn is a stage note for the requester, not the result. You get a new turn for this delegation {when}, at once if a job fails",
        wait.jobs.len()
    );
    if let Some(at) = &wait.timeout_at {
        detail.push_str(&format!(", or at {at} (timeout)"));
    }
    detail.push_str(". In that turn your final message is the result unless you call delegation_await again.");
    Ok(json!({"ok": true, "ask_id": running, "waiting": true,
        "jobs": ask.jobs.iter().filter(|job| wait.jobs.contains(&job.job_id)).map(job_line).collect::<Vec<_>>(),
        "until": wait.until, "resume": wait.resume, "timeout_at": wait.timeout_at, "detail": detail}))
}

/// The turn of attempt `attempt` ended `completed` with a registered wait:
/// a stage, not the answer (§2.2). Keeps the last message as the stage
/// note, holds `waitingForEvent`, releases the lease and wakes the wait at
/// once when it is already met.
pub fn yield_attempt(store: &AskStore, session: &str, ask_id: &str, attempt: u32, turn: &Value, now: DateTime<Utc>) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return Ok(());
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == attempt).cloned() else {
        return Ok(());
    };
    let items: Vec<Value> = turn.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let readers = AskState::ask_sessions(&ask);
    let message_artifact = match super::answer::final_message(&items) {
        Some(text) => Some(store.put_artifact(text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(ask_id), now)?),
        None => record.last_message_artifact.clone(),
    };
    let stage = message_artifact
        .as_deref()
        .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
        .and_then(|(_, bytes)| super::views::preview(&String::from_utf8_lossy(&bytes)));
    store.with_state(|state| {
        state.update_ask(ask_id, now, |ask| {
            if let Some(record) = ask.attempts.iter_mut().find(|item| item.n == attempt) {
                record.state = AttemptState::Yielded;
                record.ended_at = Some(now_rfc3339(now));
                record.stage_note = stage.clone();
                if message_artifact.is_some() {
                    record.last_message_artifact = message_artifact.clone();
                }
            }
            ask.execution = ExecutionState::Running;
            if let Some(wait) = &ask.awaiting {
                ask.hold = Some(Hold::WaitingForEvent {
                    jobs: wait.jobs.clone(),
                    until: wait.until,
                    since: now_rfc3339(now),
                    timeout_at: wait.timeout_at.clone(),
                });
            }
        });
        for question in state.questions.values_mut() {
            if question.ask_id == ask_id && question.attempt == attempt && question.state == QuestionState::Open {
                question.state = QuestionState::Stale;
            }
        }
        if let Some(lease) = state.leases.get(session) {
            if matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt) {
                state.leases.remove(session);
            }
        }
        // A requester blocked in delegation_wait returns now instead of
        // sitting out its timeout; otherwise shown by agent_list (passive).
        if let Some(updated) = state.asks.get(ask_id).cloned() {
            super::delivery::record_state_delivery(state, &updated, WAITING_STATE, now);
        }
        wake(state, ask_id, now);
    })?;
    Ok(())
}

/// State item of a delegation whose handler yielded (passive).
pub const WAITING_STATE: &str = "waitingForEvent";

/// Ask the handler's runtime to cancel the delegation's running bound jobs
/// (`stop_jobs`, §5). Returns the jobs marked; the caller queues the
/// [`super::store::RuntimeCommand::CancelJobs`] command.
pub fn mark_cancel(ask: &mut AskRecord, by: ResumeBy, now: DateTime<Utc>) -> Vec<String> {
    let mut marked = Vec::new();
    for job in ask.jobs.iter_mut().filter(|job| job.terminal.is_none() && job.cancel.is_none()) {
        job.cancel = Some(JobCancel { requested_at: now_rfc3339(now), by, state: JobCancelState::Pending, detail: None, at: None });
        marked.push(job.job_id.clone());
    }
    marked
}

/// One job's state from a `cutex_job query` result (`JobRecord`).
fn queried(result: &Value) -> Option<(u64, String)> {
    let record = structured_result(result)?;
    let record = record.get("job").cloned().unwrap_or(record);
    Some((record.get("revision")?.as_u64()?, record.get("state")?.as_str()?.to_string()))
}

const TERMINAL_JOB_STATES: &[&str] = &["exited", "failed", "cancelled", "interrupted", "launch_unknown", "launchUnknown"];

/// Run a queued cancel through the handler's runtime: `query` for the
/// revision, then `cancel` (the Job adapter signs as the handler, the job's
/// subscriber). `Ok(true)` when done for every job.
pub fn run_cancel(
    runtime: &dyn super::scheduler::AskRuntime,
    store: &AskStore,
    ask_id: &str,
    session: &str,
    jobs: &[String],
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id) else {
        return Ok(true);
    };
    let pending: Vec<JobBinding> = ask
        .jobs
        .iter()
        .filter(|job| jobs.contains(&job.job_id) && job.cancel.as_ref().is_some_and(|cancel| cancel.state == JobCancelState::Pending))
        .cloned()
        .collect();
    if pending.is_empty() {
        return Ok(true);
    }
    let status = runtime.target_status(session);
    let mut results: Vec<(String, JobCancelState, Option<String>)> = Vec::new();
    for job in &pending {
        if job.terminal.is_some() {
            results.push((job.job_id.clone(), JobCancelState::NotRunning, Some("already finished".into())));
            continue;
        }
        let requested = job.cancel.as_ref().and_then(|cancel| parse_time(&cancel.requested_at));
        let stale = requested.is_some_and(|at| now - at > chrono::Duration::hours(CANCEL_GIVE_UP_HOURS));
        if !status.online {
            if stale {
                results.push((job.job_id.clone(), JobCancelState::Failed, Some("the handler stayed offline; its runtime cannot cancel the job".into())));
            }
            continue;
        }
        let query = runtime.call_mcp_tool(session, JOB_SERVER, "query", &json!({"jobId": job.job_id}));
        let outcome = match query {
            Err(error) => (JobCancelState::Failed, Some(truncate_chars(&format!("query: {error:#}"), 300))),
            Ok(result) => match queried(&result) {
                None => (JobCancelState::Failed, Some(truncate_chars(&format!("unexpected query result: {result}"), 300))),
                Some((_, state)) if TERMINAL_JOB_STATES.contains(&state.as_str()) => (JobCancelState::NotRunning, Some(format!("job is {state}"))),
                Some((revision, _)) => {
                    match runtime.call_mcp_tool(session, JOB_SERVER, "cancel", &json!({"jobId": job.job_id, "expectedRevision": revision})) {
                        Ok(result) if result.get("isError").and_then(Value::as_bool) == Some(true) => {
                            (JobCancelState::Failed, Some(truncate_chars(&format!("cancel: {result}"), 300)))
                        }
                        Ok(_) => (JobCancelState::Sent, None),
                        Err(error) => (JobCancelState::Failed, Some(truncate_chars(&format!("cancel: {error:#}"), 300))),
                    }
                }
            },
        };
        results.push((job.job_id.clone(), outcome.0, outcome.1));
    }
    let done = results.len() == pending.len();
    if !results.is_empty() {
        store.with_state(|state| {
            state.update_ask(ask_id, now, |ask| {
                for (job_id, cancel_state, detail) in &results {
                    if let Some(cancel) = ask.jobs.iter_mut().find(|job| &job.job_id == job_id).and_then(|job| job.cancel.as_mut()) {
                        cancel.state = *cancel_state;
                        cancel.detail = detail.clone();
                        cancel.at = Some(now_rfc3339(now));
                    }
                }
            });
        })?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};

    fn submit_item(job: &str) -> Value {
        json!({"type": "mcpToolCall", "id": "call-1", "server": "cutex_job", "tool": "submit", "status": "completed",
            "arguments": {"actionId": "build-1", "argv": ["make"], "cwd": "/w"},
            "result": {"content": [{"type": "text", "text": "{}"}],
                "structuredContent": {"status": "accepted", "deduplicated": false,
                    "job": {"jobId": job, "revision": 1, "state": "running", "request": {"actionId": "build-1"}}}}})
    }

    /// A running ask of TARGET holding the lease for attempt 1.
    fn running(state: &mut AskState, action: &str, now: DateTime<Utc>) -> String {
        let ask_id = state.create_ask(new_ask(action, "build and test"), now).unwrap().0.ask_id;
        state.update_ask(&ask_id, now, |ask| {
            let mut record = AttemptRecord::submitting(1, "m-1".into(), 1, now_rfc3339(now));
            record.state = AttemptState::Running;
            record.turn_id = Some("turn-1".into());
            ask.attempts.push(record);
            ask.execution = ExecutionState::Running;
        });
        state.leases.insert(
            TARGET.into(),
            AdmissionLease {
                cutex_session_id: TARGET.into(),
                holder: LeaseHolder::Ask { ask_id: ask_id.clone(), attempt: 1 },
                native_message_id: "m-1".into(),
                runtime_generation: 1,
                turn_id: Some("turn-1".into()),
                since: now_rfc3339(now),
            },
        );
        ask_id
    }

    fn request(note: &str) -> AwaitRequest {
        AwaitRequest { note: note.into(), ..AwaitRequest::default() }
    }

    /// End the wait's attempt (what `yield_attempt` does to the record).
    fn yielded(state: &mut AskState, ask_id: &str, now: DateTime<Utc>) {
        state.leases.remove(TARGET);
        state.update_ask(ask_id, now, |ask| {
            ask.current_attempt_mut().unwrap().state = AttemptState::Yielded;
            let wait = ask.awaiting.clone().unwrap();
            ask.hold = Some(Hold::WaitingForEvent { jobs: wait.jobs, until: wait.until, since: now_rfc3339(now), timeout_at: wait.timeout_at });
        });
    }

    fn exited(code: i64) -> JobOutcome {
        JobOutcome { status: "exited".into(), exit_code: Some(code) }
    }

    #[test]
    fn submit_results_bind_jobs_in_both_result_shapes() {
        assert_eq!(submitted_job(&submit_item("job-1")), Some(("job-1".into(), Some("build-1".into()))));
        let mut text_only = submit_item("job-2");
        text_only["result"] = json!({"content": [{"type": "text", "text": "{\"job\":{\"jobId\":\"job-2\"}}"}]});
        assert_eq!(submitted_job(&text_only), Some(("job-2".into(), Some("build-1".into()))));
        let mut other = submit_item("job-3");
        other["tool"] = json!("query");
        assert_eq!(submitted_job(&other), None);
        let mut failed = submit_item("job-4");
        failed["status"] = json!("failed");
        assert_eq!(submitted_job(&failed), None);
        let mut state = AskState::default();
        let now = Utc::now();
        let ask_id = running(&mut state, "a", now);
        assert_eq!(observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now), Some("job-1".into()));
        assert_eq!(observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now), Some("job-1".into()));
        let ask = &state.asks[&ask_id];
        assert_eq!(ask.jobs.len(), 1);
        assert_eq!(ask.jobs[0].source, JobSource::Auto);
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/updated"));
    }

    #[test]
    fn register_needs_the_leased_turn_and_jobs() {
        let mut state = AskState::default();
        let now = Utc::now();
        assert_eq!(register(&mut state, TARGET, &request("x"), now).unwrap_err().code, "ask_not_running");
        let ask_id = running(&mut state, "a", now);
        assert_eq!(register(&mut state, TARGET, &request("waiting"), now).unwrap_err().code, "no_jobs");
        assert_eq!(register(&mut state, REQUESTER, &request("x"), now).unwrap_err().code, "ask_not_running");
        let mut wrong = request("x");
        wrong.delegation = Some("ask-other".into());
        assert_eq!(register(&mut state, TARGET, &wrong, now).unwrap_err().code, "ask_not_running");
        let mut bad = request("x");
        bad.jobs = Some(vec!["a b".into()]);
        assert_eq!(register(&mut state, TARGET, &bad, now).unwrap_err().code, "invalid_arguments");
        let mut empty_note = request(" ");
        empty_note.jobs = Some(vec!["job-1".into()]);
        assert_eq!(register(&mut state, TARGET, &empty_note, now).unwrap_err().code, "invalid_arguments");
        observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now);
        let mut named = request("tests running");
        named.jobs = Some(vec!["job-2".into()]);
        named.timeout_s = Some(3600);
        let result = register(&mut state, TARGET, &named, now).unwrap();
        assert_eq!(result["waiting"], true);
        assert_eq!(result["jobs"].as_array().unwrap().len(), 1);
        // A second call in the same attempt extends the set.
        let result = register(&mut state, TARGET, &request("both running"), now).unwrap();
        assert_eq!(result["jobs"].as_array().unwrap().len(), 2);
        let ask = &state.asks[&ask_id];
        let wait = ask.awaiting.as_ref().unwrap();
        assert_eq!(wait.attempt, 1);
        assert_eq!(wait.note, "both running");
        assert!(wait.jobs.contains(&"job-1".to_string()) && wait.jobs.contains(&"job-2".to_string()));
        assert_eq!(ask.job("job-2").unwrap().source, JobSource::Explicit);
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/progress" && notice.params["stage"] == true));
        // Not waiting yet: the turn still runs.
        assert!(!state.asks[&ask_id].is_waiting());
    }

    #[test]
    fn finished_jobs_need_no_wait() {
        let mut state = AskState::default();
        let now = Utc::now();
        let ask_id = running(&mut state, "a", now);
        observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now);
        assert!(!note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(0), now));
        let result = register(&mut state, TARGET, &request("x"), now).unwrap();
        assert_eq!(result["waiting"], false);
        assert_eq!(result["jobs"][0]["ok"], true);
        assert!(state.asks[&ask_id].awaiting.is_none());
    }

    #[test]
    fn wake_policy_failure_all_any_each_and_timeout() {
        let now = Utc::now();
        let setup = |until: AwaitUntil, resume: AwaitResume, timeout: Option<u64>| {
            let mut state = AskState::default();
            let ask_id = running(&mut state, "a", now);
            observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now);
            observe_item(&mut state, &ask_id, 1, &submit_item("job-2"), now);
            let mut wait = request("two jobs");
            wait.until = until;
            wait.resume = resume;
            wait.timeout_s = timeout;
            register(&mut state, TARGET, &wait, now).unwrap();
            (state, ask_id)
        };
        // Awaited completions go passive and are recorded once per event.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::All, None);
        assert!(note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(0), now));
        yielded(&mut state, &ask_id, now);
        assert!(state.asks[&ask_id].is_waiting());
        assert_eq!(wake_due(&mut state, now), Vec::<String>::new());
        assert!(note_job_completion(&mut state, TARGET, "job-2", "evt-2", &exited(0), now));
        let ask = &state.asks[&ask_id];
        assert!(!ask.is_waiting() && ask.resume_pending());
        let resume = ask.resume.as_ref().unwrap();
        assert_eq!((resume.reason, resume.from_attempt), (ResumeReason::Jobs, 1));
        assert_eq!(ask.execution, ExecutionState::Running);
        assert!(ask.hold.is_none());
        // A failure wakes at once.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::All, None);
        yielded(&mut state, &ask_id, now);
        note_job_completion(&mut state, TARGET, "job-2", "evt-2", &JobOutcome { status: "failed".into(), exit_code: None }, now);
        assert_eq!(state.asks[&ask_id].resume.as_ref().unwrap().reason, ResumeReason::JobFailed);
        // A non-zero exit is a failure too.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::All, None);
        yielded(&mut state, &ask_id, now);
        note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(2), now);
        assert_eq!(state.asks[&ask_id].resume.as_ref().unwrap().reason, ResumeReason::JobFailed);
        // until any: the first success.
        let (mut state, ask_id) = setup(AwaitUntil::Any, AwaitResume::All, None);
        yielded(&mut state, &ask_id, now);
        note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(0), now);
        assert_eq!(state.asks[&ask_id].resume.as_ref().unwrap().reason, ResumeReason::Jobs);
        // resume each: every success.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::Each, None);
        yielded(&mut state, &ask_id, now);
        note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(0), now);
        assert!(state.asks[&ask_id].resume_pending());
        // Timeout.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::All, Some(60));
        yielded(&mut state, &ask_id, now);
        assert!(wake_due(&mut state, now).is_empty());
        assert!(any_due(&state, now + chrono::Duration::seconds(61)));
        assert_eq!(wake_due(&mut state, now + chrono::Duration::seconds(61)), vec![ask_id.clone()]);
        assert_eq!(state.asks[&ask_id].resume.as_ref().unwrap().reason, ResumeReason::Timeout);
        // A job nobody awaits is recorded but delivered as before.
        let (mut state, ask_id) = setup(AwaitUntil::All, AwaitResume::All, None);
        state.update_ask(&ask_id, now, |ask| ask.awaiting = None);
        assert!(!note_job_completion(&mut state, TARGET, "job-1", "evt-1", &exited(0), now));
        assert!(state.asks[&ask_id].job("job-1").unwrap().terminal.is_some());
        assert!(!note_job_completion(&mut state, TARGET, "job-9", "evt-9", &exited(0), now));
    }

    #[test]
    fn manual_resume_and_reopen() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask_id = running(&mut state, "a", now);
        observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now);
        assert_eq!(resume_now(&mut state, &ask_id, Some(REQUESTER), None, now).unwrap_err().code, "ask_not_waiting");
        register(&mut state, TARGET, &request("running"), now).unwrap();
        yielded(&mut state, &ask_id, now);
        assert_eq!(resume_now(&mut state, &ask_id, Some(TARGET), None, now).unwrap_err().code, "ask_not_found");
        resume_now(&mut state, &ask_id, Some(REQUESTER), Some("go on with partial data"), now).unwrap();
        let resume = state.asks[&ask_id].resume.clone().unwrap();
        assert_eq!((resume.reason, resume.by, resume.note.as_deref()), (ResumeReason::Resumed, Some(ResumeBy::Requester), Some("go on with partial data")));
        assert_eq!(resume_now(&mut state, &ask_id, None, None, now).unwrap()["resuming"], true);

        // Reopen: only answered delegations; the review closes again later.
        let mut state = AskState::default();
        let mut review = new_ask("r", "draft");
        review.options.review = true;
        let ask_id = state.create_ask(review, now).unwrap().0.ask_id;
        assert_eq!(reopen(&mut state, &ask_id, Some(REQUESTER), Some("more"), now).unwrap_err().code, "ask_not_reopenable");
        state.update_ask(&ask_id, now, |ask| {
            let mut record = AttemptRecord::submitting(1, "m".into(), 1, now_rfc3339(now));
            record.state = AttemptState::Answered;
            ask.attempts.push(record);
            ask.execution = ExecutionState::Answered;
            ask.review = Some(ReviewState::AwaitingAcceptance);
        });
        let answered = state.asks[&ask_id].clone();
        crate::asks::delivery::record_answer_delivery(&mut state, &answered, "ans-1", now);
        assert_eq!(reopen(&mut state, &ask_id, Some(REQUESTER), None, now).unwrap_err().code, "invalid_arguments");
        reopen(&mut state, &ask_id, Some(REQUESTER), Some("tests are missing"), now).unwrap();
        let ask = &state.asks[&ask_id];
        assert_eq!((ask.execution, ask.review, ask.closed), (ExecutionState::Running, None, None));
        assert!(ask.resume_pending());
        assert_eq!(ask.resume.as_ref().unwrap().reason, ResumeReason::Reopened);
        assert!(state.deliveries.values().all(|delivery| delivery.settled_by.as_deref() == Some("reopened")));
        // Accepted delegations stay closed.
        state.update_ask(&ask_id, now, |ask| {
            ask.resume = None;
            ask.execution = ExecutionState::Answered;
            ask.review = Some(ReviewState::Accepted);
            ask.closed = Some(CloseOutcome::Accepted);
        });
        assert_eq!(reopen(&mut state, &ask_id, None, Some("x"), now).unwrap_err().code, "ask_not_reopenable");
    }

    #[test]
    fn cancel_marks_only_running_jobs() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask_id = running(&mut state, "a", now);
        observe_item(&mut state, &ask_id, 1, &submit_item("job-1"), now);
        observe_item(&mut state, &ask_id, 1, &submit_item("job-2"), now);
        note_job_completion(&mut state, TARGET, "job-2", "evt-2", &exited(0), now);
        let marked = state.update_ask(&ask_id, now, |ask| mark_cancel(ask, ResumeBy::Requester, now)).unwrap();
        assert_eq!(marked, vec!["job-1".to_string()]);
        // A cancelled job's completion goes passive (no turn for it).
        assert!(note_job_completion(&mut state, TARGET, "job-1", "evt-1", &JobOutcome { status: "cancelled".into(), exit_code: None }, now));
        assert_eq!(queried(&json!({"structuredContent": {"revision": 3, "state": "running"}})), Some((3, "running".into())));
    }
}

/// Await → yield → wake → continuation through the scheduler, the observer
/// and the tools (fake runtime; contract §11).
#[cfg(test)]
mod flow {
    use super::*;
    use crate::asks::observe::observe;
    use crate::asks::scheduler::tests::{world, FakeRuntime};
    use crate::asks::scheduler::tick;
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};
    use crate::asks::tools::tests::call;

    fn started(turn: &str) -> Value {
        json!({"threadId": "t", "turn": {"id": turn, "status": "inProgress", "items": [], "error": null}})
    }

    fn item(turn: &str, item: Value) -> Value {
        json!({"threadId": "t", "turnId": turn, "item": item})
    }

    fn completed(turn: &str, message: &str) -> Value {
        json!({"threadId": "t", "turn": {"id": turn, "status": "completed", "error": null,
            "items": [{"type": "agentMessage", "id": "m", "text": message}]}})
    }

    fn submit(job: &str) -> Value {
        json!({"type": "mcpToolCall", "id": format!("call-{job}"), "server": "cutex_job", "tool": "submit", "status": "completed",
            "arguments": {"actionId": job}, "result": {"content": [], "structuredContent": {"status": "accepted",
                "job": {"jobId": job, "revision": 1, "state": "running", "request": {"actionId": job}}}}})
    }

    /// The turn id of the lease the scheduler just took.
    fn leased_turn(store: &AskStore) -> String {
        store.snapshot().unwrap().leases[TARGET].turn_id.clone().unwrap()
    }

    fn end_turn(runtime: &FakeRuntime, store: &AskStore, turn: &str, message: &str, now: DateTime<Utc>) {
        observe(store, TARGET, "turn/completed", &completed(turn, message), now).unwrap();
        runtime.set(TARGET, |status| status.active_turn_id = None);
    }

    fn complete(store: &AskStore, job: &str, outcome: JobOutcome, now: DateTime<Utc>) -> bool {
        store.with_state(|state| note_job_completion(state, TARGET, job, &format!("evt-{job}"), &outcome, now)).unwrap()
    }

    fn exit(code: i64) -> JobOutcome {
        JobOutcome { status: "exited".into(), exit_code: Some(code) }
    }

    #[test]
    fn failure_wakes_then_all_complete_then_answer_and_reopen() {
        let (world, dir) = world("await-flow");
        let (store, store_dir) = temp_store("await-flow");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut review = new_ask("a", "build and test the branch");
        review.options.review = true;
        let ask_id = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        // Attempt 1: starts two jobs and awaits them.
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        observe(&store, TARGET, "item/completed", &item(&turn, submit("job-1")), now).unwrap();
        observe(&store, TARGET, "item/completed", &item(&turn, submit("job-2")), now).unwrap();
        // Only the handler, in the leased turn.
        assert_eq!(call(&world, &store, REQUESTER, "delegation_await", json!({"note": "x"}), "w0")["code"], "delegation_not_running");
        let waiting = call(&world, &store, TARGET, "delegation_await", json!({"note": "build + tests running"}), "w1");
        assert_eq!(waiting["waiting"], true, "{waiting}");
        assert_eq!(waiting["jobs"].as_array().unwrap().len(), 2);
        end_turn(&runtime, &store, &turn, "Started the build and the tests; waiting for them.", now);
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        assert!(ask.is_waiting(), "{ask:?}");
        assert_eq!(ask.execution, ExecutionState::Running);
        assert!(matches!(&ask.hold, Some(Hold::WaitingForEvent { jobs, .. }) if jobs.len() == 2));
        assert_eq!(ask.attempts[0].state, AttemptState::Yielded);
        assert_eq!(ask.attempts[0].stage_note.as_deref(), Some("Started the build and the tests; waiting for them."));
        assert!(ask.answer_id.is_none() && state.answers.is_empty(), "a stage is not an answer");
        assert!(state.leases.is_empty());
        // Nothing to admit while waiting.
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());

        // job-2 fails: passive completion, immediate wake, continuation.
        assert!(complete(&store, "job-2", exit(1), now));
        assert_eq!(store.snapshot().unwrap().asks[&ask_id].resume.as_ref().unwrap().reason, ResumeReason::JobFailed);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let (_, submission) = runtime.submitted.borrow().last().cloned().unwrap();
        assert!(submission.text.contains("attempt 2 · continue"), "{}", submission.text);
        assert!(submission.text.contains("job job-2 failed (exit 1) (1/2 ended)"), "{}", submission.text);
        assert!(submission.text.contains("Your stage note (attempt 1): Started the build"), "{}", submission.text);
        assert!(submission.text.contains("delegation_await again to keep waiting for: job-1"), "{}", submission.text);
        let view = submission.view.clone().unwrap();
        assert_eq!(view["data"]["resumeOf"], 1);
        assert_eq!(view["data"]["continuation"]["reason"], "jobFailed");
        assert_eq!(submission.attempt, Some(2));
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        assert!(ask.resume.is_none(), "consumed at the receipt");
        assert_eq!(ask.attempts[1].continuation.as_ref().unwrap().of, 1);
        // Attempt 2 waits again (for job-1 only).
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        let waiting = call(&world, &store, TARGET, "delegation_await", json!({"note": "fixed the test; waiting for the build"}), "w2");
        assert_eq!(waiting["jobs"].as_array().unwrap().len(), 1, "{waiting}");
        end_turn(&runtime, &store, &turn, "Waiting for the build.", now);
        assert!(store.snapshot().unwrap().asks[&ask_id].is_waiting());

        // job-1 succeeds: all done, continuation 3 answers.
        assert!(complete(&store, "job-1", exit(0), now));
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let (_, submission) = runtime.submitted.borrow().last().cloned().unwrap();
        assert!(submission.text.contains("all awaited jobs ended (1/1)"), "{}", submission.text);
        assert!(!submission.text.contains("delegation_await again"), "{}", submission.text);
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        end_turn(&runtime, &store, &turn, "Build green, tests fixed.", now);
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        assert_eq!((ask.execution, ask.review), (ExecutionState::Answered, Some(ReviewState::AwaitingAcceptance)));
        assert_eq!(
            ask.attempts.iter().map(|record| record.state).collect::<Vec<_>>(),
            vec![AttemptState::Yielded, AttemptState::Yielded, AttemptState::Answered]
        );
        assert_eq!(state.answers.len(), 1);

        // Changes requested: same delegation, attempt 4.
        let reopened = call(
            &world,
            &store,
            REQUESTER,
            "delegation_close",
            json!({"delegation": ask_id, "outcome": "changes_requested", "note": "add a regression test"}),
            "r1",
        );
        assert_eq!(reopened["reopened"], true, "{reopened}");
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let (_, submission) = runtime.submitted.borrow().last().cloned().unwrap();
        assert!(submission.text.contains("reopened") && submission.text.contains("(rev 1): add a regression test"), "{}", submission.text);
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        end_turn(&runtime, &store, &turn, "Added the regression test.", now);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask_id].review, Some(ReviewState::AwaitingAcceptance));
        assert_eq!(state.answers.values().map(|answer| answer.revision).max(), Some(2));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// A note with distinct numbered requirements, `chars` characters long,
    /// ending in a marker that must reach the handler.
    fn long_note(chars: usize, tail: &str) -> String {
        let mut note = String::new();
        let mut n = 1;
        while note.chars().count() + tail.len() < chars {
            note.push_str(&format!("Requirement {n}: fix item {n} as described in review/full.md. "));
            n += 1;
        }
        let mut note: String = note.chars().take(chars - tail.len()).collect();
        note.push_str(tail);
        assert_eq!(note.chars().count(), chars);
        note
    }

    /// The incident of 2026-10-12: a changes_requested note past 1000
    /// characters was clipped silently. Now it reaches the handler whole
    /// (≤ 4000 characters inline), or as a preview that says it continues
    /// and points to the full note; a note over the hard limit is refused.
    #[test]
    fn long_reopen_notes_reach_the_handler_whole_or_with_a_pointer() {
        let (world, dir) = world("await-long-note");
        let (store, store_dir) = temp_store("await-long-note");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut review = new_ask("a", "implement the parser");
        review.options.review = true;
        let ask_id = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        end_turn(&runtime, &store, &turn, "Parser done.", now);

        // 2500 characters: delivered whole, inline.
        let note = long_note(2500, " REQUIREMENT 3 and the full review: review/full.md END");
        let reopened = call(&world, &store, REQUESTER, "delegation_close",
            json!({"delegation": ask_id, "outcome": "changes_requested", "note": note}), "r1");
        assert_eq!(reopened["reopened"], true, "{reopened}");
        assert_eq!(store.snapshot().unwrap().asks[&ask_id].resume.as_ref().unwrap().note.as_deref(), Some(note.as_str()));
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let (_, submission) = runtime.submitted.borrow().last().cloned().unwrap();
        assert!(submission.text.contains(&note), "{}", submission.text);
        assert!(!submission.text.contains("note continues"), "{}", submission.text);
        // The view keeps a marked preview.
        let view = submission.view.clone().unwrap();
        let preview = view["data"]["continuation"]["note"].as_str().unwrap();
        assert!(preview.ends_with('…') && preview.chars().count() == 300, "{preview}");
        assert_eq!(view["data"]["continuation"]["noteTruncated"], true);
        assert_eq!(view["data"]["continuation"]["noteChars"], 2500);
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        end_turn(&runtime, &store, &turn, "Fixed 1-3.", now);

        // 7000 characters: a preview that says it continues, with a pointer
        // that resolves to the full note for the handler only.
        let note = long_note(7000, " REQUIREMENT 9: see review/full.md END");
        let reopened = call(&world, &store, REQUESTER, "delegation_reopen", json!({"delegation": ask_id, "note": note}), "r2");
        assert_eq!(reopened["reopened"], true, "{reopened}");
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let (_, submission) = runtime.submitted.borrow().last().cloned().unwrap();
        let text = &submission.text;
        assert!(!text.contains("REQUIREMENT 9"), "{text}");
        let preview: String = note.chars().take(NOTE_INLINE_CHARS).collect();
        assert!(text.contains(&format!("{preview}…\n[note continues: 3000 more characters not shown (7000 in all). Read the full note before you start: call the MCP tool delegation_read (cutex server) with delegation=\"")), "{text}");
        assert!(text.contains("The note above is cut; read all of it first: call the MCP tool delegation_read"), "{text}");
        let start = text.find("artifact=\"").expect("artifact pointer") + "artifact=\"".len();
        let artifact = &text[start..start + text[start..].find('"').unwrap()];
        let read = call(&world, &store, TARGET, "delegation_read", json!({"delegation": ask_id, "artifact": artifact}), "x1");
        assert_eq!(read["text"].as_str(), Some(note.as_str()), "{read}");
        assert_eq!(read["next_cursor"], Value::Null, "{read}");
        let other = "cutex.00000000-0000-0000-0000-0000000000c1";
        let denied = call(&world, &store, other, "delegation_read", json!({"delegation": ask_id, "artifact": artifact}), "x2");
        assert!(denied.get("text").is_none() && denied["ok"] != true, "{denied}");
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        end_turn(&runtime, &store, &turn, "Fixed all nine.", now);

        // Over the hard limit: refused with the limit, nothing changed.
        let note = "x".repeat(NOTE_MAX_CHARS + 1);
        let refused = call(&world, &store, REQUESTER, "delegation_close",
            json!({"delegation": ask_id, "outcome": "changes_requested", "note": note}), "r3");
        assert_eq!(refused["code"], "note_too_long", "{refused}");
        assert_eq!(refused["limit"], NOTE_MAX_CHARS, "{refused}");
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask_id].review, Some(ReviewState::AwaitingAcceptance));
        assert!(state.asks[&ask_id].resume.is_none());

        // accepted with a note: the note is reported as not sent.
        let accepted = call(&world, &store, REQUESTER, "delegation_close",
            json!({"delegation": ask_id, "outcome": "accepted", "note": "thanks"}), "r4");
        assert_eq!(accepted["noteDelivered"], false, "{accepted}");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn stop_while_waiting_closes_and_cancels_jobs() {
        let (world, dir) = world("await-stop");
        let (store, store_dir) = temp_store("await-stop");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask_id = store.with_state(|state| state.create_ask(new_ask("a", "run it"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let turn = leased_turn(&store);
        observe(&store, TARGET, "turn/started", &started(&turn), now).unwrap();
        observe(&store, TARGET, "item/completed", &item(&turn, submit("job-1")), now).unwrap();
        call(&world, &store, TARGET, "delegation_await", json!({"note": "running", "timeout_s": 600}), "w1");
        end_turn(&runtime, &store, &turn, "Running.", now);
        // Manual resume while the handler is busy, then stop before the
        // continuation is admitted: it closes at once.
        runtime.set(TARGET, |status| status.active_turn_id = Some("owner".into()));
        let resumed = call(&world, &store, REQUESTER, "delegation_resume", json!({"delegation": ask_id, "note": "enough"}), "r1");
        assert_eq!(resumed["resuming"], true, "{resumed}");
        tick(&world, &runtime, &store, now).unwrap();
        assert!(matches!(store.snapshot().unwrap().asks[&ask_id].hold, Some(Hold::TargetBusy { .. })));
        let stopped = call(&world, &store, REQUESTER, "delegation_stop", json!({"delegation": ask_id, "stop_jobs": true}), "s1");
        assert_eq!(stopped["cancellingJobs"], json!(["job-1"]), "{stopped}");
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        assert_eq!((ask.execution, ask.closed), (ExecutionState::Interrupted, Some(CloseOutcome::Stopped)));
        assert!(ask.resume.is_none() && ask.awaiting.is_none());
        // The management process cancels through the handler's runtime.
        runtime.set(TARGET, |status| status.active_turn_id = None);
        tick(&world, &runtime, &store, now).unwrap();
        let calls = runtime.mcp_calls.borrow().clone();
        assert_eq!(calls.iter().map(|(_, tool, _)| tool.as_str()).collect::<Vec<_>>(), vec!["query", "cancel"]);
        assert_eq!(calls[1].2, json!({"jobId": "job-1", "expectedRevision": 4}));
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask_id].job("job-1").unwrap().cancel.as_ref().unwrap().state, JobCancelState::Sent);
        assert!(state.commands.is_empty());
        assert_eq!(runtime.submitted.borrow().len(), 1, "no continuation after the stop");
        // The cancelled job's completion goes passive.
        assert!(complete(&store, "job-1", JobOutcome { status: "cancelled".into(), exit_code: None }, now));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn revisions_and_follow_up_supersede() {
        let (world, dir) = world("await-rev");
        let (store, store_dir) = temp_store("await-rev");
        let now = Utc::now();
        let ask_id = store.with_state(|state| state.create_ask(new_ask("a", "write the doc"), now).unwrap().0.ask_id).unwrap();
        let tell = call(&world, &store, REQUESTER, "agent_tell", json!({"delegation": ask_id, "text": "use the v2 API", "supersedes": "v1"}), "t1");
        assert_eq!(tell["revision"]["id"], "v2", "{tell}");
        assert_eq!(tell["revision"]["level"], "sent");
        assert_eq!(call(&world, &store, REQUESTER, "agent_tell", json!({"text": "x", "supersedes": "v1"}), "t2")["code"], "invalid_arguments");
        assert_eq!(call(&world, &store, TARGET, "agent_tell", json!({"delegation": ask_id, "text": "x"}), "t3")["code"], "delegation_not_found");
        let ack = call(&world, &store, TARGET, "delegation_progress", json!({"delegation": ask_id, "ack": "v2", "text": "switched"}), "p1");
        assert_eq!(ack["ack"], "v2", "{ack}");
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask_id].revisions[0].level, RevisionLevel::Acknowledged);
        // Answered without review, then a follow-up supersedes it.
        store.with_state(|state| state.update_ask(&ask_id, now, |ask| ask.execution = ExecutionState::Answered)).unwrap();
        let mut follow = new_ask("b", "and the examples");
        follow.follow_up_of = Some(ask_id.clone());
        let next = store.with_state(|state| state.create_ask(follow, now).unwrap().0.ask_id).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask_id].closed, Some(CloseOutcome::Superseded));
        assert_eq!(state.asks[&ask_id].superseded_by.as_deref(), Some(next.as_str()));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
