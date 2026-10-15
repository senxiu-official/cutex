//! Admission: one scheduler per target session, run by the management
//! process (asks design §5.3).
//!
//! ExternalInput drains every eligible pending item into the current turn
//! and auto-starts turns for an idle thread, racing cutex's own owner-queue
//! flush. cutex therefore owns admission:
//!
//! - **Asks wait in cutex**, never in the native pending list. An ask is
//!   admitted only when the target is online and idle, its native gate is
//!   open, it is not paused, no profile switch is pending, the boundary
//!   still allows it, and its review (if gated) revalidates.
//! - **Admission lease**: at most one turn-starting admission per session.
//!   Taken (with the attempt record and its native message id) in the same
//!   store transaction, *before* the native submit; held through the turn;
//!   released at the turn's terminal event or by reconciliation.
//! - **Shared order** across owner input and asks uses persistent admission
//!   keys; held items are skipped. Results ride along before numbered turns.
//! - **One ask per turn** through the native exclusive admission hook (N1:
//!   `thread/externalInput/submit` with `admission: "exclusiveTurn"`), which
//!   refuses with `busy` instead of queueing and returns the receipt's
//!   `turn_id`. A runtime without N1 is detected at connect; its asks are
//!   held with `runtimeUnsupported` (never submitted through the merging
//!   path), and `ifBusy: "fail"` asks fail at once.
//! - After a crash between submit and receipt the attempt stays
//!   `submitting`; the next tick queries native status by message id and
//!   never resubmits blindly.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::directory::Directory;
use super::gate::{GateInputs, GateVerdict};
use super::model::*;
use super::render::{self, RequesterLabel};
use super::store::{AskError, AskState, AskStore, RuntimeCommand};
use crate::management::v2::admission::AdmissionSnapshot;
use super::{new_id, now_rfc3339};
use crate::approvals::ApprovalStore;

/// The native exclusive admission option (N1).
pub const EXCLUSIVE_TURN: &str = "exclusiveTurn";

/// Runtime facts of a target session needed for admission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetStatus {
    /// The management process holds a live runtime connection.
    pub online: bool,
    pub runtime_generation: u64,
    /// A native turn is running (any origin).
    pub active_turn_id: Option<String>,
    /// Shared waiting order, refreshed under the ask store lock.
    pub admission: Option<AdmissionSnapshot>,
    /// Native Plan mode holds external input.
    pub plan_mode: bool,
    /// An `afterTurn` profile switch waits (admission barrier, §5.11).
    pub profile_switch_pending: bool,
    /// The runtime offers N1 (`exclusiveTurn`).
    pub exclusive_admission: bool,
    /// The target can't run (login, quota, runtime down).
    pub blocked: Option<BlockedReason>,
    /// Offline for this process, but a live runtime binding exists (started by
    /// another cutex process, e.g. the Agent Bus): reconnect, never launch (r208).
    pub reconnectable: bool,
    /// What the receiver accepts (its canonical byte policy); texts are
    /// rendered against it (input overflow §A1).
    pub receiver_limit: crate::app_server::external_input::size::ReceiverLimit,
}

/// What one native submission is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub message_id: String,
    /// `agent` source id (requester session) or `None` for the owner /
    /// cutex itself (service source `cutex-asks`).
    pub source_agent: Option<String>,
    pub event_type: &'static str,
    pub text: String,
    /// The asks this submission carries (one for an ASK, the coalesced
    /// asks of an ANSWER/QUESTION event): runtime metadata (Claude).
    pub asks: Vec<String>,
    /// The attempt of an ASK.
    pub attempt: Option<u32>,
    /// The structured view (`cutex.delegation-ask.v1` / `-results.v1`), r208.
    pub view: Option<Value>,
}

/// Source id of events cutex itself submits (owner asks, ANSWER events).
pub const CUTEX_ASKS_SOURCE: &str = "cutex-asks";

impl Submission {
    /// The native envelope of this submission for `binding`. Same inputs give
    /// the same envelope (status, reconcile and Human recovery rebuild its
    /// key). A runtime that does not advertise ExternalInput v2 gets it as v1
    /// without the view (the desktop then shows the plain text row).
    pub fn envelope(
        &self,
        binding: &crate::launch::stock::ExternalInputBinding,
        structured_view: bool,
    ) -> anyhow::Result<crate::app_server::external_input::Envelope> {
        use crate::app_server::external_input::{Delivery, Envelope, Message, Source, SourceKind};
        let source = match &self.source_agent {
            Some(agent) => Source { kind: SourceKind::Agent, id: agent.clone() },
            None => Source { kind: SourceKind::Service, id: CUTEX_ASKS_SOURCE.into() },
        };
        let mut envelope = Envelope {
            version: 1,
            owner_id: binding.owner_id.clone(),
            thread_id: binding.thread_id.clone(),
            runtime_generation: binding.runtime_generation,
            message: Message {
                id: self.message_id.clone(),
                source,
                event_type: self.event_type.to_string(),
                // r217h2: a results event for a busy requester is `soon` (read at
                // its next step); submit and status rebuild the same key from its id.
                delivery: if super::returns::is_next_step_event(&self.message_id) {
                    Delivery::Soon
                } else {
                    Delivery::AfterTurn
                },
                text: self.text.clone(),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        // r208 delegation views: v2 envelope only when the submission has one
        // (pre-upgrade attempts and events are rebuilt without it, unchanged).
        if let Some(view) = self.view.as_ref().filter(|_| structured_view).and_then(super::views::structured) {
            envelope.version = 2;
            envelope.view = Some(view);
        }
        envelope.semantic_sha256 = envelope.digest();
        envelope.validate()?;
        Ok(envelope)
    }
}

/// Result of an exclusive submission (N1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Accepted: N1 reserved a dedicated turn and returned its id.
    Started { turn_id: String },
    /// Native refused (nothing queued): `active_turn`, `pending_obligation`,
    /// `pending_turn_trigger`, `gate_closed`, `plan_mode`, `server_draining`.
    Busy { reason: String },
    /// The runtime lacks N1 (or the connection disappeared before any
    /// request was sent). Nothing was sent.
    Unsupported(String),
    /// Definitely refused. Nothing ran.
    Refused(String),
    /// The receiver's canonical byte policy rejected the message before
    /// admission: nothing ran, and the same message is never accepted, so it
    /// is never retried. `item_bytes`: about what the policy measured;
    /// `limit`: the receiver's limit, when known.
    TooLarge { item_bytes: Option<u64>, limit: Option<u64> },
    /// Sent; the outcome is unknown. Reconcile by message id.
    Uncertain(String),
}

/// Host event (owner): cutex gave up delivering a delegation message,
/// `{code, kind: "ask"|"results", askIds, target, targetName, requester,
/// detail}`; `code` is `event_too_large` or `submit_failed`. Once per
/// failure; never repeated for the same message.
pub const DELIVERY_FAILED_EVENT: &str = "cutex/ask/deliveryFailed";

/// When a retryable submit failure may be retried: `SUBMIT_RETRY_BASE_SECS`
/// doubled per failure in a row, at most `SUBMIT_RETRY_MAX_SECS`.
pub fn submit_retry_delay(failures: u32) -> chrono::Duration {
    let doublings = failures.saturating_sub(1).min(20);
    let secs = super::SUBMIT_RETRY_BASE_SECS.saturating_mul(1_i64 << doublings).min(super::SUBMIT_RETRY_MAX_SECS);
    chrono::Duration::seconds(secs)
}

/// Whether the backoff of an ask's failed submits allows a native call now.
pub fn submit_retry_due(ask: &AskRecord, now: DateTime<Utc>) -> bool {
    ask.submit_retry
        .as_ref()
        .and_then(|retry| super::parse_time(&retry.not_before))
        .is_none_or(|not_before| now >= not_before)
}

/// Count one retryable failure; no native call for the ask before
/// `not_before`. Returns the failures in a row.
fn note_submit_failure(ask: &mut AskRecord, error: &str, not_before: DateTime<Utc>) -> u32 {
    let failures = ask.submit_retry.as_ref().map_or(0, |retry| retry.failures).saturating_add(1);
    ask.submit_retry = Some(SubmitRetry {
        failures,
        not_before: now_rfc3339(not_before),
        last_error: super::truncate_chars(error, 300),
    });
    failures
}

/// The attempt's `lastError` while submits are retried.
fn retry_note(error: &str, failures: u32) -> String {
    format!(
        "{} (submit failure {failures}/{}; retried with backoff)",
        super::truncate_chars(error, 300),
        super::SUBMIT_RETRY_LIMIT
    )
}

/// `event_too_large` detail for owner, requester and clients.
pub fn too_large_detail(kind: &str, text_bytes: usize, item_bytes: Option<u64>, limit: Option<u64>, target: &str) -> String {
    let size = match item_bytes {
        Some(bytes) => format!("{text_bytes} bytes of text, about {bytes} bytes as a model item"),
        None => format!("{text_bytes} bytes of text"),
    };
    let limit = match limit {
        Some(limit) => format!("receiver canonical byte policy: {limit} bytes"),
        None => "receiver canonical byte policy".to_string(),
    };
    format!("{}: the {kind} message ({size}) is larger than {target} accepts ({limit}); nothing was delivered or run", super::EVENT_TOO_LARGE)
}

/// The ExternalInput source a submission carries (what the receiver
/// measures together with the text).
pub fn submission_source(source_agent: Option<&str>) -> crate::app_server::external_input::Source {
    use crate::app_server::external_input::size;
    match source_agent {
        Some(agent) => size::agent_source(agent),
        None => size::service_source(ASKS_SOURCE),
    }
}

/// The service source id of owner / cutex submissions.
pub const ASKS_SOURCE: &str = "cutex-asks";

/// The first render level from `floor` whose text fits `budget`
/// (input overflow §A2). `Err`: even the deepest level is too large (its
/// sizes).
pub fn fit_level(
    budget: &crate::app_server::external_input::size::Budget,
    message_id: &str,
    source_agent: Option<&str>,
    event_type: &str,
    floor: u8,
    render: &mut dyn FnMut(u8) -> anyhow::Result<String>,
) -> anyhow::Result<Result<(String, RenderInfo), RenderInfo>> {
    use crate::app_server::external_input::size::canonical_item_bytes;
    let source = submission_source(source_agent);
    let mut last = None;
    for level in floor.min(super::RENDER_LEVELS - 1)..super::RENDER_LEVELS {
        let text = render(level)?;
        let item_bytes = canonical_item_bytes(message_id, &source, event_type, &text);
        let info = RenderInfo { level, text_bytes: text.len(), item_bytes, limit: budget.limit.item_limit(), rejected: false };
        if budget.fits(item_bytes, text.len()) {
            return Ok(Ok((text, info)));
        }
        last = Some(info);
    }
    Ok(Err(last.expect("at least one render level")))
}

/// `event_too_large` detail when no render level fits.
pub fn unfit_detail(kind: &str, info: &RenderInfo, target: &str) -> String {
    format!(
        "{} even with every long part replaced by a delegation_read pointer (render level {})",
        too_large_detail(kind, info.text_bytes, Some(info.item_bytes as u64), info.limit, target),
        info.level
    )
}

/// The render level an admission starts from: a refused attempt keeps its
/// level (one deeper when the receiver rejected it as too large).
fn render_floor(ask: &AskRecord, attempt_n: u32) -> u8 {
    ask.current_attempt()
        .filter(|record| record.n == attempt_n && record.state == AttemptState::Refused)
        .and_then(|record| record.render.as_ref())
        .map(|render| render.level + u8::from(render.rejected))
        .unwrap_or(0)
}

/// The receiver rejected a submission as too large: render one level
/// deeper on the next admission if there is one (requeue), else fail.
#[allow(clippy::too_many_arguments)]
fn rejected_too_large(
    state: &mut AskState,
    target: &str,
    ask_id: &str,
    attempt_n: u32,
    kind: &str,
    text_bytes: usize,
    item_bytes: Option<u64>,
    limit: Option<u64>,
    target_label: &str,
    report: &mut TickReport,
    now: DateTime<Utc>,
) {
    let detail = too_large_detail(kind, text_bytes, item_bytes, limit, target_label);
    let level = state
        .asks
        .get(ask_id)
        .and_then(|ask| ask.attempts.iter().find(|record| record.n == attempt_n))
        .and_then(|record| record.render.as_ref())
        .map(|render| render.level);
    match level {
        Some(level) if level + 1 < super::RENDER_LEVELS => {
            refuse_attempt(state, target, ask_id, attempt_n, None, now);
            state.update_ask(ask_id, now, |ask| {
                if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt_n) {
                    if let Some(render) = record.render.as_mut() {
                        render.rejected = true;
                    }
                    record.last_error = Some(format!("{detail}; rendering again with more pointers"));
                }
            });
            report.requeued.push(ask_id.to_string());
        }
        _ => {
            fail_undeliverable(state, target, ask_id, super::EVENT_TOO_LARGE, &detail, now);
            report.failed.push(ask_id.to_string());
        }
    }
}

/// The hold an N1 `busy` refusal reason means.
pub fn busy_hold(reason: &str) -> Hold {
    match reason {
        "plan_mode" => Hold::Paused { by: PausedBy::PlanMode },
        // An interrupt from some path closed the native gate; only the
        // owner's resume reopens it.
        "gate_closed" => Hold::HeldByRuntime { native: Some("gate_closed".into()) },
        _ => Hold::TargetBusy { position: Some(1) },
    }
}

/// Native processing status of one message id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeMessageStatus {
    /// The runtime has no record of it (never submitted).
    Absent,
    Pending,
    /// Persisted in a turn (receipt).
    Persisted { turn_id: String },
    /// Held by the runtime (`PlanMode`, `Interrupted`, `RequestUncertain`,
    /// `CanonicalSizePolicy`, ...).
    Held { reason: String, turn_id: Option<String> },
    /// The status query itself failed.
    Unavailable(String),
}

/// Outcome of answering a native pending request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RespondOutcome {
    Submitted,
    /// Someone else answered first (owner client, or already resolved).
    AlreadyResolved,
    /// Not written (runtime offline, stale connection): try again later.
    NotSent(String),
    /// Written with an unknown outcome: never re-forwarded.
    Unknown(String),
}

/// The management process's runtime connections, as asks need them.
pub trait AskRuntime {
    fn target_status(&self, session: &str) -> TargetStatus;
    /// N1 submit. Implementations re-check owner input under the session's
    /// input lock and answer `Busy` when the owner got there first.
    fn submit_exclusive(&self, session: &str, generation: u64, submission: &Submission) -> SubmitOutcome;
    fn message_status(&self, session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus;
    /// N2: interrupt the turn without closing the native external-input gate.
    fn interrupt_keep_gate(&self, session: &str, turn_id: &str) -> anyhow::Result<()>;
    /// N2: reopen the native gate without a human turn.
    fn resume_gate(&self, session: &str) -> anyhow::Result<()>;
    /// A turn of the thread from history (`thread/read` with turns):
    /// `{id, status, items, error}`; `None` when unknown or unreadable.
    fn read_turn(&self, session: &str, turn_id: &str) -> Option<Value>;
    /// Bring an offline agent online (auto-wake, §8.4). Default: unsupported.
    fn wake(&self, session: &str) -> anyhow::Result<()> {
        anyhow::bail!("auto-wake is not available for {session}")
    }
    /// Start an offline explicit-launch agent again on the owner's latest
    /// reviewed launch for `requester`'s delegation (`wake::decide` allowed
    /// it; the launch rechecks the review under its own locks). Background,
    /// like [`AskRuntime::wake`]. Default: unsupported.
    /// `ask_id`: the delegation it is for (the owner's notice names it; the
    /// implementation records it only when a launch actually ran).
    fn relaunch_reviewed(&self, session: &str, requester: &str, ask_id: &str) -> anyhow::Result<()> {
        anyhow::bail!("reviewed relaunch of {session} for {ask_id} is not available (requested by {requester})")
    }
    /// Reconnect to the live runtime of `session` (never launches; r208).
    /// Implementations rate-limit and run it in the background.
    fn reconnect(&self, session: &str) -> anyhow::Result<()> {
        anyhow::bail!("reconnect is not available for {session}")
    }
    /// Steer a short note into the running turn (`soon`), not a new turn:
    /// the compaction reminder (r208). Default: unsupported.
    fn steer_note(&self, session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
        let _ = (session, generation, submission);
        SubmitOutcome::Unsupported("steer".into())
    }
    /// r217h2: the runtime of `session` takes native `soon` (delegation
    /// results for a busy requester read at its next step). Claude adapters
    /// must explicitly advertise durable next-step results. Default: no.
    fn takes_next_step(&self, session: &str, generation: u64) -> bool {
        let _ = (session, generation);
        false
    }
    /// Durable inboxes must reconcile the same identity across runtime
    /// generations. A restart never grants permission to create another event.
    fn durable_next_step(&self, session: &str) -> bool {
        let _ = session;
        false
    }
    /// Answer a native pending request (a question) through the same
    /// first-response-wins claim as owner clients (`server_requests`).
    fn respond_question(&self, session: &str, generation: u64, response_id: &str, request_id: &Value, result: &Value) -> RespondOutcome;
    /// Call a tool of one of the agent's MCP servers as the agent itself
    /// (`mcpServer/tool/call` with the thread's `_meta.threadId`): the
    /// `stop_jobs` cancel of Job Service jobs (delegation await §5).
    /// Returns the tool result (`{content, structuredContent, isError}`).
    /// Default: unsupported.
    fn call_mcp_tool(&self, session: &str, server: &str, tool: &str, arguments: &Value) -> anyhow::Result<Value> {
        let _ = (server, tool, arguments);
        anyhow::bail!("MCP tool calls are not available for {session}")
    }
}

/// Directory, gate inputs and approvals for one tick; tests replace them.
pub trait SchedulerWorld {
    fn directory(&self) -> Result<Directory, AskError>;
    fn gate_inputs(&self, requester: Option<&str>, target: &str) -> Result<GateInputs, AskError>;
    fn approvals(&self) -> Option<&ApprovalStore>;
    fn limits(&self) -> super::budget::AskLimits {
        super::budget::AskLimits::default()
    }
    /// Input overflow settings (render margin, preview size).
    fn documents(&self) -> crate::config::documents::DocumentSettings {
        crate::config::documents::DocumentSettings::default()
    }
    /// The document store long message parts were moved to (`None`: tests).
    fn document_store(&self) -> Option<crate::documents::DocumentStore> {
        None
    }
    /// Provenance facts of every agent (auto-wake policy).
    fn agent_facts(&self) -> Vec<super::children::AgentFacts> {
        Vec::new()
    }
    /// How far a bus message got (revision levels, delegation await §3.2)
    /// and the turn it went into; `None` when unknown.
    fn message_level(&self, message_id: &str) -> Option<(RevisionLevel, Option<String>)> {
        let _ = message_id;
        None
    }
    /// r217h2: how long delegation results for a busy requester wait for its
    /// turn to end before they are read at its next step; `None`: never
    /// early (wait for the turn to end). Production: the wake setting
    /// `agent_bus.wake_busy_wait_secs`; default (tests) 120 s.
    fn results_busy_wait(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(crate::agent_bus::delivery::DEFAULT_WAKE_BUSY_WAIT_SECS))
    }
    /// The artifact host the artifact tools use (`None`: the Agent Bus host
    /// on the real stores).
    fn artifact_host(&self) -> Option<&dyn crate::artifacts::host::ArtifactHost> {
        None
    }
    /// Agents whose create / replace start message is not delivered (yet),
    /// with its report (`agent_list` `first_message`).
    fn undelivered_first_messages(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        std::collections::BTreeMap::new()
    }
    /// Whether `caller` is a Director or operator (has `agent_create`).
    fn manages_project(&self, caller: &str) -> bool {
        let _ = caller;
        false
    }
}

/// The production world: files on this host.
pub struct HostWorld {
    approvals: Option<ApprovalStore>,
}

impl HostWorld {
    pub fn load() -> Self {
        Self { approvals: ApprovalStore::open_default().ok() }
    }
}

impl SchedulerWorld for HostWorld {
    fn directory(&self) -> Result<Directory, AskError> {
        Directory::load()
    }
    fn gate_inputs(&self, requester: Option<&str>, target: &str) -> Result<GateInputs, AskError> {
        GateInputs::load(requester, target)
    }
    fn approvals(&self) -> Option<&ApprovalStore> {
        self.approvals.as_ref()
    }
    fn limits(&self) -> super::budget::AskLimits {
        super::budget::AskLimits::load()
    }
    fn documents(&self) -> crate::config::documents::DocumentSettings {
        crate::config::documents::current()
    }
    fn document_store(&self) -> Option<crate::documents::DocumentStore> {
        crate::documents::DocumentStore::open_default().ok()
    }
    fn agent_facts(&self) -> Vec<super::children::AgentFacts> {
        super::children::load_facts().unwrap_or_default()
    }
    fn message_level(&self, message_id: &str) -> Option<(RevisionLevel, Option<String>)> {
        let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository().ok()?;
        let snapshot = repository.snapshot_by_message_id(message_id).ok()??;
        Some(super::revisions::level_of_snapshot(&serde_json::to_value(snapshot).ok()?))
    }
    fn results_busy_wait(&self) -> Option<std::time::Duration> {
        crate::agent_bus::delivery::wake_busy_wait()
    }
    fn undelivered_first_messages(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        crate::agent_management::first_message::undelivered_on_host()
    }
    fn manages_project(&self, caller: &str) -> bool {
        crate::agent_bus::tool_policy::roles_for_session(caller)
            .is_ok_and(|roles| roles.director || roles.operator)
    }
}

/// Why the head of a target's queue can't be admitted now, if anything.
fn runtime_hold(state: &AskState, target: &str, status: &TargetStatus, head: &AskRecord) -> Option<Hold> {
    if let Some(pause) = state.pauses.get(target) {
        return Some(super::control::pause_hold(pause));
    }
    if !status.online && status.reconnectable {
        return Some(Hold::HeldByRuntime { native: Some(HOLD_RECONNECTING.into()) });
    }
    if !status.online {
        if head.hold == Some(Hold::Blocked { by: BlockedReason::Budget }) && status.blocked.is_none() {
            return head.hold.clone();
        }
        return Some(Hold::Blocked { by: status.blocked.unwrap_or(BlockedReason::RuntimeDown) });
    }
    if let Some(blocked) = status.blocked {
        return Some(Hold::Blocked { by: blocked });
    }
    if status.plan_mode {
        return Some(Hold::Paused { by: PausedBy::PlanMode });
    }
    if state.leases.contains_key(target) || status.active_turn_id.is_some() {
        return Some(Hold::TargetBusy { position: admission_position(status, &head.ask_id) });
    }
    if status.profile_switch_pending {
        return Some(Hold::ProfileSwitchPending);
    }
    if !status.exclusive_admission {
        return Some(Hold::RuntimeUnsupported { missing: EXCLUSIVE_TURN.into() });
    }
    None
}

/// Check the runtime gate and the first eligible shared admission item.
pub fn admission_hold(state: &AskState, target: &str, status: &TargetStatus, head: &AskRecord) -> Option<Hold> {
    if let Some(hold) = runtime_hold(state, target, status, head) {
        return Some(hold);
    }
    if status.admission.as_ref().and_then(|order| admissible_head(state, target, order))
        .is_none_or(|item| item.kind != "ask" || item.id != head.ask_id)
    {
        return Some(Hold::TargetBusy { position: admission_position(status, &head.ask_id) });
    }
    None
}

fn ask_eligible(state: &AskState, target: &str, ask: &AskRecord) -> bool {
    !state.pauses.contains_key(target)
        && (ask.execution == ExecutionState::Queued || ask.resume_pending())
        && matches!(ask.hold, None | Some(Hold::TargetBusy { .. }))
}

fn admissible_head<'a>(state: &AskState, target: &str, admission: &'a AdmissionSnapshot) -> Option<&'a crate::management::v2::admission::AdmissionItem> {
    // A wake resumes an existing delegation before ordinary queued work (§2.6).
    if let Some(ask) = continuations_of(state, target).into_iter().find(|ask| ask_eligible(state, target, ask)) {
        if let Some(item) = admission.items.iter().find(|item| item.kind == "ask" && item.id == ask.ask_id) {
            return Some(item);
        }
    }
    admission.items.iter().find(|item| match item.kind.as_str() {
        "owner" => item.state == "waiting",
        "ask" => state.asks.get(&item.id).is_some_and(|ask| ask_eligible(state, target, ask)),
        _ => false,
    })
}

fn admission_position(status: &TargetStatus, ask_id: &str) -> Option<usize> {
    status.admission.as_ref()?.items.iter()
        .find(|item| item.kind == "ask" && item.id == ask_id)
        .and_then(|item| usize::try_from(item.position).ok())
}

/// Only the head owner item may start a turn; a fresh input follows all waiting items.
pub fn owner_may_flush(state: &AskState, session: &str, admission: &AdmissionSnapshot, queue_id: Option<&str>) -> bool {
    if state.leases.contains_key(session) || admission.next_step.iter().any(|item| item.eligible) {
        return false;
    }
    match (admissible_head(state, session, admission), queue_id) {
        (None, None) => true,
        (Some(head), Some(id)) => head.kind == "owner" && head.id == id && head.state == "waiting",
        _ => false,
    }
}

/// Whether cutex holds the admission lease of `session` (owner Enter queues
/// instead of steering into an ask's turn, §5.3).
pub fn lease_held(store: &AskStore, session: &str) -> bool {
    store
        .peek()
        .map(|state| state.is_some_and(|state| state.leases.contains_key(session)))
        .unwrap_or(false)
}

/// Requester label as the target sees it.
fn requester_label(directory: &Directory, ask: &AskRecord) -> RequesterLabel {
    match &ask.requester {
        Requester::Owner => RequesterLabel { name: "the owner".into(), profile_model: None, narrower: false },
        Requester::Peer { .. } => RequesterLabel { name: ask.requester.label(), profile_model: None, narrower: ask.gate.as_ref().is_some_and(|g|g.required) },
        Requester::Agent { cutex_session_id, name } => {
            let info = directory.get(cutex_session_id);
            let name = info
                .and_then(|info| info.name.clone())
                .or_else(|| name.clone())
                .unwrap_or_else(|| cutex_session_id.clone());
            let profile_model = info
                .filter(|_| directory.same_domain(cutex_session_id, &ask.target_cutex_session_id))
                .map(|info| {
                    [info.profile.as_deref(), info.model.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .filter(|label| !label.is_empty());
            RequesterLabel {
                name,
                profile_model,
                narrower: ask.gate.as_ref().is_some_and(|gate| gate.required),
            }
        }
    }
}

/// The text of the previous answer for a follow-up.
fn previous_answer(store: &AskStore, state: &AskState, ask: &AskRecord) -> Option<(String, String)> {
    let previous = state.asks.get(ask.follow_up_of.as_deref()?)?;
    let answer = state.answers.get(previous.answer_id.as_deref()?)?;
    let text = answer
        .artifact_id
        .as_deref()
        .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    Some((answer.answer_id.clone(), text))
}

/// The submission of an attempt (rebuilt identically for recovery).
pub fn attempt_submission(store: &AskStore, ask: &AskRecord, attempt: &AttemptRecord) -> Option<Submission> {
    let text = attempt
        .event_artifact
        .as_deref()
        .and_then(|artifact| store.read_artifact(artifact, None).ok().flatten())
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())?;
    Some(Submission {
        message_id: attempt.native_message_id.clone(),
        source_agent: ask.requester.session().map(str::to_string),
        event_type: render::ASK_EVENT,
        text,
        asks: vec![ask.ask_id.clone()],
        attempt: Some(attempt.n),
        view: attempt.view.clone(),
    })
}

/// What one tick did (tests and diagnostics).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub admitted: Vec<String>,
    pub reviewed: Vec<String>,
    pub requeued: Vec<String>,
    pub failed: Vec<String>,
    /// Coalesced ANSWER/QUESTION events admitted.
    pub events: Vec<String>,
    /// Requesters whose events degraded to passive (over budget).
    pub degraded: Vec<String>,
    /// Attempts ended by reconciliation (interrupted / captured late).
    pub interrupted: Vec<String>,
    /// Requesters whose result-event lease reconciliation released (owner
    /// input queued behind it may flush now).
    pub released: Vec<String>,
}

pub(super) fn fail_ask(state: &mut AskState, ask_id: &str, code: &str, now: DateTime<Utc>) {
    fail_ask_with(state, ask_id, code, None, now);
}

/// [`fail_ask`] with a `lastError` that explains `code`.
fn fail_ask_with(state: &mut AskState, ask_id: &str, code: &str, detail: Option<&str>, now: DateTime<Utc>) {
    state.update_ask(ask_id, now, |ask| {
        ask.execution = ExecutionState::Failed;
        ask.hold = None;
        if let Some(attempt) = ask.current_attempt_mut() {
            if !attempt.state.is_terminal() {
                attempt.state = AttemptState::Failed;
                attempt.ended_at = Some(now_rfc3339(now));
            }
            attempt.last_error = Some(detail.unwrap_or(code).to_string());
        }
        ask.closed = Some(CloseOutcome::Completed);
        ask.closed_at = Some(now_rfc3339(now));
    });
    if let Some(ask) = state.asks.get(ask_id).cloned() {
        super::delivery::record_state_delivery(state, &ask, code, now);
    }
}

/// The runtime never ran the ask and never will (`event_too_large`, or
/// `submit_failed` after the retry limit): release the lease this attempt
/// holds, fail the ask (the requester gets the state item) and tell the
/// owner once with [`DELIVERY_FAILED_EVENT`].
fn fail_undeliverable(state: &mut AskState, session: &str, ask_id: &str, code: &str, detail: &str, now: DateTime<Utc>) {
    let held_by_this = state.leases.get(session).is_some_and(|lease| {
        matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, .. } if id == ask_id)
    });
    if held_by_this {
        state.leases.remove(session);
    }
    fail_ask_with(state, ask_id, code, Some(detail), now);
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return;
    };
    state.notice(
        DELIVERY_FAILED_EVENT,
        vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
        json!({
            "code": code,
            "kind": "ask",
            "askIds": [ask_id],
            "target": ask.target_cutex_session_id,
            "targetName": ask.target_name,
            "requester": ask.requester.session(),
            "detail": detail,
        }),
    );
}

/// Review phase for `pendingReview` asks: observe (never consume) the
/// approval; approved → queued, denied → rejected.
fn review_pending(world: &dyn SchedulerWorld, store: &AskStore, report: &mut TickReport, now: DateTime<Utc>) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let pending: Vec<AskRecord> = state
        .asks
        .values()
        .filter(|ask| ask.remote.is_none() && ask.execution == ExecutionState::PendingReview)
        .cloned()
        .collect();
    for ask in pending {
        let inputs = match crate::peers::asks::gate_inputs(world, &ask) {
            Ok(inputs) => inputs,
            Err(error) => {
                store.with_state(|state| fail_ask(state, &ask.ask_id, error.code, now))?;
                report.failed.push(ask.ask_id.clone());
                continue;
            }
        };
        let Some(approvals) = world.approvals() else {
            continue; // fail closed: stays pendingReview
        };
        let verdict = super::gate::check(approvals, &ask, &inputs, false, now)?;
        if matches!(&verdict, GateVerdict::Review { gate, created: false, .. }
            if ask.gate.as_ref() == Some(gate) && ask.hold.is_none())
        {
            continue;
        }
        store.with_state(|state| apply_review_verdict(state, &ask.ask_id, verdict, now))?;
        report.reviewed.push(ask.ask_id.clone());
    }
    Ok(())
}

fn apply_review_verdict(state: &mut AskState, ask_id: &str, verdict: GateVerdict, now: DateTime<Utc>) {
    match verdict {
        GateVerdict::Pass(gate) => {
            state.update_ask(ask_id, now, |ask| {
                ask.gate = Some(gate);
                if ask.execution == ExecutionState::PendingReview {
                    ask.execution = ExecutionState::Queued;
                }
            });
        }
        GateVerdict::Review { gate, approval, created } => {
            state.update_ask(ask_id, now, |ask| {
                ask.gate = Some(gate);
                ask.execution = ExecutionState::PendingReview;
                ask.hold = None;
            });
            if created {
                state.notice(
                    crate::management::v2::approvals::APPROVALS_CHANGED_EVENT,
                    vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
                    json!({"reason": "requested", "approval": approval}),
                );
            }
        }
        GateVerdict::Rejected { gate, note } => {
            state.update_ask(ask_id, now, |ask| {
                ask.gate = Some(gate);
                ask.execution = ExecutionState::Rejected;
                ask.hold = None;
                if let Some(attempt) = ask.current_attempt_mut() {
                    attempt.last_error = note.clone();
                }
                ask.closed = Some(CloseOutcome::Completed);
                ask.closed_at = Some(now_rfc3339(now));
            });
            if let Some(ask) = state.asks.get(ask_id).cloned() {
                super::delivery::record_state_delivery(state, &ask, "rejected", now);
            }
        }
    }
}

/// Reconcile a lease whose attempt is still `submitting` (lost reply or a
/// crash between submit and record): query native status by message id.
fn reconcile_submitting(
    runtime: &dyn AskRuntime,
    store: &AskStore,
    lease: &AdmissionLease,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let LeaseHolder::Ask { ask_id, attempt } = &lease.holder else {
        return Ok(());
    };
    let Some(ask) = state.asks.get(ask_id) else {
        store.with_state(|state| state.leases.remove(&lease.cutex_session_id))?;
        return Ok(());
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == *attempt) else {
        return Ok(());
    };
    if record.state != AttemptState::Submitting {
        return Ok(());
    }
    // Backoff after failed status queries (the first query after a failed
    // submit is due at once).
    if !submit_retry_due(ask, now) {
        return Ok(());
    }
    let Some(submission) = attempt_submission(store, ask, record) else {
        return Ok(());
    };
    let ask_id = ask_id.clone();
    let attempt = *attempt;
    // The runtime restarted since the submit: the old occurrence can no
    // longer be queried (every status query fails). Like a bound turn of a
    // restarted runtime, the attempt ends interrupted(runtimeRestart); it is
    // never re-run blindly, and the lease no longer blocks the target.
    let target = runtime.target_status(&lease.cutex_session_id);
    if target.online && target.runtime_generation != lease.runtime_generation {
        super::observe::finalize(
            store,
            &lease.cutex_session_id,
            &ask_id,
            attempt,
            &json!({"status": "interrupted"}),
            Some(InterruptReason::RuntimeRestart),
            now,
        )?;
        report.interrupted.push(ask_id);
        return Ok(());
    }
    let status = runtime.message_status(&lease.cutex_session_id, lease.runtime_generation, &submission);
    let mut interrupted_now = false;
    store.with_state(|state| match status {
        NativeMessageStatus::Persisted { turn_id } => bind_receipt(state, &lease.cutex_session_id, &ask_id, attempt, &turn_id, now),
        NativeMessageStatus::Absent => {
            let failures = state
                .asks
                .get(&ask_id)
                .and_then(|ask| ask.submit_retry.as_ref())
                .map_or(0, |retry| retry.failures);
            if failures >= super::SUBMIT_RETRY_LIMIT {
                let last = state
                    .asks
                    .get(&ask_id)
                    .and_then(|ask| ask.submit_retry.as_ref())
                    .map(|retry| retry.last_error.clone())
                    .unwrap_or_default();
                let detail = format!(
                    "{}: the runtime refused or lost the submit {failures} times in a row (last error: {last}); nothing ran",
                    super::SUBMIT_FAILED
                );
                fail_undeliverable(state, &lease.cutex_session_id, &ask_id, super::SUBMIT_FAILED, &detail, now);
                report.failed.push(ask_id.clone());
                return;
            }
            // Never reached the runtime: safe to admit again with the same
            // attempt (nothing ran), after the backoff when the submit failed.
            refuse_attempt(state, &lease.cutex_session_id, &ask_id, attempt, None, now);
            if failures > 0 {
                state.update_ask(&ask_id, now, |ask| {
                    if let Some(retry) = ask.submit_retry.as_mut() {
                        retry.not_before = now_rfc3339(now + submit_retry_delay(failures));
                    }
                });
            }
            report.requeued.push(ask_id.clone());
        }
        NativeMessageStatus::Held { reason, turn_id } => {
            if reason == "canonical_size_policy" {
                let target = state
                    .asks
                    .get(&ask_id)
                    .and_then(|ask| ask.target_name.clone())
                    .unwrap_or_else(|| lease.cutex_session_id.clone());
                let detail = format!(
                    "{}: {target} holds the ASK message under its receiver canonical byte policy; it is not run",
                    super::EVENT_TOO_LARGE
                );
                fail_undeliverable(state, &lease.cutex_session_id, &ask_id, super::EVENT_TOO_LARGE, &detail, now);
                report.failed.push(ask_id.clone());
                return;
            }
            if reason == "plan_mode" {
                state.update_ask(&ask_id, now, |ask| ask.hold = Some(Hold::Paused { by: PausedBy::PlanMode }));
                return;
            }
            // Held after an interrupt or a crash: the attempt is over
            // (`interrupted{outcomeUnknown}`); it is never re-run blindly.
            if let Some(turn_id) = turn_id {
                bind_receipt(state, &lease.cutex_session_id, &ask_id, attempt, &turn_id, now);
            }
            interrupted_now = true;
        }
        NativeMessageStatus::Pending => {}
        NativeMessageStatus::Unavailable(error) => {
            // The status query failed (runtime unreachable): ask again after
            // a backoff; past the limit the outcome stays unknown, so the
            // attempt ends interrupted(outcomeUnknown) and frees the target.
            let failures = state
                .update_ask(&ask_id, now, |ask| {
                    let failures = ask.submit_retry.as_ref().map_or(0, |retry| retry.failures).saturating_add(1);
                    let error = format!("status query failed: {error}");
                    note_submit_failure(ask, &error, now + submit_retry_delay(failures));
                    if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                        record.last_error = Some(retry_note(&error, failures));
                    }
                    failures
                })
                .unwrap_or(0);
            interrupted_now = failures >= super::SUBMIT_RETRY_LIMIT;
        }
    })?;
    if interrupted_now {
        super::observe::finalize(
            store,
            &lease.cutex_session_id,
            &ask_id,
            attempt,
            &json!({"status": "interrupted"}),
            Some(InterruptReason::OutcomeUnknown),
            now,
        )?;
        report.interrupted.push(ask_id);
    }
    Ok(())
}

/// Reconcile a lease bound to a turn whose end the observer missed (daemon
/// or runtime restart, §5.10): the attempt is reconciled against native
/// status and terminal history. A completed turn is captured from history;
/// otherwise the attempt ends `interrupted{runtimeRestart|outcomeUnknown}`
/// with the partial message. Never re-run silently.
fn reconcile_bound(
    runtime: &dyn AskRuntime,
    store: &AskStore,
    lease: &AdmissionLease,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let (Some(turn_id), LeaseHolder::Ask { ask_id, attempt }) = (&lease.turn_id, &lease.holder) else {
        return Ok(());
    };
    let status = runtime.target_status(&lease.cutex_session_id);
    if !status.online {
        return Ok(());
    }
    let restarted = status.runtime_generation != lease.runtime_generation;
    if !restarted && status.active_turn_id.as_deref() == Some(turn_id.as_str()) {
        return Ok(());
    }
    // Give a just-reserved turn time to show up as active.
    let settled = super::parse_time(&lease.since).is_none_or(|since| now - since > chrono::Duration::seconds(BOUND_SETTLE_SECS));
    if !restarted && !settled {
        return Ok(());
    }
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id) else {
        store.with_state(|state| state.leases.remove(&lease.cutex_session_id))?;
        return Ok(());
    };
    let Some(record) = ask.attempts.iter().find(|record| record.n == *attempt).cloned() else {
        return Ok(());
    };
    if record.state.is_terminal() {
        store.with_state(|state| state.leases.remove(&lease.cutex_session_id))?;
        return Ok(());
    }
    match runtime.read_turn(&lease.cutex_session_id, turn_id) {
        Some(turn) if turn.get("status").and_then(Value::as_str) != Some("inProgress") => {
            super::observe::finalize(store, &lease.cutex_session_id, ask_id, *attempt, &turn, Some(InterruptReason::RuntimeRestart), now)?;
            report.interrupted.push(ask_id.clone());
        }
        Some(_) if !restarted => {} // still running per history
        _ => {
            // Not in history (never started, or unreadable after restart).
            if !restarted {
                if let Some(submission) = attempt_submission(store, ask, &record) {
                    if runtime.message_status(&lease.cutex_session_id, lease.runtime_generation, &submission)
                        == NativeMessageStatus::Absent
                    {
                        // The bound turn ended before the item reached
                        // context: native dropped it; admitting again is safe.
                        store.with_state(|state| refuse_attempt(state, &lease.cutex_session_id, ask_id, *attempt, None, now))?;
                        report.requeued.push(ask_id.clone());
                        return Ok(());
                    }
                }
            }
            super::observe::finalize(
                store,
                &lease.cutex_session_id,
                ask_id,
                *attempt,
                &json!({"status": "interrupted"}),
                Some(if restarted { InterruptReason::RuntimeRestart } else { InterruptReason::OutcomeUnknown }),
                now,
            )?;
            report.interrupted.push(ask_id.clone());
        }
    }
    Ok(())
}

/// Seconds a bound lease may go without its turn showing as active.
const BOUND_SETTLE_SECS: i64 = 30;

/// End a leased attempt the target can no longer stop itself: it counts as
/// stopped (`stopRequested`) and ends `interrupted(stopped)` through the
/// normal end-of-turn path ([`super::observe::finalize`]: answer record
/// with the partial message, requester delivery, `cutex/ask/answered`,
/// lease released, open questions stale). The ask closes `stopped`; a
/// review `abandoned` is kept.
pub(super) fn settle_stopped_attempt(store: &AskStore, session: &str, ask_id: &str, attempt: u32, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let open = store.with_state(|state| {
        state
            .update_ask(ask_id, now, |ask| {
                let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) else {
                    return false;
                };
                if record.state.is_terminal() {
                    return false;
                }
                record.stop_requested = true;
                record.stop_offline_since = None;
                true
            })
            .unwrap_or(false)
    })?;
    if !open {
        return Ok(false);
    }
    super::observe::finalize(store, session, ask_id, attempt, &json!({"status": "interrupted"}), Some(InterruptReason::Stopped), now)?;
    Ok(true)
}

/// Settle stops a target can no longer take (asks with an offline or
/// archived target never closed: the interrupt fails while the target is
/// offline, and bound-lease reconciliation waits for it to come back).
///
/// Rule, per leased attempt that is not over yet:
/// - target **online or reconnectable**: nothing here (the queued
///   interrupt or the reconciliation ends it; a reconnecting runtime never
///   settles early);
/// - target **archived** (retired, or no longer in the directory): the
///   attempt ends `interrupted(stopped)` at once, whether or not a stop was
///   requested (it can never run again);
/// - target **offline, not reconnectable, stop pending**: the first tick
///   that sees it records `stopOfflineSince`; once it has been seen so for
///   `asks.limits.stopOfflineGraceSecs` (default 300 s) the attempt ends
///   `interrupted(stopped)`. Seeing the target online or reconnectable in
///   between clears the mark (the grace starts over).
fn settle_unreachable_stops(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let leased: Vec<(String, String, u32)> = state
        .leases
        .values()
        .filter_map(|lease| match &lease.holder {
            LeaseHolder::Ask { ask_id, attempt } => Some((lease.cutex_session_id.clone(), ask_id.clone(), *attempt)),
            _ => None,
        })
        .collect();
    if leased.is_empty() {
        return Ok(());
    }
    // Loaded at most once per tick, and only for an offline target.
    let mut directory: Option<Option<Directory>> = None;
    let mut grace: Option<chrono::Duration> = None;
    for (session, ask_id, attempt) in leased {
        let Some(record) = state
            .asks
            .get(&ask_id)
            .and_then(|ask| ask.attempts.iter().find(|record| record.n == attempt))
        else {
            continue;
        };
        if record.state.is_terminal() {
            continue;
        }
        let status = runtime.target_status(&session);
        if status.online || status.reconnectable {
            if record.stop_offline_since.is_some() {
                store.with_state(|state| {
                    state.update_ask(&ask_id, now, |ask| {
                        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                            record.stop_offline_since = None;
                        }
                    })
                })?;
            }
            continue;
        }
        let archived = directory
            .get_or_insert_with(|| world.directory().ok())
            .as_ref()
            .is_some_and(|directory| directory.get(&session).is_none_or(|info| info.archived));
        if archived {
            if settle_stopped_attempt(store, &session, &ask_id, attempt, now)? {
                report.interrupted.push(ask_id);
            }
            continue;
        }
        if !record.stop_requested {
            continue;
        }
        let grace = *grace.get_or_insert_with(|| {
            // Capped at a year (chrono's range; "never" is not offered).
            chrono::Duration::seconds(world.limits().stop_offline_grace_secs.min(365 * 24 * 3600) as i64)
        });
        match record.stop_offline_since.as_deref().and_then(super::parse_time) {
            Some(since) if now - since >= grace => {
                if settle_stopped_attempt(store, &session, &ask_id, attempt, now)? {
                    report.interrupted.push(ask_id);
                }
            }
            Some(_) => {}
            None => {
                store.with_state(|state| {
                    state.update_ask(&ask_id, now, |ask| {
                        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                            record.stop_offline_since = Some(now_rfc3339(now));
                        }
                    })
                })?;
            }
        }
    }
    Ok(())
}

/// Bind attempt ↔ turn from a receipt.
pub fn bind_receipt(state: &mut AskState, session: &str, ask_id: &str, attempt: u32, turn_id: &str, now: DateTime<Utc>) {
    if let Some(lease) = state.leases.get_mut(session) {
        if matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt) {
            lease.turn_id = Some(turn_id.to_string());
        }
    }
    state.update_ask(ask_id, now, |ask| {
        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
            if record.state == AttemptState::Submitting {
                record.state = AttemptState::Admitted;
            }
            record.turn_id.get_or_insert_with(|| turn_id.to_string());
        }
        if matches!(ask.execution, ExecutionState::Queued | ExecutionState::Admitted) {
            ask.execution = ExecutionState::Admitted;
        }
        ask.hold = None;
        // A continuation is running now: its resume record is consumed.
        ask.resume = None;
        // The runtime took it: earlier submit failures no longer count.
        ask.submit_retry = None;
    });
}

/// Nothing ran: release the lease and queue the ask again.
fn refuse_attempt(state: &mut AskState, session: &str, ask_id: &str, attempt: u32, hold: Option<Hold>, now: DateTime<Utc>) {
    let held_by_this = state
        .leases
        .get(session)
        .is_some_and(|lease| matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt));
    if held_by_this {
        state.leases.remove(session);
    }
    state.update_ask(ask_id, now, |ask| {
        if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
            record.state = AttemptState::Refused;
            record.ended_at = Some(now_rfc3339(now));
        }
        if !ask.execution.is_terminal() {
            // A refused continuation stays running and woken (resume kept).
            ask.execution = if ask.resume.is_some() { ExecutionState::Running } else { ExecutionState::Queued };
            ask.hold = hold;
        }
    });
}

const NATIVE_BUSY_RETRY_PREFIX: &str = "native busy: ";

/// These replies prove nothing was accepted, but root idle status does not
/// expose their gate/draining condition. Probe again after five seconds rather
/// than rewriting the attempt and entire store on every scheduler tick. Zero
/// failures keeps this wait separate from the uncertain-submit failure budget.
fn refuse_busy_attempt(state: &mut AskState, session: &str, ask_id: &str, attempt: u32, reason: &str, generation: u64, now: DateTime<Utc>) {
    refuse_attempt(state, session, ask_id, attempt, Some(busy_hold(reason)), now);
    if reason == super::control::NATIVE_GATE_CLOSED {
        // Only the owner reopens a closed gate: show it as a pause instead of
        // probing it every few seconds behind the owner's own input.
        super::control::runtime_gate_pause(state, session, generation, now);
    }
    if matches!(reason, "gate_closed" | "server_draining") {
        state.update_ask(ask_id, now, |ask| {
            ask.submit_retry = Some(SubmitRetry {
                failures: 0,
                not_before: now_rfc3339(now + chrono::Duration::seconds(super::SUBMIT_RETRY_BASE_SECS)),
                last_error: format!("{NATIVE_BUSY_RETRY_PREFIX}{reason}"),
            });
        });
    }
}

fn waiting_for_native_busy_retry(ask: &AskRecord, now: DateTime<Utc>) -> bool {
    ask.current_attempt().is_some_and(|attempt| attempt.state == AttemptState::Refused)
        && ask.submit_retry.as_ref().is_some_and(|retry| retry.failures == 0
            && matches!(retry.last_error.strip_prefix(NATIVE_BUSY_RETRY_PREFIX), Some("gate_closed" | "server_draining")))
        && !submit_retry_due(ask, now)
}

/// Woken continuations / reopened asks of a target waiting for admission,
/// oldest wake first.
pub fn continuations_of(state: &AskState, target: &str) -> Vec<AskRecord> {
    let mut asks: Vec<AskRecord> = state
        .asks
        .values()
        .filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == target) && ask.resume_pending())
        .cloned()
        .collect();
    asks.sort_by(|a, b| {
        let key = |ask: &AskRecord| ask.resume.as_ref().map(|resume| resume.since.clone()).unwrap_or_default();
        key(a).cmp(&key(b)).then(a.seq.cmp(&b.seq))
    });
    asks
}

/// Compute the eligible head without modifying the store.
fn target_plan(state: &AskState, target: &str, status: &TargetStatus) -> Option<(Vec<AskRecord>, Option<String>)> {
    let queued: Vec<AskRecord> = status.admission.as_ref()?.items.iter()
        .filter(|item| item.kind == "ask")
        .filter_map(|item| state.asks.get(&item.id))
        .cloned()
        .collect();
    // Recompute runtime holds without changing records until the final reason
    // is known, so an unchanged tick does not create transient revisions.
    let eligible = |ask: &AskRecord| {
        (ask.execution == ExecutionState::Queued || ask.resume_pending())
            && runtime_hold(state, target, status, ask).is_none()
    };
    let continuation = continuations_of(state, target).into_iter().find(&eligible);
    let selected = continuation.map(|ask| ask.ask_id).or_else(|| {
        status.admission.as_ref()?.items.iter().find(|item| match item.kind.as_str() {
            "owner" => item.state == "waiting",
            "ask" => state.asks.get(&item.id).is_some_and(&eligible),
            _ => false,
        }).filter(|item| item.kind == "ask").map(|item| item.id.clone())
    });
    Some((queued, selected))
}

fn ridealongs_first(state: &AskState, target: &str, status: &TargetStatus, ask_id: &str) -> bool {
    !state.asks.get(ask_id).is_some_and(AskRecord::resume_pending)
        && !crate::management::v2::admission::owner_priority(target)
        && status.admission.as_ref().is_some_and(|order| order.next_step.iter().any(|item| item.eligible))
}

fn planned_hold(state: &AskState, target: &str, status: &TargetStatus, ask: &AskRecord, selected: Option<&str>, now: DateTime<Utc>) -> Option<Hold> {
    runtime_hold(state, target, status, ask)
        .or_else(|| waiting_for_native_busy_retry(ask, now).then(|| ask.hold.clone()).flatten())
        .or_else(|| {
        (selected != Some(ask.ask_id.as_str()))
            .then(|| Hold::TargetBusy { position: admission_position(status, &ask.ask_id) })
    })
}

fn plan_target(state: &mut AskState, target: &str, status: &TargetStatus, now: DateTime<Utc>) -> Option<String> {
    let (queued, selected) = target_plan(state, target, status)?;
    for ask in &queued {
        if ask.execution == ExecutionState::PendingReview {
            continue;
        }
        let hold = planned_hold(state, target, status, ask, selected.as_deref(), now);
        if ask.options.if_busy == IfBusy::Fail && hold.is_some() {
            fail_ask(state, &ask.ask_id, "busy", now);
            continue;
        }
        let entering_wake_hold = hold.as_ref().is_some_and(Hold::wakes_waiters) && ask.hold != hold;
        state.update_ask(&ask.ask_id, now, |record| record.hold = hold.clone());
        if entering_wake_hold {
            if let Some(updated) = state.asks.get(&ask.ask_id).cloned() {
                super::delivery::record_state_delivery(state, &updated, "held", now);
            }
        }
    }
    selected.filter(|id| !ridealongs_first(state, target, status, id)
        && state.asks.get(id).is_some_and(|ask| ask_eligible(state, target, ask)))
}

/// Admit one ask: revalidate boundary and gate, render, take the lease with
/// the attempt record, then submit through N1.
fn admit(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    directory: &Directory,
    target: &str,
    status: &TargetStatus,
    ask_id: &str,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    let Some(ask) = state.asks.get(ask_id).cloned() else {
        return Ok(());
    };
    // Its last submit failed: not before the backoff ends (FIFO: the asks
    // behind it wait too).
    if !submit_retry_due(&ask, now) {
        return Ok(());
    }
    // A woken continuation / reopened delegation: same ask, next attempt;
    // boundary and gate were checked when it was first admitted.
    let resume = if ask.resume_pending() { ask.resume.clone() } else { None };
    if let Some(resume) = resume {
        return admit_continuation(world, runtime, store, directory, target, status, &ask, &resume, report, now);
    }
    // Boundary at admission: a link revoked while queued fails the ask.
    if let Some(requester) = ask.requester.session() {
        if !directory.may_interact(requester, target) {
            store.with_state(|state| fail_ask(state, ask_id, "recipient_outside_boundary", now))?;
            report.failed.push(ask_id.to_string());
            return Ok(());
        }
    }
    // Gate revalidation; consumes an approval of this exact snapshot.
    let inputs = match crate::peers::asks::gate_inputs(world, &ask) {
        Ok(inputs) => inputs,
        Err(error) => {
            store.with_state(|state| fail_ask(state, ask_id, error.code, now))?;
            report.failed.push(ask_id.to_string());
            return Ok(());
        }
    };
    // A refused attempt (busy, unsupported, rejected as too large) never
    // ran: the approval consumed when it was admitted still covers an
    // unchanged snapshot, so its retry does not ask the owner again.
    // Nor did an attempt that failed on the exhausted usage limit and was
    // put back in the queue (it is redelivered unchanged).
    let approved_before = ask
        .current_attempt()
        .filter(|record| record.state == AttemptState::Refused || (record.state == AttemptState::Failed && ask.quota_requeues > 0))
        .and(ask.gate.clone())
        .filter(|previous| {
            previous.admitted_with_approval
                && previous.snapshot_sha256 == super::gate::evaluate(&super::gate::ask_facts(&ask), &inputs).snapshot_sha256
        });
    let gate = match (approved_before, world.approvals()) {
        (Some(gate), _) => gate,
        (None, Some(approvals)) => match super::gate::check(approvals, &ask, &inputs, true, now)? {
            GateVerdict::Pass(gate) => gate,
            other => {
                store.with_state(|state| apply_review_verdict(state, ask_id, other, now))?;
                report.reviewed.push(ask_id.to_string());
                return Ok(());
            }
        },
        (None, None) => {
            let gate = super::gate::evaluate(&super::gate::ask_facts(&ask), &inputs);
            if gate.required {
                return Ok(()); // fail closed: no approval store, no gated admission
            }
            gate
        }
    };
    let attempt_n = match ask.current_attempt() {
        Some(record) if record.state == AttemptState::Refused => record.n,
        Some(record) => record.n + 1,
        None => 1,
    };
    let interrupted_before = ask
        .attempts
        .iter()
        .any(|record| record.state == AttemptState::Interrupted);
    let previous = previous_answer(store, &state, &ask);
    // Render against the receiver's budget: the first level that fits.
    let settings = world.documents();
    let budget = status.receiver_limit.budget(settings.margin_percent);
    let message_id = new_id("cutex-ask-msg");
    let label = requester_label(directory, &ask);
    let target_label = ask.target_name.clone().unwrap_or_else(|| target.to_string());
    let mut brief_artifact = ask.body.text_artifact.clone();
    let readers: Vec<String> = ask.requester.session().into_iter().chain([target]).map(str::to_string).collect();
    let fitted = fit_level(&budget, &message_id, ask.requester.session(), render::ASK_EVENT, render_floor(&ask, attempt_n), &mut |level| {
        if level >= 2 && brief_artifact.is_none() {
            brief_artifact =
                Some(store.put_artifact(ask.body.text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(ask_id), now)?);
        }
        Ok(render::ask_text_at(
            &ask,
            attempt_n,
            &label,
            previous.as_ref().map(|(id, text)| (id.as_str(), text.as_str())),
            interrupted_before,
            render::Shape { level, preview: settings.preview_bytes as usize, brief_artifact: brief_artifact.as_deref(), limit: Some(budget.notice_limit()), note_artifact: None },
        ))
    })?;
    let (text, render_info) = match fitted {
        Ok(fitted) => fitted,
        Err(info) => {
            let detail = unfit_detail("ASK", &info, &target_label);
            store.with_state(|state| fail_undeliverable(state, target, ask_id, super::EVENT_TOO_LARGE, &detail, now))?;
            report.failed.push(ask_id.to_string());
            return Ok(());
        }
    };
    let event_artifact = store.put_artifact(text.as_bytes(), "text/plain; charset=utf-8", &[], Some(ask_id), now)?;
    let ask_view = super::views::ask_view(&ask, attempt_n, gate.required);
    let target_info = directory.get(target);
    // Take the lease and write the attempt before the native submit.
    let queue = store.queue_repository()?;
    let taken = store.with_state(|state| {
        let at_head = queue.admission_with_state(target, state).ok()
            .filter(|order| state.asks.get(ask_id).is_some_and(|ask| ask.resume_pending())
                || crate::management::v2::admission::owner_priority(target)
                || !order.next_step.iter().any(|item| item.eligible))
            .and_then(|order| admissible_head(state, target, &order).cloned())
            .is_some_and(|item| item.kind == "ask" && item.id == ask_id);
        if !at_head || state.pauses.contains_key(target) || state.leases.contains_key(target)
            || state.asks.get(ask_id).is_none_or(|ask| ask.execution != ExecutionState::Queued)
        {
            return false;
        }
        state.leases.insert(
            target.to_string(),
            AdmissionLease {
                cutex_session_id: target.to_string(),
                holder: LeaseHolder::Ask { ask_id: ask_id.to_string(), attempt: attempt_n },
                native_message_id: message_id.clone(),
                runtime_generation: status.runtime_generation,
                turn_id: None,
                since: now_rfc3339(now),
            },
        );
        state.update_ask(ask_id, now, |ask| {
            ask.gate = Some(gate.clone());
            let mut record = AttemptRecord::submitting(attempt_n, message_id.clone(), status.runtime_generation, now_rfc3339(now));
            record.event_artifact = Some(event_artifact.clone());
            record.view = Some(ask_view.clone());
            record.render = Some(render_info.clone());
            record.profile = target_info.and_then(|info| info.profile.clone());
            record.model = target_info.and_then(|info| info.model.clone());
            match ask.attempts.last_mut() {
                Some(last) if last.n == attempt_n => *last = record,
                _ => ask.attempts.push(record),
            }
            ask.execution = ExecutionState::Admitted;
            ask.hold = None;
        });
        true
    })?;
    if !taken {
        return Ok(());
    }
    let submission = Submission {
        message_id: message_id.clone(),
        source_agent: ask.requester.session().map(str::to_string),
        event_type: render::ASK_EVENT,
        text,
        asks: vec![ask.ask_id.clone()],
        attempt: Some(attempt_n),
        view: Some(ask_view),
    };
    let outcome = runtime.submit_exclusive(target, status.runtime_generation, &submission);
    store.with_state(|state| match outcome {
        SubmitOutcome::Started { turn_id } => {
            bind_receipt(state, target, ask_id, attempt_n, &turn_id, now);
            report.admitted.push(ask_id.to_string());
        }
        SubmitOutcome::Busy { reason } => {
            refuse_busy_attempt(state, target, ask_id, attempt_n, &reason, status.runtime_generation, now);
            report.requeued.push(ask_id.to_string());
        }
        SubmitOutcome::Unsupported(missing) => {
            refuse_attempt(state, target, ask_id, attempt_n, Some(Hold::RuntimeUnsupported { missing }), now);
            report.requeued.push(ask_id.to_string());
        }
        SubmitOutcome::Refused(reason) => {
            state.leases.remove(target);
            fail_ask(state, ask_id, &reason, now);
            report.failed.push(ask_id.to_string());
        }
        SubmitOutcome::TooLarge { item_bytes, limit } => {
            rejected_too_large(state, target, ask_id, attempt_n, "ASK", submission.text.len(), item_bytes, limit, &target_label, report, now);
        }
        SubmitOutcome::Uncertain(error) => {
            // Keep `submitting` and the lease; reconcile by message id (at
            // once), and admit again only after a backoff.
            state.update_ask(ask_id, now, |ask| {
                let failures = note_submit_failure(ask, &error, now);
                if let Some(record) = ask.current_attempt_mut() {
                    record.last_error = Some(retry_note(&error, failures));
                }
            });
        }
    })?;
    Ok(())
}

/// Admit the continuation of a woken (or reopened) delegation as attempt
/// n+1 of the same ask (delegation await §2.6): lease and attempt record
/// before the N1 submit, exactly like [`admit`].
#[allow(clippy::too_many_arguments)]
fn admit_continuation(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    directory: &Directory,
    target: &str,
    status: &TargetStatus,
    ask: &AskRecord,
    resume: &ResumeRecord,
    report: &mut TickReport,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let ask_id = ask.ask_id.as_str();
    let state = store.snapshot()?;
    let attempt_n = match ask.current_attempt() {
        Some(record) if record.state == AttemptState::Refused => record.n,
        Some(record) => record.n + 1,
        None => 1,
    };
    let stage_note = ask
        .attempts
        .iter()
        .find(|record| record.n == resume.from_attempt)
        .and_then(|record| record.stage_note.clone());
    let answer_revision = ask
        .answer_id
        .as_deref()
        .and_then(|answer| state.answers.get(answer))
        .map(|answer| answer.revision)
        .filter(|_| resume.reason == ResumeReason::Reopened);
    let settings = world.documents();
    let budget = status.receiver_limit.budget(settings.margin_percent);
    let message_id = new_id("cutex-ask-msg");
    let label = requester_label(directory, ask);
    let target_label = ask.target_name.clone().unwrap_or_else(|| target.to_string());
    let mut brief_artifact = ask.body.text_artifact.clone();
    let readers: Vec<String> = ask.requester.session().into_iter().chain([target]).map(str::to_string).collect();
    // A note longer than the level shows inline is stored whole as an
    // artifact of the delegation; the handler gets a preview and an explicit
    // pointer to it (never a silently cut note).
    let mut note_artifact: Option<String> = None;
    let fitted = fit_level(&budget, &message_id, ask.requester.session(), render::ASK_EVENT, render_floor(ask, attempt_n), &mut |level| {
        if level >= 1 && brief_artifact.is_none() && ask.body.text.len() > 200 {
            brief_artifact =
                Some(store.put_artifact(ask.body.text.as_bytes(), "text/markdown; charset=utf-8", &readers, Some(ask_id), now)?);
        }
        if let Some(note) = resume.note.as_deref().filter(|note| note_artifact.is_none() && render::note_is_cut(note, level)) {
            note_artifact = Some(store.put_artifact(note.as_bytes(), "text/plain; charset=utf-8", &readers, Some(ask_id), now)?);
        }
        Ok(render::continuation_text(
            ask,
            attempt_n,
            &label,
            resume,
            stage_note.as_deref(),
            answer_revision,
            render::Shape {
                level,
                preview: settings.preview_bytes as usize,
                brief_artifact: brief_artifact.as_deref(),
                limit: Some(budget.notice_limit()),
                note_artifact: note_artifact.as_deref(),
            },
        ))
    })?;
    let (text, render_info) = match fitted {
        Ok(fitted) => fitted,
        Err(info) => {
            let detail = unfit_detail("ASK continuation", &info, &target_label);
            store.with_state(|state| fail_undeliverable(state, target, ask_id, super::EVENT_TOO_LARGE, &detail, now))?;
            report.failed.push(ask_id.to_string());
            return Ok(());
        }
    };
    let event_artifact = store.put_artifact(text.as_bytes(), "text/plain; charset=utf-8", &[], Some(ask_id), now)?;
    let gated = ask.gate.as_ref().is_some_and(|gate| gate.required);
    let view = super::views::continuation_view(ask, attempt_n, gated, resume);
    let target_info = directory.get(target);
    let continuation = Continuation {
        reason: resume.reason,
        of: resume.from_attempt,
        jobs: resume.jobs.clone(),
        by: resume.by,
        note: resume.note.clone(),
    };
    let queue = store.queue_repository()?;
    let taken = store.with_state(|state| {
        let at_head = queue.admission_with_state(target, state).ok()
            .filter(|order| state.asks.get(ask_id).is_some_and(|ask| ask.resume_pending())
                || crate::management::v2::admission::owner_priority(target)
                || !order.next_step.iter().any(|item| item.eligible))
            .and_then(|order| admissible_head(state, target, &order).cloned())
            .is_some_and(|item| item.kind == "ask" && item.id == ask_id);
        if !at_head || state.pauses.contains_key(target) || state.leases.contains_key(target) || state.asks.get(ask_id).is_none_or(|ask| !ask.resume_pending()) {
            return false;
        }
        state.leases.insert(
            target.to_string(),
            AdmissionLease {
                cutex_session_id: target.to_string(),
                holder: LeaseHolder::Ask { ask_id: ask_id.to_string(), attempt: attempt_n },
                native_message_id: message_id.clone(),
                runtime_generation: status.runtime_generation,
                turn_id: None,
                since: now_rfc3339(now),
            },
        );
        state.update_ask(ask_id, now, |ask| {
            let mut record = AttemptRecord::submitting(attempt_n, message_id.clone(), status.runtime_generation, now_rfc3339(now));
            record.event_artifact = Some(event_artifact.clone());
            record.view = Some(view.clone());
            record.render = Some(render_info.clone());
            record.profile = target_info.and_then(|info| info.profile.clone());
            record.model = target_info.and_then(|info| info.model.clone());
            record.continuation = Some(continuation.clone());
            match ask.attempts.last_mut() {
                Some(last) if last.n == attempt_n => *last = record,
                _ => ask.attempts.push(record),
            }
            // Execution stays `running` through a continuation.
            ask.execution = ExecutionState::Running;
            ask.hold = None;
        });
        true
    })?;
    if !taken {
        return Ok(());
    }
    let submission = Submission {
        message_id: message_id.clone(),
        source_agent: ask.requester.session().map(str::to_string),
        event_type: render::ASK_EVENT,
        text,
        asks: vec![ask_id.to_string()],
        attempt: Some(attempt_n),
        view: Some(view),
    };
    let outcome = runtime.submit_exclusive(target, status.runtime_generation, &submission);
    store.with_state(|state| match outcome {
        SubmitOutcome::Started { turn_id } => {
            bind_receipt(state, target, ask_id, attempt_n, &turn_id, now);
            report.admitted.push(ask_id.to_string());
        }
        SubmitOutcome::Busy { reason } => {
            refuse_busy_attempt(state, target, ask_id, attempt_n, &reason, status.runtime_generation, now);
            report.requeued.push(ask_id.to_string());
        }
        SubmitOutcome::Unsupported(missing) => {
            refuse_attempt(state, target, ask_id, attempt_n, Some(Hold::RuntimeUnsupported { missing }), now);
            report.requeued.push(ask_id.to_string());
        }
        SubmitOutcome::Refused(reason) => {
            state.leases.remove(target);
            fail_ask(state, ask_id, &reason, now);
            report.failed.push(ask_id.to_string());
        }
        SubmitOutcome::TooLarge { item_bytes, limit } => {
            rejected_too_large(state, target, ask_id, attempt_n, "ASK continuation", submission.text.len(), item_bytes, limit, &target_label, report, now);
        }
        SubmitOutcome::Uncertain(error) => {
            state.update_ask(ask_id, now, |ask| {
                let failures = note_submit_failure(ask, &error, now);
                if let Some(record) = ask.current_attempt_mut() {
                    record.last_error = Some(retry_note(&error, failures));
                }
            });
        }
    })?;
    Ok(())
}

/// A `runtimeGate` pause belongs to one runtime generation: once the target
/// runs a new one (restart), its gate state is unknown again, so the pause
/// ends and the next delivery probes the gate (and pauses again if it is
/// still closed).
fn end_stale_runtime_gates(runtime: &dyn AskRuntime, store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<()> {
    let gated: Vec<(String, Option<u64>)> = store.inspect(|state| {
        state.pauses.values()
            .filter(|pause| pause.origin.as_deref() == Some(super::control::ORIGIN_RUNTIME_GATE))
            .map(|pause| (pause.cutex_session_id.clone(), pause.runtime_generation))
            .collect()
    })?;
    for (session, generation) in gated {
        let status = runtime.target_status(&session);
        if status.online && generation != Some(status.runtime_generation) {
            store.with_state(|state| {
                super::control::runtime_gate_opened(state, &session, Some(status.runtime_generation), "runtimeRestart", now)
            })?;
        }
    }
    Ok(())
}

/// Execute queued runtime commands (only the management process can).
/// A failed command stays queued for the next tick unless it can never
/// succeed (missing records).
pub fn run_commands(runtime: &dyn AskRuntime, store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<()> {
    let commands = store.snapshot()?.commands;
    for queued in commands {
        let done = match &queued.command {
            RuntimeCommand::ResumeGate { cutex_session_id } => match runtime.resume_gate(cutex_session_id) {
                Ok(()) => true,
                Err(error) => {
                    // Without N2 a human turn reopens the gate; don't retry forever.
                    let message = format!("{error:#}");
                    message.contains("unavailable") || message.contains("not connected") || message.contains("no such")
                }
            },
            other => super::commands::run(runtime, store, other, now)?,
        };
        if done {
            store.with_state(|state| state.commands.retain(|item| item.command_id != queued.command_id))?;
        }
    }
    Ok(())
}

/// The reminder steered into an attempt's turn after a compaction.
pub fn compaction_reminder_text(ask: &AskRecord) -> String {
    let mut text = format!(
        "[delegation reminder] Your context was just compacted. You are handling delegation {} '{}' from {}; the brief is:\n",
        crate::short_ids::display(&ask.ask_id),
        ask.title,
        ask.requester.label()
    );
    // A brief without an artifact is at most 4 KiB: shown whole.
    let shown = match ask.body.text_artifact {
        Some(_) => crate::app_server::external_input::size::preview(&ask.body.text, 2048),
        None => ask.body.text.as_str(),
    };
    if shown.len() == ask.body.text_bytes || ask.body.text_artifact.is_none() {
        text.push_str(shown);
    } else {
        use crate::app_server::external_input::overlength::{delegation_read_call, sender_label, Notice};
        let artifact = ask.body.text_artifact.as_deref();
        let read = |cursor| delegation_read_call(&ask.ask_id, artifact, cursor);
        let from = match ask.requester.session() {
            Some(session) => sender_label(Some(&ask.requester.label()), "agent", session),
            None => "the owner".to_string(),
        };
        let stored_as = match artifact {
            Some(artifact) => format!("{} (delegation {})", crate::short_ids::display(artifact), crate::short_ids::display(&ask.ask_id)),
            None => format!("delegation {}", crate::short_ids::display(&ask.ask_id)),
        };
        text.push_str(
            &Notice {
                what: "delegation brief",
                from: &from,
                bytes: ask.body.text_bytes,
                limit: None,
                stored_as: &stored_as,
                read: &read,
                preview: shown,
            }
            .render(),
        );
    }
    text.push_str("\nYour final message in this turn is the result of this delegation.");
    text
}

/// Most delegations listed in one requester reminder.
const REMINDER_LIST_MAX: usize = 20;

/// The reminder steered into a requester's turn after a compaction in the
/// turn that carried an ANSWER/QUESTION event (r210): which delegations the
/// event delivered, how to re-read them, and what still awaits acceptance.
pub fn event_compaction_reminder_text(state: &AskState, event: &EventOutboxRecord) -> String {
    let mut text = String::from(
        "[delegation reminder] Your context was just compacted while this turn was handling delegation results; the delivered results may no longer be in your context.\n",
    );
    let mut delivered: Vec<&DeliveryRecord> = event.delivery_ids.iter().filter_map(|id| state.deliveries.get(id)).collect();
    delivered.sort_by(|a, b| a.ask_id.cmp(&b.ask_id).then(a.delivery_id.cmp(&b.delivery_id)));
    if !delivered.is_empty() {
        text.push_str("Delivered in this turn:\n");
        for delivery in delivered.iter().take(REMINDER_LIST_MAX) {
            let ask = state.asks.get(&delivery.ask_id);
            let title = ask.map(|ask| ask.title.as_str()).unwrap_or("");
            let target = ask.and_then(|ask| ask.target_name.as_deref()).unwrap_or("");
            let what = match delivery.kind {
                DeliveryKind::Answer => format!("answer {}", delivery.ref_id),
                DeliveryKind::Question => format!("question {} (answer it with question_answer)", delivery.ref_id),
                DeliveryKind::State => format!("state {}", delivery.ref_id),
            };
            text.push_str(&format!("- {} '{}'", crate::short_ids::display(&delivery.ask_id), super::byte_prefix(title, 200)));
            if !target.is_empty() {
                text.push_str(&format!(" → {target}"));
            }
            text.push_str(&format!(": {what}\n"));
        }
        if delivered.len() > REMINDER_LIST_MAX {
            text.push_str(&format!("- … and {} more\n", delivered.len() - REMINDER_LIST_MAX));
        }
        text.push_str("Re-read each result with delegation_read(delegation=\"<delegation id>\") before you continue.\n");
    }
    let mut awaiting: Vec<&AskRecord> = state
        .asks
        .values()
        .filter(|ask| {
            ask.requester_session() == Some(event.cutex_session_id.as_str())
                && ask.is_open()
                && ask.review == Some(ReviewState::AwaitingAcceptance)
        })
        .collect();
    awaiting.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.ask_id.cmp(&b.ask_id)));
    if !awaiting.is_empty() {
        text.push_str("Your delegations awaiting your acceptance (review, then delegation_close outcome=accepted, or send a follow-up):\n");
        for ask in awaiting.iter().take(REMINDER_LIST_MAX) {
            text.push_str(&format!("- {} '{}'", crate::short_ids::display(&ask.ask_id), super::byte_prefix(&ask.title, 200)));
            if let Some(target) = &ask.target_name {
                text.push_str(&format!(" → {target}"));
            }
            text.push('\n');
        }
        if awaiting.len() > REMINDER_LIST_MAX {
            text.push_str(&format!("- … and {} more\n", awaiting.len() - REMINDER_LIST_MAX));
        }
    }
    text.truncate(text.trim_end().len());
    text
}

/// Fit a reminder (a header line plus list lines) to the receiver: drop
/// list lines from the end, saying so, until it fits. `None`: not even the
/// first line fits.
pub fn fit_note(budget: &crate::app_server::external_input::size::Budget, message_id: &str, event_type: &str, text: &str) -> Option<String> {
    let source = submission_source(None);
    let fits = |text: &str| budget.fits_text(message_id, &source, event_type, text);
    if fits(text) {
        return Some(text.to_string());
    }
    let lines: Vec<&str> = text.lines().collect();
    for keep in (1..lines.len()).rev() {
        let candidate = format!("{}
- … ({} more line(s) left out to fit; agent_list my_delegations shows everything)", lines[..keep].join("
"), lines.len() - keep);
        if fits(&candidate) {
            return Some(candidate);
        }
    }
    let first = crate::app_server::external_input::size::preview(lines.first().copied().unwrap_or_default(), 400);
    fits(first).then(|| first.to_string())
}

/// Steer one reminder per observed compaction into a requester turn that
/// carries an ANSWER/QUESTION event (r210 backstop for natives before r50).
fn send_event_compaction_reminders(runtime: &dyn AskRuntime, store: &AskStore, margin_percent: u8) -> anyhow::Result<()> {
    let state = store.snapshot()?;
    for lease in state.leases.values() {
        let LeaseHolder::Event { event_id } = &lease.holder else {
            continue;
        };
        let Some(event) = state.events.get(event_id) else {
            continue;
        };
        if !event.compaction_reminder_pending {
            continue;
        }
        let mut asks: Vec<String> = event
            .delivery_ids
            .iter()
            .filter_map(|id| state.deliveries.get(id))
            .map(|delivery| delivery.ask_id.clone())
            .collect();
        asks.sort();
        asks.dedup();
        let message_id = new_id("cutex-ask-reminder");
        let budget = runtime.target_status(&lease.cutex_session_id).receiver_limit.budget(margin_percent);
        let Some(text) = fit_note(&budget, &message_id, render::REMINDER_EVENT, &event_compaction_reminder_text(&state, event)) else {
            eprintln!("asks: compaction reminder for event {event_id} not sent: {} (even its first line)", super::EVENT_TOO_LARGE);
            let event_id = event_id.clone();
            store.with_state(|state| {
                if let Some(event) = state.events.get_mut(&event_id) {
                    event.compaction_reminder_pending = false;
                }
            })?;
            continue;
        };
        let submission = Submission {
            message_id,
            source_agent: None,
            event_type: render::REMINDER_EVENT,
            text,
            asks,
            attempt: None,
            view: None,
        };
        let outcome = runtime.steer_note(&lease.cutex_session_id, lease.runtime_generation, &submission);
        if matches!(outcome, SubmitOutcome::Busy { .. }) {
            continue; // try again next tick
        }
        if let SubmitOutcome::Unsupported(reason) | SubmitOutcome::Refused(reason) = &outcome {
            eprintln!("asks: compaction reminder for event {event_id} not sent: {reason}");
        }
        if let SubmitOutcome::TooLarge { .. } = &outcome {
            eprintln!("asks: compaction reminder for event {event_id} not sent: {}", super::EVENT_TOO_LARGE);
        }
        let event_id = event_id.clone();
        store.with_state(|state| {
            if let Some(event) = state.events.get_mut(&event_id) {
                event.compaction_reminder_pending = false;
            }
        })?;
    }
    Ok(())
}

/// Steer one reminder per observed compaction into the running attempt's
/// turn (r208 backstop: remote compaction can drop the turn's ASK item).
fn send_compaction_reminders(runtime: &dyn AskRuntime, store: &AskStore, margin_percent: u8, now: DateTime<Utc>) -> anyhow::Result<()> {
    send_event_compaction_reminders(runtime, store, margin_percent)?;
    let state = store.snapshot()?;
    for lease in state.leases.values() {
        let LeaseHolder::Ask { ask_id, attempt } = &lease.holder else {
            continue;
        };
        let Some(ask) = state.asks.get(ask_id) else {
            continue;
        };
        let Some(record) = ask.attempts.iter().find(|record| record.n == *attempt) else {
            continue;
        };
        if !record.compaction_reminder_pending || record.state.is_terminal() {
            continue;
        }
        let message_id = new_id("cutex-ask-reminder");
        let budget = runtime.target_status(&lease.cutex_session_id).receiver_limit.budget(margin_percent);
        let text = fit_note(&budget, &message_id, render::REMINDER_EVENT, &compaction_reminder_text(ask));
        let Some(text) = text else {
            eprintln!("asks: compaction reminder for {ask_id} not sent: {} (even its first line)", super::EVENT_TOO_LARGE);
            let (ask_id, attempt) = (ask_id.clone(), *attempt);
            store.with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                        record.compaction_reminder_pending = false;
                    }
                })
            })?;
            continue;
        };
        let submission = Submission {
            message_id,
            source_agent: None,
            event_type: render::REMINDER_EVENT,
            text,
            asks: vec![ask.ask_id.clone()],
            attempt: Some(*attempt),
            view: None,
        };
        let outcome = runtime.steer_note(&lease.cutex_session_id, lease.runtime_generation, &submission);
        if matches!(outcome, SubmitOutcome::Busy { .. }) {
            continue; // try again next tick
        }
        if let SubmitOutcome::Unsupported(reason) | SubmitOutcome::Refused(reason) = &outcome {
            eprintln!("asks: compaction reminder for {ask_id} not sent: {reason}");
        }
        if let SubmitOutcome::TooLarge { .. } = &outcome {
            eprintln!("asks: compaction reminder for {ask_id} not sent: {}", super::EVENT_TOO_LARGE);
        }
        let sent = matches!(outcome, SubmitOutcome::Started { .. } | SubmitOutcome::Uncertain(_));
        let (ask_id, attempt) = (ask_id.clone(), *attempt);
        store.with_state(|state| {
            state.update_ask(&ask_id, now, |ask| {
                if let Some(record) = ask.attempts.iter_mut().find(|record| record.n == attempt) {
                    record.compaction_reminder_pending = false;
                    if sent {
                        record.compaction_reminders += 1;
                    }
                }
            })
        })?;
    }
    Ok(())
}

/// Resolve runtime status before taking the global ask-store write lock. A
/// status lookup can connect to native and wait for an RPC; holding the store
/// during it blocks owner input and unrelated sessions too.
fn fall_through_questions(store: &AskStore, offline: impl Fn(&str) -> bool, now: DateTime<Utc>) -> anyhow::Result<()> {
    let (has_open_questions, requesters) = store.inspect(|state| {
        let mut requesters = std::collections::BTreeSet::new();
        let mut has_open_questions = false;
        for question in state.questions.values().filter(|question| question.state == QuestionState::Open && question.routed_to_owner_at.is_none()) {
            has_open_questions = true;
            if let Some(requester) = state.asks.get(&question.ask_id).and_then(|ask| ask.requester.session()) {
                requesters.insert(requester.to_owned());
            }
        }
        (has_open_questions, requesters)
    })?;
    if has_open_questions {
        let offline: std::collections::BTreeMap<_, _> = requesters.into_iter().map(|requester| {
            let unavailable = offline(&requester);
            (requester, unavailable)
        }).collect();
        // Recheck current question state under the lock. An unobserved requester
        // (added while probes ran) is not presumed offline; its next tick probes
        // it. Expired questions still fall through independently of status.
        store.with_state(|state| super::questions::fall_through(state, &|requester| offline.get(requester).copied().unwrap_or(false), now))?;
    }
    Ok(())
}

/// One scheduler pass over every target with waiting asks or a lease.
pub fn tick(world: &dyn SchedulerWorld, runtime: &dyn AskRuntime, store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<TickReport> {
    let mut report = TickReport::default();
    run_commands(runtime, store, now)?;
    end_stale_runtime_gates(runtime, store, now)?;
    // Questions fall through to the owner after the timeout or when the
    // requester is offline (§5.9).
    fall_through_questions(store, |requester| !runtime.target_status(requester).online, now)?;
    review_pending(world, store, &mut report, now)?;
    let state = store.snapshot()?;
    for lease in state.leases.values() {
        reconcile_submitting(runtime, store, lease, &mut report, now)?;
        reconcile_bound(runtime, store, lease, &mut report, now)?;
    }
    settle_unreachable_stops(world, runtime, store, &mut report, now)?;
    send_compaction_reminders(runtime, store, world.documents().margin_percent, now)?;
    // Await timeouts (and waits met while the turn still ran).
    if super::awaits::any_due(&store.snapshot()?, now) {
        store.with_state(|state| super::awaits::wake_due(state, now))?;
    }
    super::revisions::reconcile(world, store, now)?;
    let pending = store.snapshot()?;
    // Existing ridealongs run first. Otherwise deliver results generated by
    // this pass at its end; never reconcile the same uncertain send twice.
    let results_first = !super::delivery::requesters_with_pending(&pending).is_empty()
        || pending.leases.values().any(|lease| matches!(lease.holder, LeaseHolder::Event { .. }))
        || !pending.next_step_leases.is_empty();
    if results_first {
        super::returns::admit_events(world, runtime, store, &mut report, now)?;
    }
    let state = store.snapshot()?;
    let mut targets: Vec<String> = state
        .asks
        .values()
        .filter(|ask| ask.remote.is_none() && (ask.execution == ExecutionState::Queued || ask.resume_pending()))
        .map(|ask| ask.target_cutex_session_id.clone())
        .collect();
    targets.sort();
    targets.dedup();
    // A target with nothing queued blocks nobody any more: forget who was
    // told it needs the owner, so a later block notifies again (the online
    // path below only runs for targets that still have queued asks).
    let unblocked: Vec<String> = state
        .owner_action
        .keys()
        .cloned()
        .chain(state.owner_action_told.keys().filter_map(|key| key.split_once('\u{1f}').map(|(_, target)| target.to_string())))
        .filter(|target| targets.binary_search(target).is_err())
        .collect();
    if !unblocked.is_empty() {
        store.with_state(|state| {
            for target in &unblocked {
                super::wake::clear_owner_action(state, target);
            }
        })?;
    }
    let directory = if targets.is_empty() { None } else { world.directory().ok() };
    let mut provenance = None;
    for target in targets {
        let Some(directory) = directory.as_ref() else {
            break;
        };
        if directory.get(&target).is_none_or(|info| info.archived) {
            let ids: Vec<String> = state
                .waiting_for_target(&target)
                .iter()
                .map(|ask| ask.ask_id.clone())
                .chain(continuations_of(&state, &target).into_iter().map(|ask| ask.ask_id))
                .collect();
            store.with_state(|state| {
                for id in &ids {
                    fail_ask(state, id, "recipient_archived", now);
                }
            })?;
            report.failed.extend(ids);
            continue;
        }
        let mut status = runtime.target_status(&target);
        let queue = store.queue_repository()?;
        let snapshot = store.snapshot()?;
        status.admission = Some(if status.online {
            queue.admission_with_state(&target, &snapshot)?
        } else {
            queue.numbered_admission_with_state(&target, &snapshot)?
        });
        let (planned, selected) = target_plan(&snapshot, &target, &status).unwrap_or_default();
        let changed = planned.iter().filter(|ask| ask.execution != ExecutionState::PendingReview).any(|ask| {
            let hold = planned_hold(&snapshot, &target, &status, ask, selected.as_deref(), now);
            ask.hold != hold || (ask.options.if_busy == IfBusy::Fail && hold.is_some())
        });
        let head = if changed {
            store.with_state(|state| -> anyhow::Result<_> {
                status.admission = Some(if status.online {
                    queue.admission_with_state(&target, state)?
                } else {
                    queue.numbered_admission_with_state(&target, state)?
                });
                Ok(plan_target(state, &target, &status, now))
            })??
        } else {
            selected.filter(|id| !ridealongs_first(&snapshot, &target, &status, id))
        };
        if let Some(ask_id) = head {
            admit(world, runtime, store, directory, &target, &status, &ask_id, &mut report, now)?;
        } else {
            if status.online && state.owner_action.contains_key(&target) {
                store.with_state(|state| super::wake::clear_owner_action(state, &target))?;
            }
            if !status.online && status.reconnectable {
                if let Err(error) = runtime.reconnect(&target) {
                    eprintln!("asks: reconnect of {target} failed: {error:#}");
                }
                continue;
            }
            maybe_wake(world, runtime, store, directory, &target, &status, &mut provenance, now)?;
        }
    }
    if !results_first {
        super::returns::admit_events(world, runtime, store, &mut report, now)?;
    }
    Ok(report)
}

/// The head ask of an offline target may auto-wake it (§8.4).
#[allow(clippy::too_many_arguments)]
fn maybe_wake(
    world: &dyn SchedulerWorld,
    runtime: &dyn AskRuntime,
    store: &AskStore,
    directory: &Directory,
    target: &str,
    status: &TargetStatus,
    provenance: &mut Option<std::collections::BTreeMap<String, super::children::Provenance>>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    if status.online || status.blocked.is_some() {
        return Ok(());
    }
    let state = store.snapshot()?;
    // Waiting asks in admission order. The first ask whose requester may
    // start the target decides (2026-10-14 e2e: an owner's or a peer's ask
    // at the head, which may not start an explicit-launch worker, must not
    // keep its Director's later delegation from the reviewed relaunch that
    // `agent_delegate` already promised it); otherwise the head's reason.
    let waiting: Vec<AskRecord> = status
        .admission
        .as_ref()
        .map(|admission| {
            admission
                .items
                .iter()
                .filter(|item| item.kind == "ask")
                .filter_map(|item| state.asks.get(&item.id))
                .filter(|ask| ask.execution == ExecutionState::Queued || ask.resume_pending())
                .filter(|ask| matches!(ask.hold, Some(Hold::Blocked { by: BlockedReason::RuntimeDown | BlockedReason::Budget })))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // The head itself must be waiting for the runtime (as before).
    let head_waits = status
        .admission
        .as_ref()
        .and_then(|admission| admission.items.iter().find(|item| item.kind == "ask"))
        .is_some_and(|item| waiting.first().is_some_and(|ask| ask.ask_id == item.id));
    if !head_waits || super::wake::recently_woken(&state, target, now) {
        return Ok(());
    }
    let provenance = provenance.get_or_insert_with(|| super::children::provenance(&world.agent_facts()));
    let archived = directory.get(target).is_none_or(|info| info.archived);
    let limits = world.limits();
    let decide = |ask: &AskRecord| {
        super::wake::decide(&state, provenance, ask, archived, limits.agent_started_online_per_domain, &|session| {
            state.presence.get(session).is_some_and(|presence| presence.online)
        })
    };
    let (head, decision) = waiting
        .iter()
        .map(|ask| (ask, decide(ask)))
        .find(|(_, decision)| !matches!(decision, super::wake::WakeDecision::Never(_)))
        .unwrap_or_else(|| (&waiting[0], decide(&waiting[0])));
    let head = head.clone();
    match decision {
        super::wake::WakeDecision::Wake { domain } => {
            store.with_state(|state| {
                super::wake::record_wake(state, target, &domain, now);
                state.update_ask(&head.ask_id, now, |ask| ask.hold = Some(Hold::Blocked { by: BlockedReason::RuntimeDown }));
            })?;
            if let Err(error) = runtime.wake(target) {
                eprintln!("asks: auto-wake of {target} failed: {error:#}");
            }
        }
        super::wake::WakeDecision::RelaunchReviewed { domain, requester } => {
            store.with_state(|state| {
                super::wake::record_wake(state, target, &domain, now);
                // The owner's `reviewedRelaunch` notice is recorded by the
                // runtime once a launch actually ran (not when the owner's
                // own start got there first).
                state.update_ask(&head.ask_id, now, |ask| ask.hold = Some(Hold::Blocked { by: BlockedReason::RuntimeDown }));
            })?;
            if let Err(error) = runtime.relaunch_reviewed(target, &requester, &head.ask_id) {
                eprintln!("asks: reviewed relaunch of {target} for {requester} failed: {error:#}");
            }
        }
        super::wake::WakeDecision::OverBudget { domain } => {
            store.with_state(|state| super::wake::hold_over_budget(state, &head.ask_id, &domain, now))?;
        }
        super::wake::WakeDecision::Never(reason) => {
            // The owner paused it (and is told by the pause) or archived it
            // (the tick fails those asks); otherwise only the owner can help.
            let known = state.owner_action.get(target);
            let all_known = state
                .waiting_for_target(target)
                .iter()
                .filter(|ask| ask.execution == ExecutionState::Queued)
                .all(|ask| known.is_some_and(|known| known.contains(&ask.ask_id)));
            if !all_known && !matches!(reason, "ownerPaused" | "archived") {
                store.with_state(|state| super::wake::record_owner_action(state, target, reason, None, now))?;
            }
        }
    }
    Ok(())
}

/// A JSON summary of what admission waits for on a target (diagnostics).
pub fn describe_target(state: &AskState, target: &str) -> Value {
    json!({
        "lease": state.leases.get(target),
        "pause": state.pauses.get(target),
        "waiting": state.waiting_for_target(target).iter().map(|ask| &ask.ask_id).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent_management::envelope::SandboxLevel;
    use crate::asks::directory::tests::directory;
    use crate::asks::gate::tests::{inputs, party};
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER, TARGET};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    fn pending_question(id: &str, ask_id: &str, now: DateTime<Utc>) -> QuestionRecord {
        QuestionRecord {
            question_id: id.into(), ask_id: ask_id.into(), attempt: 1, runtime_generation: 1,
            native: NativeQuestionRef { thread_id: "thread".into(), turn_id: "turn".into(), item_id: id.into(), request_id: json!(id) },
            questions: json!([]), route: QuestionRoute::Requester, routed_to_owner_at: None,
            state: QuestionState::Open, resolution: None, created_at: now_rfc3339(now),
        }
    }

    #[test]
    fn question_status_probes_release_store_lock_and_deduplicate_requesters() {
        let (store, dir) = temp_store("question-status-lock");
        let now = Utc::now();
        store.with_state(|state| {
            let ask_id = state.create_ask(new_ask("question-status", "question"), now).unwrap().0.ask_id;
            for id in ["q1", "q2"] {
                state.questions.insert(id.into(), pending_question(id, &ask_id, now));
            }
        }).unwrap();
        let calls = std::cell::Cell::new(0);
        fall_through_questions(&store, |requester| {
            assert_eq!(requester, REQUESTER);
            calls.set(calls.get() + 1);
            let lock = std::fs::OpenOptions::new().read(true).write(true).open(dir.join("asks-v1.lock")).unwrap();
            // A nonblocking exclusive probe fails deterministically if either a
            // read or write store lock is retained during the runtime lookup.
            lock.try_lock().expect("runtime status must run outside the ask-store lock");
            drop(lock);
            true
        }, now).unwrap();
        assert_eq!(calls.get(), 1);
        let state = store.snapshot().unwrap();
        assert!(state.questions.values().all(|question| question.routed_to_owner_at.is_some()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn question_status_probes_recheck_resolved_and_new_requesters() {
        let (store, dir) = temp_store("question-status-race");
        let now = Utc::now();
        store.with_state(|state| {
            let ask_id = state.create_ask(new_ask("question-race", "question"), now).unwrap().0.ask_id;
            for id in ["unchanged", "resolved"] {
                state.questions.insert(id.into(), pending_question(id, &ask_id, now));
            }
        }).unwrap();
        fall_through_questions(&store, |requester| {
            assert_eq!(requester, REQUESTER);
            // Simulate changes while native status is in flight. Check the
            // lock before writing so a regression fails instead of deadlocking.
            let lock = std::fs::OpenOptions::new().read(true).write(true).open(dir.join("asks-v1.lock")).unwrap();
            lock.try_lock().expect("runtime probe must not retain the store lock");
            drop(lock);
            store.with_state(|state| {
                state.questions.get_mut("resolved").unwrap().state = QuestionState::Resolved;
                let mut ask = new_ask("concurrent-question", "new question");
                ask.requester = Requester::Agent { cutex_session_id: "unobserved-requester".into(), name: None };
                let ask_id = state.create_ask(ask, now).unwrap().0.ask_id;
                state.questions.insert("new".into(), pending_question("new", &ask_id, now));
            }).unwrap();
            true
        }, now).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.questions["unchanged"].routed_to_owner_at.is_some());
        assert!(state.questions["resolved"].routed_to_owner_at.is_none());
        assert!(state.questions["new"].routed_to_owner_at.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn question_status_probes_preserve_timeout_and_skip_routed_questions() {
        let (store, dir) = temp_store("question-status-timeout");
        let now = Utc::now();
        store.with_state(|state| {
            let ask_id = state.create_ask(new_ask("question-timeout", "question"), now).unwrap().0.ask_id;
            let old = now - chrono::Duration::seconds(super::super::QUESTION_TIMEOUT_SECS);
            state.questions.insert("expired".into(), pending_question("expired", &ask_id, old));
        }).unwrap();
        fall_through_questions(&store, |_| false, now).unwrap();
        assert!(store.snapshot().unwrap().questions["expired"].routed_to_owner_at.is_some());
        fall_through_questions(&store, |_| panic!("already routed questions need no status probe"), now).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    pub struct FakeWorld {
        pub directory: Directory,
        pub inputs: RefCell<GateInputs>,
        pub approvals: ApprovalStore,
        pub facts: Vec<crate::asks::children::AgentFacts>,
        /// Artifact tools run on this host (temporary store, fixed paths).
        pub artifacts: Option<std::rc::Rc<crate::artifacts::host::tests::TestHost>>,
    }

    impl SchedulerWorld for FakeWorld {
        fn directory(&self) -> Result<Directory, AskError> {
            Ok(self.directory.clone())
        }
        fn gate_inputs(&self, requester: Option<&str>, _target: &str) -> Result<GateInputs, AskError> {
            let mut inputs = self.inputs.borrow().clone();
            if requester.is_none() {
                inputs.requester = None;
            }
            Ok(inputs)
        }
        fn approvals(&self) -> Option<&ApprovalStore> {
            Some(&self.approvals)
        }
        fn agent_facts(&self) -> Vec<crate::asks::children::AgentFacts> {
            self.facts.clone()
        }
        fn document_store(&self) -> Option<crate::documents::DocumentStore> {
            Some(crate::documents::DocumentStore::at(std::env::temp_dir().join("cutex-asks-fake-documents")))
        }
        fn artifact_host(&self) -> Option<&dyn crate::artifacts::host::ArtifactHost> {
            self.artifacts.as_deref().map(|host| host as &dyn crate::artifacts::host::ArtifactHost)
        }
    }

    #[derive(Default)]
    pub struct FakeRuntime {
        pub status: RefCell<BTreeMap<String, TargetStatus>>,
        pub submitted: RefCell<Vec<(String, Submission)>>,
        /// Next outcomes (default: a receipt with a fresh turn id).
        pub outcomes: RefCell<Vec<SubmitOutcome>>,
        pub native: RefCell<BTreeMap<String, NativeMessageStatus>>,
        pub interrupts: RefCell<Vec<String>>,
        pub resumes: RefCell<Vec<String>>,
        pub responses: RefCell<Vec<(String, Value, Value)>>,
        pub turns: RefCell<BTreeMap<String, Value>>,
        pub wakes: RefCell<Vec<String>>,
        /// `(target, requester)` of reviewed relaunches.
        pub relaunches: RefCell<Vec<(String, String)>>,
        /// The ask of each reviewed relaunch.
        pub relaunch_asks: RefCell<Vec<String>>,
        pub reconnects: RefCell<Vec<String>>,
        pub steered: RefCell<Vec<(String, Submission)>>,
        /// `(session, tool, arguments)` of MCP tool calls (stop_jobs).
        pub mcp_calls: RefCell<Vec<(String, String, Value)>>,
        /// Native status queries (`message_status`) made.
        pub status_queries: RefCell<usize>,
        /// r217h2: the runtime takes `soon` (results at the next step).
        pub next_step: std::cell::Cell<bool>,
        pub durable_next_step: std::cell::Cell<bool>,
    }

    impl FakeRuntime {
        pub fn idle(sessions: &[&str]) -> Self {
            let runtime = Self::default();
            for session in sessions {
                runtime.status.borrow_mut().insert(
                    session.to_string(),
                    TargetStatus { online: true, runtime_generation: 1, exclusive_admission: true, ..TargetStatus::default() },
                );
            }
            runtime
        }
        pub fn set(&self, session: &str, update: impl FnOnce(&mut TargetStatus)) {
            update(self.status.borrow_mut().entry(session.to_string()).or_default());
        }
    }

    impl AskRuntime for FakeRuntime {
        fn durable_next_step(&self, _session: &str) -> bool {
            self.durable_next_step.get()
        }
        fn target_status(&self, session: &str) -> TargetStatus {
            self.status.borrow().get(session).cloned().unwrap_or_default()
        }
        fn submit_exclusive(&self, session: &str, _generation: u64, submission: &Submission) -> SubmitOutcome {
            self.submitted.borrow_mut().push((session.to_string(), submission.clone()));
            let outcome = if self.outcomes.borrow().is_empty() {
                SubmitOutcome::Started { turn_id: format!("turn-{}", self.submitted.borrow().len()) }
            } else {
                self.outcomes.borrow_mut().remove(0)
            };
            if let SubmitOutcome::Started { turn_id } = &outcome {
                self.native
                    .borrow_mut()
                    .insert(submission.message_id.clone(), NativeMessageStatus::Persisted { turn_id: turn_id.clone() });
                self.set(session, |status| status.active_turn_id = Some(turn_id.clone()));
            }
            outcome
        }
        fn message_status(&self, _session: &str, _generation: u64, submission: &Submission) -> NativeMessageStatus {
            *self.status_queries.borrow_mut() += 1;
            self.native
                .borrow()
                .get(&submission.message_id)
                .cloned()
                .unwrap_or(NativeMessageStatus::Absent)
        }
        fn interrupt_keep_gate(&self, session: &str, turn_id: &str) -> anyhow::Result<()> {
            if !self.target_status(session).online {
                anyhow::bail!("{session} is not connected");
            }
            self.interrupts.borrow_mut().push(format!("{session}:{turn_id}"));
            Ok(())
        }
        fn resume_gate(&self, session: &str) -> anyhow::Result<()> {
            self.resumes.borrow_mut().push(session.to_string());
            Ok(())
        }
        fn wake(&self, session: &str) -> anyhow::Result<()> {
            self.wakes.borrow_mut().push(session.to_string());
            Ok(())
        }
        fn relaunch_reviewed(&self, session: &str, requester: &str, ask_id: &str) -> anyhow::Result<()> {
            self.relaunches.borrow_mut().push((session.to_string(), requester.to_string()));
            self.relaunch_asks.borrow_mut().push(ask_id.to_string());
            Ok(())
        }
        fn reconnect(&self, session: &str) -> anyhow::Result<()> {
            self.reconnects.borrow_mut().push(session.to_string());
            Ok(())
        }
        fn steer_note(&self, session: &str, _generation: u64, submission: &Submission) -> SubmitOutcome {
            self.steered.borrow_mut().push((session.to_string(), submission.clone()));
            // Accepted, not read yet (a test sets the receipt).
            self.native
                .borrow_mut()
                .entry(submission.message_id.clone())
                .or_insert(NativeMessageStatus::Pending);
            SubmitOutcome::Started { turn_id: "steered".into() }
        }
        fn takes_next_step(&self, _session: &str, _generation: u64) -> bool {
            self.next_step.get()
        }
        fn read_turn(&self, _session: &str, turn_id: &str) -> Option<Value> {
            self.turns.borrow().get(turn_id).cloned()
        }
        fn respond_question(&self, session: &str, _generation: u64, _response_id: &str, request_id: &Value, result: &Value) -> RespondOutcome {
            self.responses.borrow_mut().push((session.to_string(), request_id.clone(), result.clone()));
            RespondOutcome::Submitted
        }
        fn call_mcp_tool(&self, session: &str, server: &str, tool: &str, arguments: &Value) -> anyhow::Result<Value> {
            assert_eq!(server, crate::asks::awaits::JOB_SERVER);
            self.mcp_calls.borrow_mut().push((session.to_string(), tool.to_string(), arguments.clone()));
            Ok(match tool {
                "query" => json!({"structuredContent": {"jobId": arguments["jobId"], "revision": 4, "state": "running"}}),
                _ => json!({"structuredContent": {"status": "accepted"}}),
            })
        }
    }

    pub fn world(label: &str) -> (FakeWorld, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-asks-world-{label}-{}", uuid::Uuid::new_v4().simple()));
        let world = FakeWorld {
            directory: directory(&[(REQUESTER, "Eva", Some("alpha")), (TARGET, "Kimi", Some("alpha"))]),
            inputs: RefCell::new(inputs(
                Some(party(REQUESTER, SandboxLevel::FullAccess, &[], "chatgpt")),
                party(TARGET, SandboxLevel::WorkspaceWrite, &["/w"], "deepseek"),
            )),
            approvals: ApprovalStore::at(dir.join("approvals.json")),
            facts: Vec::new(),
            artifacts: None,
        };
        (world, dir)
    }

    /// Finish the running turn of `session` in the fake runtime and in the
    /// store (what the runtime-event observer does).
    pub fn finish_turn(runtime: &FakeRuntime, store: &AskStore, session: &str) {
        runtime.set(session, |status| status.active_turn_id = None);
        store.with_state(|state| state.leases.remove(session)).unwrap();
    }

    /// Acceptance §12.3 item 1 (admission): two asks plus an earlier queued
    /// owner message on a busy target run in FIFO order, exactly one ask per
    /// turn, each bound to its receipt's turn.
    #[test]
    fn fifo_one_ask_per_turn_with_owner_input_first() {
        let (world, dir) = world("fifo");
        let (store, store_dir) = temp_store("fifo");
        let runtime = FakeRuntime::idle(&[TARGET]);
        runtime.set(TARGET, |status| {
            status.active_turn_id = Some("owner-turn".into());
        });
        let queue = store.queue_repository().unwrap();
        let owner_params = crate::management::v2::user_input::parse_user_input_submit_params(&json!({
            "clientUserMessageId":"owner-first", "origin":{"kind":"other","clientId":"test"},
            "strategy":"queue", "input":[{"type":"text","text":"owner"}]
        })).unwrap();
        queue.enqueue(TARGET, "thread", "owner-request", &owner_params).unwrap();
        let owner = queue.list(TARGET).unwrap().1.remove(0);
        let now = Utc::now();
        let (first, second) = store
            .with_state(|state| {
                let a = state.create_ask(new_ask("a", "first"), now).unwrap().0.ask_id;
                let b = state.create_ask(new_ask("b", "second"), now).unwrap().0.ask_id;
                (a, b)
            })
            .unwrap();
        // Busy: both held with positions, nothing submitted.
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert!(report.admitted.is_empty());
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&first].hold, Some(Hold::TargetBusy { position: Some(2) }));
        assert_eq!(state.asks[&second].hold, Some(Hold::TargetBusy { position: Some(3) }));
        let unchanged = serde_json::to_value(&state).unwrap();
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
        assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(), unchanged);
        // The turn ends but the owner's earlier item flushes first.
        runtime.set(TARGET, |status| status.active_turn_id = None);
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
        let state = store.snapshot().unwrap();
        let admission = queue.admission_with_state(TARGET, &state).unwrap();
        assert!(owner_may_flush(&state, TARGET, &admission, Some(&owner.queue_id)));
        assert!(!owner_may_flush(&state, TARGET, &admission, None));
        queue.remove(TARGET, &owner.queue_id, owner.revision).unwrap();
        // Now the first ask, alone.
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![first.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&first].execution, ExecutionState::Admitted);
        assert_eq!(state.asks[&first].attempts[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(state.leases[TARGET].turn_id.as_deref(), Some("turn-1"));
        assert!(!owner_may_flush(&state, TARGET, &queue.admission_with_state(TARGET, &state).unwrap(), None));
        assert_eq!(runtime.submitted.borrow().len(), 1);
        assert!(runtime.submitted.borrow()[0].1.text.contains("first"));
        // Runtime metadata (Claude receipt mapping): the ask and attempt.
        assert_eq!(runtime.submitted.borrow()[0].1.asks, vec![first.clone()]);
        assert_eq!(runtime.submitted.borrow()[0].1.attempt, Some(1));
        // While its turn runs the second waits.
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
        finish_turn(&runtime, &store, TARGET);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![second.clone()]);
        assert_eq!(runtime.submitted.borrow().len(), 2);
        assert_eq!(store.snapshot().unwrap().asks[&second].attempts[0].turn_id.as_deref(), Some("turn-2"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    fn queued_owner(store: &AskStore, id: &str) -> crate::management::v2::user_input::UserInputQueueItem {
        let queue = store.queue_repository().unwrap();
        let params = crate::management::v2::user_input::parse_user_input_submit_params(&json!({
            "clientUserMessageId":id, "origin":{"kind":"other","clientId":"test"},
            "strategy":"queue", "input":[{"type":"text","text":id}]
        })).unwrap();
        queue.enqueue(TARGET, "thread", id, &params).unwrap();
        queue.list(TARGET).unwrap().1.into_iter().find(|item| item.client_user_message_id == id).unwrap()
    }

    fn queued_answer(store: &AskStore, id: &str, now: DateTime<Utc>) -> String {
        store.with_state(|state| {
            let mut source = new_ask(id, "answer source");
            source.requester = Requester::Agent { cutex_session_id: TARGET.into(), name: None };
            source.target_cutex_session_id = REQUESTER.into();
            let ask_id = state.create_ask(source, now).unwrap().0.ask_id;
            let ask = state.asks.get_mut(&ask_id).unwrap();
            ask.execution = ExecutionState::Answered;
            ask.answer_id = Some(id.into());
            let ask = ask.clone();
            state.answers.insert(id.into(), AnswerRecord {
                answer_id: id.into(), ask_id, attempt: 1, revision: 1,
                status: AnswerStatus::Completed, artifact_id: None, bytes: 0,
                metadata: AnswerMetadata::default(), created_at: now_rfc3339(now), preview: None,
            });
            super::super::delivery::record_answer_delivery(state, &ask, id, now).unwrap()
        }).unwrap()
    }

    #[test]
    fn stalled_ridealong_yields_to_waiting_owner_at_deadline() {
        let (store, dir) = temp_store("stalled-ridealong");
        let session = format!("stalled-ridealong-{}", uuid::Uuid::new_v4());
        let queue = store.queue_repository().unwrap();
        let params = crate::management::v2::user_input::parse_user_input_submit_params(&json!({
            "clientUserMessageId":"owner", "origin":{"kind":"other","clientId":"test"},
            "strategy":"queue", "input":[{"type":"text","text":"owner"}]
        })).unwrap();
        queue.enqueue(&session, "thread", "owner", &params).unwrap();
        let owner = queue.list(&session).unwrap().1.remove(0);
        let now = Utc::now();
        store.with_state(|state| {
            let mut source = new_ask("stalled-answer", "answer source");
            source.requester = Requester::Agent { cutex_session_id: session.clone(), name: None };
            let ask_id = state.create_ask(source, now).unwrap().0.ask_id;
            let ask = state.asks.get_mut(&ask_id).unwrap();
            ask.execution = ExecutionState::Answered;
            ask.answer_id = Some("answer".into());
            let ask = ask.clone();
            state.answers.insert("answer".into(), AnswerRecord {
                answer_id: "answer".into(), ask_id, attempt: 1, revision: 1,
                status: AnswerStatus::Completed, artifact_id: None, bytes: 0,
                metadata: AnswerMetadata::default(), created_at: now_rfc3339(now), preview: None,
            });
            super::super::delivery::record_answer_delivery(state, &ask, "answer", now).unwrap();
        }).unwrap();
        assert!(!queue.owner_may_start(&session, Some(&owner.queue_id)).unwrap());
        crate::management::v2::admission::take_change(&session);
        crate::management::v2::admission_wait::elapse_for_test(&session);
        assert!(crate::management::v2::admission::take_change(&session));
        assert!(queue.owner_may_start(&session, Some(&owner.queue_id)).unwrap());
        assert_eq!(queue.admission(&session).unwrap().next_step.len(), 1, "ridealong remains pending");
        crate::management::v2::admission_wait::clear(&session);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ridealongs_coalesce_before_numbered_owner_and_ask_turns() {
        let (world, dir) = world("mixed-admission");
        let (store, store_dir) = temp_store("mixed-admission");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let queue = store.queue_repository().unwrap();
        let owner_a = queued_owner(&store, "owner-a");
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("ask-b", "ask b"), now).unwrap().0.ask_id).unwrap();
        let first_delivery = queued_answer(&store, "answer-c", now);
        let owner_d = queued_owner(&store, "owner-d");
        let second_delivery = queued_answer(&store, "answer-e", now);
        let expected = vec![owner_a.queue_id.clone(), ask.clone(), owner_d.queue_id.clone()];
        let admission = queue.admission(TARGET).unwrap();
        assert_eq!(admission.items.iter().map(|item| item.id.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(admission.next_step.len(), 2);
        assert!(!owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&owner_a.queue_id)));
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert!(report.admitted.is_empty());
        assert_eq!(report.events.len(), 1);
        let delivered = &store.snapshot().unwrap().events[&report.events[0]].delivery_ids;
        assert!(delivered.contains(&first_delivery) && delivered.contains(&second_delivery));
        finish_turn(&runtime, &store, TARGET);
        for id in &expected {
            let admission = queue.admission(TARGET).unwrap();
            assert_eq!(&admission.items[0].id, id);
            if id == &ask {
                assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask.clone()]);
                finish_turn(&runtime, &store, TARGET);
            } else {
                assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(id)));
                let owner = [&owner_a, &owner_d].into_iter().find(|owner| &owner.queue_id == id).unwrap();
                queue.remove(TARGET, id, owner.revision).unwrap();
            }
        }
        assert!(queue.admission(TARGET).unwrap().items.is_empty());
        assert_eq!(runtime.submitted.borrow().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn moves_during_a_turn_change_only_future_admission() {
        for to_tail in [true, false] {
            let (world, dir) = world("admission-active-move");
            let (store, store_dir) = temp_store("admission-active-move");
            let runtime = FakeRuntime::idle(&[TARGET]);
            let now = Utc::now();
            let first = store.with_state(|state| state.create_ask(new_ask("A", "A"), now).unwrap().0.ask_id).unwrap();
            let second = store.with_state(|state| state.create_ask(new_ask("B", "B"), now).unwrap().0.ask_id).unwrap();
            let owner = queued_owner(&store, "c");
            let last = store.with_state(|state| state.create_ask(new_ask("D", "D"), now).unwrap().0.ask_id).unwrap();
            let queue = store.queue_repository().unwrap();
            assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![first.clone()]);
            let lease = store.snapshot().unwrap().leases[TARGET].clone();
            let admission = queue.admission(TARGET).unwrap();
            let before = admission.items.iter().find(|item| item.id == second).unwrap().reference();
            let (moved, _, _, changed) = queue.move_admission_item(TARGET, &owner.queue_id, admission.order_revision,
                if to_tail { None } else { Some(&before) }, true).unwrap();
            assert!(changed);
            assert!(queue.move_admission_item(TARGET, &owner.queue_id, admission.order_revision, None, true).is_err(), "stale CAS");
            assert_eq!(store.snapshot().unwrap().leases[TARGET], lease);
            assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
            assert_eq!(runtime.submitted.borrow().len(), 1);
            let expected = if to_tail {
                vec![second.clone(), last.clone(), owner.queue_id.clone()]
            } else {
                vec![owner.queue_id.clone(), second.clone(), last.clone()]
            };
            assert_eq!(moved.items.iter().map(|item| item.id.clone()).collect::<Vec<_>>(), expected);
            finish_turn(&runtime, &store, TARGET);
            for id in expected {
                let admission = queue.admission(TARGET).unwrap();
                assert_eq!(admission.items[0].id, id);
                if id == owner.queue_id {
                    assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&id)));
                    queue.remove(TARGET, &id, owner.revision).unwrap();
                } else {
                    assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![id]);
                    finish_turn(&runtime, &store, TARGET);
                }
            }
            assert_eq!(runtime.submitted.borrow().iter().map(|(_, submission)| submission.asks[0].clone()).collect::<Vec<_>>(), vec![first, second, last]);
            let _ = std::fs::remove_dir_all(dir);
            let _ = std::fs::remove_dir_all(store_dir);
        }
    }

    #[test]
    fn paused_asks_do_not_block_owner_and_resume_keeps_order() {
        let (world, dir) = world("admission-pause-move");
        let (store, store_dir) = temp_store("admission-pause-move");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let first = store.with_state(|state| state.create_ask(new_ask("first", "first"), now).unwrap().0.ask_id).unwrap();
        let owner = queued_owner(&store, "owner");
        let second = store.with_state(|state| state.create_ask(new_ask("second", "second"), now).unwrap().0.ask_id).unwrap();
        store.with_state(|state| {
            state.pauses.insert(TARGET.into(), AdmissionPause {
                cutex_session_id: TARGET.into(), by: PausedBy::Owner,
                since: now_rfc3339(now), origin: Some("ownerPause".into()), stop_hold: false, runtime_generation: None,
            });
        }).unwrap();
        let queue = store.queue_repository().unwrap();
        let admission = queue.admission(TARGET).unwrap();
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
        assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&owner.queue_id)));
        assert_eq!(admission.items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec![first.as_str(), owner.queue_id.as_str(), second.as_str()]);
        queue.remove(TARGET, &owner.queue_id, owner.revision).unwrap();
        store.with_state(|state| { state.pauses.remove(TARGET); }).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![first]);
        finish_turn(&runtime, &store, TARGET);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![second]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn pending_review_is_excluded_and_does_not_block_later_ask() {
        let (world, dir) = world("pending-review-order");
        let (store, store_dir) = temp_store("pending-review-order");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let (first, second) = store.with_state(|state| {
            let first = state.create_ask(new_ask("review-first", "review"), now).unwrap().0.ask_id;
            let second = state.create_ask(new_ask("second", "second"), now).unwrap().0.ask_id;
            state.asks.get_mut(&first).unwrap().execution = ExecutionState::PendingReview;
            (first, second)
        }).unwrap();
        let queue = store.queue_repository().unwrap();
        let admission = queue.admission(TARGET).unwrap();
        assert_eq!(admission.items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec![second.as_str()]);
        let mut status = runtime.target_status(TARGET);
        status.admission = Some(admission);
        assert_eq!(store.with_state(|state| plan_target(state, TARGET, &status, now)).unwrap(), Some(second.clone()));
        store.with_state(|state| state.asks.get_mut(&first).unwrap().execution = ExecutionState::Queued).unwrap();
        assert_eq!(queue.admission(TARGET).unwrap().items[0].id, first);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![first]);
        finish_turn(&runtime, &store, TARGET);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![second]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn paused_owner_does_not_block_ask_and_auto_skips_held_asks() {
        let (world, dir) = world("paused-owner");
        let (store, store_dir) = temp_store("paused-owner");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let queue = store.queue_repository().unwrap();
        let owner = queued_owner(&store, "paused-owner");
        queue.pause(TARGET, "ownerPause", None).unwrap();
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("ask", "ask"), now).unwrap().0.ask_id).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask]);
        assert_eq!(queue.admission(TARGET).unwrap().items[0].id, owner.queue_id);
        finish_turn(&runtime, &store, TARGET);
        let held = store.with_state(|state| {
            let id = state.create_ask(new_ask("held", "held"), now).unwrap().0.ask_id;
            state.asks.get_mut(&id).unwrap().hold = Some(Hold::WaitingForApproval);
            id
        }).unwrap();
        let admission = queue.admission(TARGET).unwrap();
        assert!(admission.items.iter().any(|item| item.id == held));
        assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, None));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn woken_continuation_precedes_earlier_owner_and_new_ask() {
        let (world, dir) = world("continuation-priority");
        let (store, store_dir) = temp_store("continuation-priority");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let queue = store.queue_repository().unwrap();
        let owner = queued_owner(&store, "owner");
        let now = Utc::now();
        let (ordinary, resumed) = store.with_state(|state| {
            let ordinary = state.create_ask(new_ask("ordinary", "ordinary"), now).unwrap().0.ask_id;
            let resumed = state.create_ask(new_ask("resumed", "resumed"), now).unwrap().0.ask_id;
            let ask = state.asks.get_mut(&resumed).unwrap();
            ask.execution = ExecutionState::Running;
            ask.resume = Some(ResumeRecord {
                reason: ResumeReason::Resumed, since: now_rfc3339(now), from_attempt: 0,
                jobs: Vec::new(), by: Some(ResumeBy::Owner), note: Some("continue".into()),
            });
            (ordinary, resumed)
        }).unwrap();
        let admission = queue.admission(TARGET).unwrap();
        assert!(!owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&owner.queue_id)));
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![resumed]);
        finish_turn(&runtime, &store, TARGET);
        let admission = queue.admission(TARGET).unwrap();
        assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&owner.queue_id)));
        queue.remove(TARGET, &owner.queue_id, owner.revision).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ordinary]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn idle_fail_fast_ask_waits_for_ridealong_without_becoming_busy() {
        let (store, dir) = temp_store("idle-fail-ridealong");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut request = new_ask("fail-fast", "ask");
        request.options.if_busy = IfBusy::Fail;
        let ask = store.with_state(|state| state.create_ask(request, now).unwrap().0.ask_id).unwrap();
        let mut status = runtime.target_status(TARGET);
        let mut admission = store.queue_repository().unwrap().admission(TARGET).unwrap();
        admission.next_step.push(crate::management::v2::admission::AdmissionNextStep {
            id: "pending-bus".into(), delivery_kind: "bus".into(), label: None,
            enqueued_at: now.to_rfc3339(), timing: "afterTurn".into(), next_step_at: None, held: None, ask_id: None,
            from: None, from_host: None, preview: None, eligible: true,
        });
        status.admission = Some(admission);
        assert_eq!(store.with_state(|state| plan_target(state, TARGET, &status, now)).unwrap(), None);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Queued);
        assert_eq!(state.asks[&ask].hold, None);
        status.admission.as_mut().unwrap().next_step.clear();
        assert_eq!(store.with_state(|state| plan_target(state, TARGET, &status, now)).unwrap(), Some(ask));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn busy_refusal_requeues_and_missing_n1_holds_or_fails_fast() {
        let (world, dir) = world("busy");
        let (store, store_dir) = temp_store("busy");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Busy { reason: "active_turn".into() });
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().requeued, vec![ask.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Queued);
        assert_eq!(state.asks[&ask].attempts[0].state, AttemptState::Refused);
        assert!(state.leases.is_empty());
        // The refused attempt is reused (nothing ran), with a new message id.
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts.len(), 1);
        assert_ne!(runtime.submitted.borrow()[0].1.message_id, runtime.submitted.borrow()[1].1.message_id);

        // A runtime without N1: held, never submitted; ifBusy=fail fails.
        finish_turn(&runtime, &store, TARGET);
        runtime.set(TARGET, |status| status.exclusive_admission = false);
        let held = store.with_state(|state| state.create_ask(new_ask("h", "x"), now).unwrap().0.ask_id).unwrap();
        let mut fail_fast = new_ask("f", "y");
        fail_fast.options.if_busy = IfBusy::Fail;
        let failing = store.with_state(|state| state.create_ask(fail_fast, now).unwrap().0.ask_id).unwrap();
        let submitted = runtime.submitted.borrow().len();
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&held].hold, Some(Hold::RuntimeUnsupported { missing: EXCLUSIVE_TURN.into() }));
        assert_eq!(state.asks[&failing].execution, ExecutionState::Failed);
        assert_eq!(runtime.submitted.borrow().len(), submitted);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Acceptance §12.3 item 4 (part): a lost submit reply leaves the
    /// attempt `submitting`; the next tick reconciles by message id and never
    /// resubmits blindly.
    #[test]
    fn uncertain_submit_reconciles_by_message_id() {
        let (world, dir) = world("uncertain");
        let (store, store_dir) = temp_store("uncertain");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("reply lost".into()));
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts[0].state, AttemptState::Submitting);
        assert!(state.leases.contains_key(TARGET));
        // Native actually persisted it in turn-9.
        let message = state.asks[&ask].attempts[0].native_message_id.clone();
        runtime.native.borrow_mut().insert(message, NativeMessageStatus::Persisted { turn_id: "turn-9".into() });
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts[0].state, AttemptState::Admitted);
        assert_eq!(state.asks[&ask].attempts[0].turn_id.as_deref(), Some("turn-9"));
        assert_eq!(runtime.submitted.borrow().len(), 1, "never resubmitted");
        assert_eq!(state.asks[&ask].submit_retry, None, "a receipt clears the failures");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The receiver refuses a message by its canonical byte policy although
    /// the cutex estimate fit (another limit): the same text would be refused
    /// forever, so the next admission renders one level deeper (new message
    /// id, same attempt, no new approval); when the deepest level is refused
    /// too, the ask fails at once with `event_too_large` (sizes in
    /// `lastError`), the requester gets one state item, the owner one host
    /// notice, and nothing is ever resubmitted.
    #[test]
    fn too_large_submit_renders_deeper_then_fails_and_is_never_retried() {
        let (world, dir) = world("too-large");
        let (store, store_dir) = temp_store("too-large");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", &"long brief line\n".repeat(400)), now).unwrap().0.ask_id).unwrap();
        for _ in 0..super::super::RENDER_LEVELS {
            runtime.outcomes.borrow_mut().push(SubmitOutcome::TooLarge { item_bytes: Some(11_501), limit: Some(10_000) });
        }
        for level in 0..super::super::RENDER_LEVELS - 1 {
            let report = tick(&world, &runtime, &store, now).unwrap();
            assert_eq!(report.requeued, vec![ask.clone()], "level {level}");
            let state = store.snapshot().unwrap();
            let record = &state.asks[&ask];
            assert_eq!(record.execution, ExecutionState::Queued);
            assert_eq!(record.attempts.len(), 1, "the same attempt");
            let render = record.attempts[0].render.clone().unwrap();
            assert_eq!((render.level, render.rejected), (level, true));
            assert!(state.leases.is_empty());
        }
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.failed, vec![ask.clone()]);
        let submitted = runtime.submitted.borrow();
        assert_eq!(submitted.len(), usize::from(super::super::RENDER_LEVELS));
        let ids: std::collections::BTreeSet<&str> = submitted.iter().map(|(_, s)| s.message_id.as_str()).collect();
        assert_eq!(ids.len(), submitted.len(), "a new message id per level");
        assert!(submitted[0].1.text.len() > submitted[3].1.text.len());
        assert!(submitted[3].1.text.contains("[overlength delegation brief]") && submitted[3].1.text.contains("read full text: call the MCP tool delegation_read (cutex server) with "), "{}", submitted[3].1.text);
        drop(submitted);
        let state = store.snapshot().unwrap();
        let record = &state.asks[&ask];
        assert_eq!(record.execution, ExecutionState::Failed);
        assert_eq!(record.attempts[0].state, AttemptState::Failed);
        assert!(state.leases.is_empty(), "the target is free for other work");
        let error = record.attempts[0].last_error.as_deref().unwrap();
        assert!(error.starts_with("event_too_large: the ASK message ("), "{error}");
        assert!(error.contains("about 11501 bytes as a model item") && error.contains("Kimi accepts (receiver canonical byte policy: 10000 bytes)"), "{error}");
        let items = state.deliveries.values().filter(|d| d.ask_id == ask && d.ref_id.starts_with("event_too_large@")).count();
        assert_eq!(items, 1, "the requester learns it failed");
        let notices: Vec<&crate::asks::store::Notice> =
            state.notices.iter().filter(|notice| notice.method == DELIVERY_FAILED_EVENT).collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].sessions, vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()]);
        assert_eq!(notices[0].params["code"], "event_too_large");
        assert_eq!(notices[0].params["askIds"], json!([ask.clone()]));
        assert_eq!(notices[0].params["requester"], REQUESTER);
        for later in [now + chrono::Duration::seconds(2), now + chrono::Duration::hours(1)] {
            tick(&world, &runtime, &store, later).unwrap();
        }
        assert_eq!(runtime.submitted.borrow().len(), usize::from(super::super::RENDER_LEVELS), "never resubmitted");
        let state = store.snapshot().unwrap();
        assert_eq!(state.notices.iter().filter(|notice| notice.method == DELIVERY_FAILED_EVENT).count(), 1);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The 2026-10 Linux-host case: a follow-up quoting an 8 KiB previous answer
    /// is over a 10,000-byte receiver at level 0; cutex renders the first
    /// level that fits (preview + delegation_read pointer) by itself, before
    /// any submit. A text that fits nowhere fails without a submit.
    #[test]
    fn follow_up_with_a_long_previous_answer_fits_a_default_receiver() {
        use crate::app_server::external_input::size::{canonical_item_bytes, ReceiverLimit};
        let mut state = AskState::default();
        let now = Utc::now();
        let mut new = new_ask("f", &"Please check \"this\" again.\n".repeat(60));
        new.follow_up_of = None;
        let (mut ask, _) = state.create_ask(new, now).unwrap();
        ask.follow_up_of = Some("ask-previous".into());
        let previous = "Result line with \"quotes\" and \\paths\\\n".repeat(220);
        let label = RequesterLabel { name: "Eva".into(), profile_model: None, narrower: false };
        let budget = ReceiverLimit::Default.budget(10);
        let full = render::ask_text(&ask, 1, &label, Some(("ans-1", &previous)), false);
        let source = submission_source(Some(REQUESTER));
        assert!(canonical_item_bytes("m", &source, render::ASK_EVENT, &full) > 10_000, "level 0 is over the limit");
        let shape = |level| render::Shape { level, preview: 800, brief_artifact: Some("art-brief"), limit: Some(9_000), note_artifact: None };
        let (text, info) = fit_level(&budget, "cutex-ask-msg-1", Some(REQUESTER), render::ASK_EVENT, 0, &mut |level| {
            Ok(render::ask_text_at(&ask, 1, &label, Some(("ans-1", &previous)), false, shape(level)))
        })
        .unwrap()
        .unwrap();
        assert!(info.level >= 1 && info.item_bytes <= 9_000, "{info:?}");
        assert!(text.contains("delegation_read (cutex server) with delegation=\"delegation-previous\""), "{text}");
        assert_eq!(info.item_bytes, canonical_item_bytes("cutex-ask-msg-1", &source, render::ASK_EVENT, &text));
        let tiny = ReceiverLimit::Item(300).budget(0);
        let unfit = fit_level(&tiny, "m", Some(REQUESTER), render::ASK_EVENT, 0, &mut |level| {
            Ok(render::ask_text_at(&ask, 1, &label, Some(("ans-1", &previous)), false, shape(level)))
        })
        .unwrap()
        .unwrap_err();
        assert_eq!(unfit.level, super::super::RENDER_LEVELS - 1);
        assert!(unfit_detail("ASK", &unfit, "Kimi").contains("even with every long part replaced"));
    }

    /// A refused attempt (busy) keeps the approval it consumed when the
    /// snapshot did not change: its retry does not ask the owner again.
    #[test]
    fn refused_attempt_retry_reuses_its_consumed_approval() {
        let (world, dir) = world("refused-approval");
        let (store, store_dir) = temp_store("refused-approval");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        // The target can do more than the requester: delivery is gated.
        *world.inputs.borrow_mut() = inputs(
            Some(party(REQUESTER, SandboxLevel::WorkspaceWrite, &["/w"], "chatgpt")),
            party(TARGET, SandboxLevel::FullAccess, &[], "deepseek"),
        );
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        let inputs = world.inputs.borrow().clone();
        store
            .with_state(|state| {
                let ask = state.asks.get_mut(&ask).unwrap();
                let mut gate = super::super::gate::evaluate(&super::super::gate::ask_facts(ask), &inputs);
                assert!(gate.required);
                gate.admitted_with_approval = true;
                ask.gate = Some(gate);
                let mut record = AttemptRecord::submitting(1, "m-1".into(), 1, now_rfc3339(now));
                record.state = AttemptState::Refused;
                ask.attempts.push(record);
            })
            .unwrap();
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.admitted, vec![ask.clone()], "{report:?}");
        assert!(report.reviewed.is_empty());
        let state = store.snapshot().unwrap();
        assert!(state.asks[&ask].gate.as_ref().unwrap().admitted_with_approval);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn submit_retry_delay_doubles_up_to_the_cap() {
        assert_eq!(submit_retry_delay(1), chrono::Duration::seconds(5));
        assert_eq!(submit_retry_delay(2), chrono::Duration::seconds(10));
        assert_eq!(submit_retry_delay(4), chrono::Duration::seconds(40));
        assert_eq!(submit_retry_delay(8), chrono::Duration::seconds(600));
        assert_eq!(submit_retry_delay(u32::MAX), chrono::Duration::seconds(600));
    }

    #[test]
    fn native_busy_retry_keeps_idle_ticks_read_only_then_eventually_submits() {
        // gate_closed pauses instead (runtime_gate_pause_*).
        for reason in ["server_draining"] {
            for continuation in [false, true] {
                let label = format!("{reason}-{continuation}");
                let (world, dir) = world(&label);
                let (store, store_dir) = temp_store(&label);
                let runtime = FakeRuntime::idle(&[TARGET]);
                let now = crate::asks::parse_time(&now_rfc3339(Utc::now())).unwrap();
                let ask = store.with_state(|state| {
                    let id = state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id;
                    if continuation {
                        let ask = state.asks.get_mut(&id).unwrap();
                        ask.execution = ExecutionState::Running;
                        ask.resume = Some(ResumeRecord {
                            reason: ResumeReason::Resumed, since: now_rfc3339(now), from_attempt: 0,
                            jobs: vec![], by: Some(ResumeBy::Owner), note: None,
                        });
                    }
                    id
                }).unwrap();
                for _ in 0..2 {
                    runtime.outcomes.borrow_mut().push(SubmitOutcome::Busy { reason: reason.into() });
                }
                let mut due = now;
                for attempt in 1..=2 {
                    tick(&world, &runtime, &store, due).unwrap();
                    assert_eq!(runtime.submitted.borrow().len(), attempt);
                    let path = store_dir.join("asks-v1.json");
                    let bytes = std::fs::read(&path).unwrap();
                    let stamp = crate::platform::file_stamp::FileStamp::of(&path);
                    let before = store.snapshot().unwrap();
                    assert_eq!(before.asks[&ask].hold, Some(busy_hold(reason)));
                    assert_eq!(before.asks[&ask].submit_retry.as_ref().unwrap().failures, 0);
                    assert!(before.leases.is_empty(), "a definite refusal owns no lease");
                    assert_eq!(before.asks[&ask].resume.is_some(), continuation);
                    for second in 1..super::super::SUBMIT_RETRY_BASE_SECS {
                        tick(&world, &runtime, &store, due + chrono::Duration::seconds(second)).unwrap();
                        assert_eq!(std::fs::read(&path).unwrap(), bytes, "unchanged busy ticks must not rewrite history");
                        assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
                        assert_eq!(runtime.submitted.borrow().len(), attempt);
                    }
                    due += chrono::Duration::seconds(super::super::SUBMIT_RETRY_BASE_SECS);
                }
                let report = tick(&world, &runtime, &store, due).unwrap();
                assert_eq!(report.admitted, vec![ask.clone()]);
                let after = store.snapshot().unwrap();
                assert_eq!(runtime.submitted.borrow().len(), 3);
                assert_eq!(after.asks[&ask].attempts.len(), 1, "retry reuses the never-accepted attempt");
                assert!(after.asks[&ask].submit_retry.is_none());
                assert!(after.asks[&ask].resume.is_none());
                assert_eq!(after.asks[&ask].execution, if continuation { ExecutionState::Running } else { ExecutionState::Admitted });
                if dir.exists() { std::fs::remove_dir_all(dir).unwrap(); }
                std::fs::remove_dir_all(store_dir).unwrap();
            }
        }
    }

    /// 2026-10-13 wedge (a Claude worker agent): a closed Claude
    /// gate refused an ask every five seconds while the owner's own message
    /// waited behind it. A `gate_closed` refusal now records a
    /// `runtimeGate` pause: the ask waits `heldByRuntime(gate_closed)` and
    /// is not retried, the owner's queued message may start, and the owner's
    /// resume (or message) reopens the gate; a runtime restart ends it.
    #[test]
    fn runtime_gate_pause_holds_asks_and_lets_the_owner_go_first() {
        let (world, dir) = world("gate-pause");
        let (store, store_dir) = temp_store("gate-pause");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = crate::asks::parse_time(&now_rfc3339(Utc::now())).unwrap();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Busy { reason: "gate_closed".into() });
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        let pause = &state.pauses[TARGET];
        assert_eq!(pause.origin.as_deref(), Some(super::super::control::ORIGIN_RUNTIME_GATE));
        assert_eq!(pause.runtime_generation, Some(1));
        assert!(!pause.stop_hold);
        assert_eq!(state.asks[&ask].hold, Some(Hold::HeldByRuntime { native: Some("gate_closed".into()) }));
        let notice = state.notices.iter().rev().find(|notice| notice.method == super::super::control::PAUSE_CHANGED_EVENT).unwrap();
        assert_eq!(notice.params["reason"], json!("runtimeGate"));
        assert_eq!(notice.params["asksPaused"]["origin"], json!("runtimeGate"));
        crate::management::v2::contract_validation::validate_cutex_event_message(
            &json!({"method": notice.method, "params": notice.params}),
        ).expect("pauseChanged matches the schema");
        // No probing every five seconds, and the hold stays as it is.
        for second in [5, 30, 600] {
            tick(&world, &runtime, &store, now + chrono::Duration::seconds(second)).unwrap();
        }
        assert_eq!(runtime.submitted.borrow().len(), 1);
        assert_eq!(store.snapshot().unwrap().asks[&ask].hold, Some(Hold::HeldByRuntime { native: Some("gate_closed".into()) }));
        // The owner's queued message is the admissible head now.
        let owner = queued_owner(&store, "owner");
        let admission = store.queue_repository().unwrap().admission(TARGET).unwrap();
        assert!(owner_may_flush(&store.snapshot().unwrap(), TARGET, &admission, Some(&owner.queue_id)));
        // The owner's message acknowledges it: the gate reopens, the ask goes.
        store.with_state(|state| super::super::control::owner_message(state, TARGET, "queued", now)).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.pauses.is_empty());
        assert_eq!(state.asks[&ask].hold, None);
        tick(&world, &runtime, &store, now + chrono::Duration::seconds(601)).unwrap();
        assert_eq!(*runtime.resumes.borrow(), vec![TARGET.to_string()]);
        if dir.exists() { std::fs::remove_dir_all(dir).unwrap(); }
        std::fs::remove_dir_all(store_dir).unwrap();
    }

    #[test]
    fn runtime_gate_pause_ends_with_a_new_runtime_generation() {
        let (world, dir) = world("gate-restart");
        let (store, store_dir) = temp_store("gate-restart");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = crate::asks::parse_time(&now_rfc3339(Utc::now())).unwrap();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Busy { reason: "gate_closed".into() });
        tick(&world, &runtime, &store, now).unwrap();
        assert!(store.snapshot().unwrap().pauses.contains_key(TARGET));
        // Offline: the pause stays (nothing known about the next runtime yet).
        runtime.set(TARGET, |status| status.online = false);
        tick(&world, &runtime, &store, now + chrono::Duration::seconds(10)).unwrap();
        assert!(store.snapshot().unwrap().pauses.contains_key(TARGET));
        // A restarted runtime (generation 2): the pause ends and the ask is admitted.
        runtime.set(TARGET, |status| { status.online = true; status.runtime_generation = 2; });
        let report = tick(&world, &runtime, &store, now + chrono::Duration::seconds(20)).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.pauses.is_empty());
        assert_eq!(report.admitted, vec![ask]);
        assert!(runtime.resumes.borrow().is_empty(), "a new runtime needs no gate resume");
        let reasons: Vec<&Value> = state.notices.iter()
            .filter(|notice| notice.method == super::super::control::PAUSE_CHANGED_EVENT)
            .map(|notice| &notice.params["reason"]).collect();
        assert_eq!(reasons, [&json!("runtimeGate"), &json!("runtimeRestart")]);
        if dir.exists() { std::fs::remove_dir_all(dir).unwrap(); }
        std::fs::remove_dir_all(store_dir).unwrap();
    }

    #[test]
    fn native_busy_retry_preserves_owner_pause_and_unknown_acceptance() {
        let (world, dir) = world("busy-unknown");
        let (store, store_dir) = temp_store("busy-unknown");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = crate::asks::parse_time(&now_rfc3339(Utc::now())).unwrap();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Busy { reason: "gate_closed".into() });
        tick(&world, &runtime, &store, now).unwrap();
        // An owner's real pause still overrides the advisory native hold.
        store.with_state(|state| {
            state.pauses.insert(TARGET.into(), AdmissionPause {
                cutex_session_id: TARGET.into(), by: PausedBy::Owner,
                since: now_rfc3339(now), origin: Some("ownerStop".into()), stop_hold: true, runtime_generation: None,
            });
        }).unwrap();
        let due = now + chrono::Duration::seconds(super::super::SUBMIT_RETRY_BASE_SECS);
        tick(&world, &runtime, &store, due).unwrap();
        assert_eq!(runtime.submitted.borrow().len(), 1);
        assert_eq!(store.snapshot().unwrap().asks[&ask].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        store.with_state(|state| { state.pauses.remove(TARGET); }).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("reply lost".into()));
        tick(&world, &runtime, &store, due).unwrap();
        let pending = store.snapshot().unwrap();
        let message = pending.asks[&ask].attempts[0].native_message_id.clone();
        assert_eq!(pending.asks[&ask].submit_retry.as_ref().unwrap().failures, 1);
        runtime.native.borrow_mut().insert(message, NativeMessageStatus::Pending);
        for second in [1, 6, 30] {
            tick(&world, &runtime, &store, due + chrono::Duration::seconds(second)).unwrap();
            assert_eq!(runtime.submitted.borrow().len(), 2, "unknown acceptance must reconcile, never blindly resubmit");
            assert!(store.snapshot().unwrap().leases.contains_key(TARGET));
        }
        if dir.exists() { std::fs::remove_dir_all(dir).unwrap(); }
        std::fs::remove_dir_all(store_dir).unwrap();
    }

    /// Retryable submit errors (the reply says nothing ran) are retried with
    /// exponential backoff, never sooner, and the ask fails `submit_failed`
    /// after `SUBMIT_RETRY_LIMIT` failures in a row (owner told once).
    #[test]
    fn failed_submits_back_off_and_fail_after_the_limit() {
        let (world, dir) = world("submit-retry");
        let (store, store_dir) = temp_store("submit-retry");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let mut now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        for _ in 0..crate::asks::SUBMIT_RETRY_LIMIT {
            runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("app-server request failed: connection reset".into()));
        }
        for n in 1..=crate::asks::SUBMIT_RETRY_LIMIT {
            tick(&world, &runtime, &store, now).unwrap();
            assert_eq!(runtime.submitted.borrow().len(), n as usize);
            let state = store.snapshot().unwrap();
            assert_eq!(state.asks[&ask].attempts[0].state, AttemptState::Submitting);
            assert!(state.asks[&ask].attempts[0].last_error.as_deref().unwrap().contains(&format!("submit failure {n}/")));
            // The status query is due at once: absent, so nothing ran.
            let report = tick(&world, &runtime, &store, now).unwrap();
            if n < crate::asks::SUBMIT_RETRY_LIMIT {
                assert_eq!(report.requeued, vec![ask.clone()]);
                assert_eq!(store.snapshot().unwrap().asks[&ask].execution, ExecutionState::Queued);
                let delay = submit_retry_delay(n);
                tick(&world, &runtime, &store, now + delay - chrono::Duration::seconds(1)).unwrap();
                assert_eq!(runtime.submitted.borrow().len(), n as usize, "not before the backoff ends");
                now += delay;
            } else {
                assert_eq!(report.failed, vec![ask.clone()]);
            }
        }
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Failed);
        assert!(state.leases.is_empty());
        assert!(state.asks[&ask].attempts[0].last_error.as_deref().unwrap().starts_with("submit_failed: "));
        let notices: Vec<&crate::asks::store::Notice> =
            state.notices.iter().filter(|notice| notice.method == DELIVERY_FAILED_EVENT).collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].params["code"], "submit_failed");
        tick(&world, &runtime, &store, now + chrono::Duration::hours(1)).unwrap();
        assert_eq!(runtime.submitted.borrow().len(), crate::asks::SUBMIT_RETRY_LIMIT as usize);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The runtime restarted while an attempt was `submitting`: the old
    /// occurrence can't be queried, so the attempt ends
    /// interrupted(runtimeRestart) at once and the lease no longer blocks the
    /// target (before: queried every tick, forever).
    #[test]
    fn submitting_attempt_of_a_restarted_runtime_ends_interrupted() {
        let (world, dir) = world("submit-restart");
        let (store, store_dir) = temp_store("submit-restart");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("reply lost".into()));
        tick(&world, &runtime, &store, now).unwrap();
        assert!(store.snapshot().unwrap().leases.contains_key(TARGET));
        runtime.set(TARGET, |status| status.runtime_generation = 2);
        let queries = *runtime.status_queries.borrow();
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.interrupted, vec![ask.clone()]);
        assert_eq!(*runtime.status_queries.borrow(), queries, "the dead occurrence is not queried");
        let state = store.snapshot().unwrap();
        assert!(state.leases.is_empty());
        assert_eq!(state.asks[&ask].execution, ExecutionState::Interrupted);
        assert_eq!(state.asks[&ask].attempts[0].interrupt_reason, Some(InterruptReason::RuntimeRestart));
        assert!(state.asks[&ask].is_open(), "the requester or owner can retry it");
        assert_eq!(runtime.submitted.borrow().len(), 1, "never re-run blindly");
        assert!(state.asks[&ask].submit_retry.is_some());
        let retried = store.with_state(|state| crate::asks::lifecycle::retry(state, &ask, None, now)).unwrap().unwrap();
        assert_eq!(retried.submit_retry, None, "a retry starts with a fresh budget");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Failed status queries of a `submitting` attempt back off too; past the
    /// limit the outcome stays unknown and the attempt ends
    /// interrupted(outcomeUnknown), freeing the target.
    #[test]
    fn failed_status_queries_back_off_then_end_outcome_unknown() {
        let (world, dir) = world("status-retry");
        let (store, store_dir) = temp_store("status-retry");
        let runtime = FakeRuntime::idle(&[TARGET]);
        // Millisecond precision, like the stored `notBefore`.
        let now = crate::asks::parse_time(&now_rfc3339(Utc::now())).unwrap();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("reply lost".into()));
        tick(&world, &runtime, &store, now).unwrap();
        let message = store.snapshot().unwrap().asks[&ask].attempts[0].native_message_id.clone();
        runtime.native.borrow_mut().insert(message, NativeMessageStatus::Unavailable("socket closed".into()));
        let mut at = now;
        let mut queries = 0;
        loop {
            let report = tick(&world, &runtime, &store, at).unwrap();
            queries += 1;
            assert_eq!(*runtime.status_queries.borrow(), queries);
            if !report.interrupted.is_empty() {
                assert_eq!(report.interrupted, vec![ask.clone()]);
                break;
            }
            let retry = store.snapshot().unwrap().asks[&ask].submit_retry.clone().unwrap();
            let next = crate::asks::parse_time(&retry.not_before).unwrap();
            assert_eq!(next - at, submit_retry_delay(retry.failures));
            tick(&world, &runtime, &store, next - chrono::Duration::seconds(1)).unwrap();
            assert_eq!(*runtime.status_queries.borrow(), queries, "no query before the backoff ends");
            at = next;
        }
        assert_eq!(queries, (crate::asks::SUBMIT_RETRY_LIMIT - 1) as usize);
        let state = store.snapshot().unwrap();
        assert!(state.leases.is_empty());
        assert_eq!(state.asks[&ask].attempts[0].interrupt_reason, Some(InterruptReason::OutcomeUnknown));
        assert_eq!(runtime.submitted.borrow().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Acceptance §12.3 items 2 and 5 (parts): a gated ask waits for review,
    /// is admitted once after approval, and goes back to review when the
    /// target changes; an owner pause holds asks; a revoked boundary fails a
    /// queued ask.
    #[test]
    fn review_pause_and_boundary_at_admission() {
        let (world, dir) = world("review");
        world.inputs.borrow_mut().requester = Some(party(REQUESTER, SandboxLevel::ReadOnly, &[], "chatgpt"));
        let (store, store_dir) = temp_store("review");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("g", "x"), now).unwrap().0.ask_id).unwrap();
        // Not created as pendingReview (tools do that); the admission gate
        // catches it anyway and sends it to review.
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::PendingReview);
        let approval = state.asks[&ask].gate.as_ref().unwrap().approval_id.clone().unwrap();
        assert!(runtime.submitted.borrow().is_empty());
        assert!(state.notices.iter().any(|n| n.method == "cutex/approvals/changed"));
        world.approvals.decide(&approval, true, None, now).unwrap();
        // Owner pause holds even an approved ask.
        store.with_state(|state| {
            state.pauses.insert(TARGET.into(), AdmissionPause { cutex_session_id: TARGET.into(), by: PausedBy::Owner, since: "t".into(), origin: None, stop_hold: false, runtime_generation: None })
        }).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].execution, ExecutionState::Queued);
        assert_eq!(state.asks[&ask].hold, Some(Hold::Paused { by: PausedBy::Owner }));
        assert!(state.deliveries.values().any(|d| d.kind == DeliveryKind::State));
        store.with_state(|state| state.pauses.clear()).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask.clone()]);
        assert!(store.snapshot().unwrap().asks[&ask].gate.as_ref().unwrap().admitted_with_approval);

        // A queued ask whose requester left the boundary fails at admission.
        finish_turn(&runtime, &store, TARGET);
        let mut outside = world.directory.clone();
        outside.boundary = crate::agent_bus::boundary::AgentBoundary::new(
            &crate::config::agent_security::AgentSecurityConfig::default(),
            [(REQUESTER.to_string(), "alpha".to_string()), (TARGET.to_string(), "beta".to_string())].into(),
            Default::default(),
        );
        let world2 = FakeWorld { directory: outside, inputs: RefCell::new(world.inputs.borrow().clone()), approvals: ApprovalStore::at(dir.join("a2.json")), facts: Vec::new(), artifacts: None };
        world2.inputs.borrow_mut().requester = Some(party(REQUESTER, SandboxLevel::FullAccess, &[], "chatgpt"));
        let revoked = store.with_state(|state| state.create_ask(new_ask("r", "x"), now).unwrap().0.ask_id).unwrap();
        assert_eq!(tick(&world2, &runtime, &store, now).unwrap().failed, vec![revoked.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&revoked].attempts.len(), 0);
        assert_eq!(state.asks[&revoked].execution, ExecutionState::Failed);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn offline_target_is_blocked_and_owner_asks_are_never_gated() {
        let (world, dir) = world("offline");
        world.inputs.borrow_mut().requester = Some(party(REQUESTER, SandboxLevel::ReadOnly, &[], "chatgpt"));
        let (store, store_dir) = temp_store("offline");
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        let mut owner = new_ask("o", "from the owner");
        owner.requester = Requester::Owner;
        let ask = store.with_state(|state| state.create_ask(owner, now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(
            store.snapshot().unwrap().asks[&ask].hold,
            Some(Hold::Blocked { by: BlockedReason::RuntimeDown })
        );
        runtime.set(TARGET, |status| {
            status.online = true;
            status.exclusive_admission = true;
        });
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask]);
        assert_eq!(runtime.submitted.borrow()[0].1.source_agent, None);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Acceptance §12.3 item 4 (crash windows, fake runtime): after a
    /// runtime restart a completed turn is captured from history, a missing
    /// one ends interrupted(runtimeRestart); a held item ends
    /// interrupted(outcomeUnknown); an item the bound turn dropped before
    /// context is admitted again. Nothing is resubmitted blindly.
    #[test]
    fn restart_and_held_reconciliation() {
        let (world, dir) = world("recover");
        let (store, store_dir) = temp_store("recover");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let later = now + chrono::Duration::seconds(BOUND_SETTLE_SECS + 1);
        let ask_id = |action: &str| store.with_state(|state| state.create_ask(new_ask(action, "x"), now).unwrap().0.ask_id).unwrap();

        // 1. Completed while cutex was down; the runtime restarted.
        let first = ask_id("a");
        tick(&world, &runtime, &store, now).unwrap();
        runtime.turns.borrow_mut().insert(
            "turn-1".into(),
            json!({"id": "turn-1", "status": "completed", "items": [{"type": "agentMessage", "phase": "final_answer", "text": "done"}]}),
        );
        runtime.set(TARGET, |status| {
            status.active_turn_id = None;
            status.runtime_generation = 2;
        });
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.interrupted, vec![first.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&first].execution, ExecutionState::Answered);
        assert!(state.leases.is_empty());

        // 2. Restart, turn not in history: interrupted(runtimeRestart), open.
        let second = ask_id("b");
        tick(&world, &runtime, &store, now).unwrap();
        runtime.set(TARGET, |status| {
            status.active_turn_id = None;
            status.runtime_generation = 3;
        });
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&second].execution, ExecutionState::Interrupted);
        assert_eq!(state.asks[&second].attempts[0].interrupt_reason, Some(InterruptReason::RuntimeRestart));
        assert!(state.asks[&second].is_open());

        // 3. The bound turn ended before the item reached context (status
        // unknown): admitted again, same attempt, after the settle time.
        let submitted = runtime.submitted.borrow().len();
        let third = ask_id("c");
        tick(&world, &runtime, &store, now).unwrap();
        let message = store.snapshot().unwrap().asks[&third].attempts[0].native_message_id.clone();
        runtime.native.borrow_mut().remove(&message);
        runtime.set(TARGET, |status| status.active_turn_id = None);
        assert!(tick(&world, &runtime, &store, now).unwrap().requeued.is_empty(), "settling");
        assert_eq!(tick(&world, &runtime, &store, later).unwrap().requeued, vec![third.clone()]);
        // Safe to resubmit: admitted again at once, same attempt number.
        assert_eq!(runtime.submitted.borrow().len(), submitted + 2);
        assert_eq!(store.snapshot().unwrap().asks[&third].attempts.len(), 1);

        // 4. Submitting with a held native status: interrupted(outcomeUnknown).
        let (store4, dir4) = temp_store("recover4");
        let fourth = store4.with_state(|state| state.create_ask(new_ask("d", "x"), now).unwrap().0.ask_id).unwrap();
        runtime.set(TARGET, |status| status.active_turn_id = None);
        runtime.outcomes.borrow_mut().push(SubmitOutcome::Uncertain("lost".into()));
        tick(&world, &runtime, &store4, now).unwrap();
        let message = store4.snapshot().unwrap().asks[&fourth].attempts[0].native_message_id.clone();
        runtime
            .native
            .borrow_mut()
            .insert(message, NativeMessageStatus::Held { reason: "request_uncertain".into(), turn_id: Some("turn-x".into()) });
        let report = tick(&world, &runtime, &store4, now).unwrap();
        assert_eq!(report.interrupted, vec![fourth.clone()]);
        let state = store4.snapshot().unwrap();
        assert_eq!(state.asks[&fourth].attempts[0].interrupt_reason, Some(InterruptReason::OutcomeUnknown));
        assert!(state.leases.is_empty());
        for path in [dir, store_dir, dir4] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn busy_reasons_map_to_holds() {
        assert_eq!(busy_hold("plan_mode"), Hold::Paused { by: PausedBy::PlanMode });
        assert!(matches!(busy_hold("gate_closed"), Hold::HeldByRuntime { .. }));
        assert!(matches!(busy_hold("pending_obligation"), Hold::TargetBusy { .. }));
    }

    /// Acceptance §12.3 item 10 (part): an ask to an offline same-project
    /// peer wakes it once (retried after the retry window); an owner-stopped
    /// target is not woken.
    #[test]
    fn offline_targets_are_woken_by_policy() {
        let (mut world, dir) = world("wake");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), ..Default::default() },
            crate::asks::children::AgentFacts { cutex_session_id: TARGET.into(), project: Some("alpha".into()), ..Default::default() },
        ];
        let (store, store_dir) = temp_store("wake");
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap()).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.wakes.borrow().as_slice(), [TARGET.to_string()]);
        tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.wakes.borrow().len(), 1, "not retried at once");
        store.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, TARGET, true)).unwrap();
        tick(&world, &runtime, &store, now + chrono::Duration::minutes(5)).unwrap();
        assert_eq!(runtime.wakes.borrow().len(), 1, "owner-stopped targets are never woken");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// Owner approval 2026-10-14: the Director's delegation starts its
    /// project's offline explicit-launch worker again on the owner's reviewed
    /// launch (reviewed relaunch, never a plain wake), records it for the
    /// owner, and an owner-stopped worker stays blocked.
    #[test]
    fn director_relaunches_a_reviewed_explicit_worker() {
        let (mut world, dir) = world("reviewed-relaunch");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), director: true, ..Default::default() },
            crate::asks::children::AgentFacts {
                cutex_session_id: TARGET.into(),
                project: Some("alpha".into()),
                explicit_launch: true,
                reviewed_relaunch: true,
                ..Default::default()
            },
        ];
        let (store, store_dir) = temp_store("reviewed-relaunch");
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.relaunches.borrow().as_slice(), [(TARGET.to_string(), REQUESTER.to_string())]);
        assert!(runtime.wakes.borrow().is_empty(), "never a plain wake");
        assert_eq!(runtime.relaunch_asks.borrow().as_slice(), [ask.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].hold, Some(Hold::Blocked { by: BlockedReason::RuntimeDown }));
        // The owner's notice comes from the runtime once the launch ran.
        assert!(!state.notices.iter().any(|n| n.method == crate::asks::wake::REVIEWED_RELAUNCH_EVENT));
        assert!(!state.notices.iter().any(|n| n.method == crate::asks::wake::OWNER_ACTION_EVENT));
        tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(runtime.relaunches.borrow().len(), 1, "not retried at once");
        // The owner took it offline: blocked for good.
        store.with_state(|state| crate::asks::wake::note_owner_runtime_action(state, TARGET, true)).unwrap();
        tick(&world, &runtime, &store, now + chrono::Duration::minutes(5)).unwrap();
        assert_eq!(runtime.relaunches.borrow().len(), 1, "owner-stopped workers are never relaunched");
        assert!(store.snapshot().unwrap().notices.iter().any(|n| n.method == crate::asks::wake::OWNER_ACTION_EVENT));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// 2026-10-14 e2e: a Director that re-delegated on every
    /// `blockedOwnerAction` turn produced 34 asks in 20 s. While the
    /// (requester, target) pair stays blocked for the same reason only the
    /// first ask gives an actionable item (a turn); later ones are passive.
    /// A new reason or an unblock may notify again.
    #[test]
    fn blocked_owner_action_turns_are_coalesced_per_pair_and_reason() {
        let (mut world, dir) = world("owner-action-coalesce");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), director: true, ..Default::default() },
            crate::asks::children::AgentFacts { cutex_session_id: TARGET.into(), project: Some("alpha".into()), explicit_launch: true, ..Default::default() },
        ];
        let (store, store_dir) = temp_store("owner-action-coalesce");
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        let actionable = |store: &AskStore| {
            store
                .snapshot()
                .unwrap()
                .deliveries
                .values()
                .filter(|d| d.ref_id == crate::asks::delivery::BLOCKED_OWNER_ACTION && !d.passive)
                .count()
        };
        let all = |store: &AskStore| {
            store.snapshot().unwrap().deliveries.values().filter(|d| d.ref_id == crate::asks::delivery::BLOCKED_OWNER_ACTION).count()
        };
        // The loop: each blocked notice made the Director delegate again.
        for n in 0..34 {
            store.with_state(|state| state.create_ask(new_ask(&format!("loop-{n}"), "x"), now).unwrap()).unwrap();
            tick(&world, &runtime, &store, now + chrono::Duration::milliseconds(600 * n)).unwrap();
        }
        assert!(runtime.relaunches.borrow().is_empty() && runtime.wakes.borrow().is_empty());
        assert_eq!(all(&store), 34, "every delegation still shows its state");
        assert_eq!(actionable(&store), 1, "one turn for the pair and reason");
        // A new reason notifies again (the owner took it offline).
        store.with_state(|state| crate::asks::wake::record_owner_action(state, TARGET, "ownerStopped", None, now)).unwrap();
        store.with_state(|state| state.create_ask(new_ask("stopped", "x"), now).unwrap()).unwrap();
        store.with_state(|state| crate::asks::wake::record_owner_action(state, TARGET, "ownerStopped", None, now)).unwrap();
        assert_eq!(actionable(&store), 2, "new reason: one more turn");
        // Unblocked: nothing is queued for it any more (all ran or were
        // stopped); the next tick forgets who was told. Blocked again:
        // notify again.
        let open: Vec<String> = store.snapshot().unwrap().waiting_for_target(TARGET).iter().map(|ask| ask.ask_id.clone()).collect();
        store
            .with_state(|state| {
                for id in &open {
                    state.update_ask(id, now, |ask| ask.execution = ExecutionState::Cancelled);
                }
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        assert!(store.snapshot().unwrap().owner_action_told.is_empty(), "unblocked: forgotten");
        store.with_state(|state| state.create_ask(new_ask("again", "x"), now).unwrap()).unwrap();
        store.with_state(|state| crate::asks::wake::record_owner_action(state, TARGET, "ownerStopped", None, now)).unwrap();
        assert_eq!(actionable(&store), 3, "after an unblock: one more turn");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// 2026-10-14 e2e: an ask whose requester may not start the offline
    /// explicit-launch worker (here the owner's) at the head of its queue
    /// does not keep the Director's later delegation from the reviewed
    /// relaunch `agent_delegate` promised; the head still runs first.
    #[test]
    fn reviewed_relaunch_is_not_blocked_by_an_ineligible_head_ask() {
        let (mut world, dir) = world("reviewed-relaunch-head");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), director: true, ..Default::default() },
            crate::asks::children::AgentFacts {
                cutex_session_id: TARGET.into(),
                project: Some("alpha".into()),
                explicit_launch: true,
                reviewed_relaunch: true,
                ..Default::default()
            },
        ];
        let (store, store_dir) = temp_store("reviewed-relaunch-head");
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        let mut owner = new_ask("owner", "owner work");
        owner.requester = Requester::Owner;
        let first = store.with_state(|state| state.create_ask(owner, now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        assert!(runtime.relaunches.borrow().is_empty(), "the owner's ask alone never relaunches");
        let second = store.with_state(|state| state.create_ask(new_ask("director", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now + chrono::Duration::seconds(1)).unwrap();
        assert_eq!(runtime.relaunches.borrow().as_slice(), [(TARGET.to_string(), REQUESTER.to_string())]);
        assert_eq!(runtime.relaunch_asks.borrow().as_slice(), [second.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.waiting_for_target(TARGET).first().map(|ask| ask.ask_id.clone()), Some(first), "queue order unchanged");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r208 C: a target this process has no connection to, but whose runtime
    /// binding is live (started by the Agent Bus process), is reconnected,
    /// never woken; its asks wait `heldByRuntime(reconnecting)` (no waiter
    /// wake-up), and an answer to such a requester triggers a reconnect.
    #[test]
    fn live_binding_without_connection_reconnects_instead_of_waking() {
        let (mut world, dir) = world("reconnect");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), ..Default::default() },
            crate::asks::children::AgentFacts { cutex_session_id: TARGET.into(), project: Some("alpha".into()), ..Default::default() },
        ];
        let (store, store_dir) = temp_store("reconnect");
        let runtime = FakeRuntime::default();
        runtime.set(TARGET, |status| status.reconnectable = true);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].hold, Some(Hold::HeldByRuntime { native: Some(HOLD_RECONNECTING.into()) }));
        assert!(state.deliveries.is_empty(), "reconnecting does not wake waiters");
        assert_eq!(runtime.reconnects.borrow().as_slice(), [TARGET.to_string()]);
        assert!(runtime.wakes.borrow().is_empty(), "never launched");
        // Connected now: admitted.
        runtime.set(TARGET, |status| {
            status.online = true;
            status.reconnectable = false;
            status.exclusive_admission = true;
        });
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask.clone()]);
        // The requester's runtime is live but not connected: reconnect it
        // for the answer event.
        runtime.set(REQUESTER, |status| status.reconnectable = true);
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "completed"}}), now).unwrap();
        finish_turn(&runtime, &store, TARGET);
        assert!(tick(&world, &runtime, &store, now).unwrap().events.is_empty());
        assert!(runtime.reconnects.borrow().contains(&REQUESTER.to_string()));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r208 D: an offline target cutex can't start (explicit launch without a
    /// live runtime, or a failed wake) gives the requester an actionable
    /// `blockedOwnerAction` item once, delivered as an event when it is idle,
    /// and the owner one notice.
    #[test]
    fn owner_action_needed_wakes_the_requester_once() {
        let (mut world, dir) = world("owner-action");
        world.facts = vec![
            crate::asks::children::AgentFacts { cutex_session_id: REQUESTER.into(), project: Some("alpha".into()), ..Default::default() },
            crate::asks::children::AgentFacts {
                cutex_session_id: TARGET.into(),
                project: Some("alpha".into()),
                explicit_launch: true,
                ..Default::default()
            },
        ];
        let (store, store_dir) = temp_store("owner-action");
        let runtime = FakeRuntime::idle(&[REQUESTER]);
        let now = Utc::now();
        let mut passive = new_ask("a", "x");
        passive.options.notify = Notify::Passive;
        store.with_state(|state| state.create_ask(passive, now).unwrap()).unwrap();
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert!(runtime.wakes.borrow().is_empty(), "explicit launch: never auto-started");
        let state = store.snapshot().unwrap();
        let owner_items: Vec<&DeliveryRecord> = state
            .deliveries
            .values()
            .filter(|d| d.ref_id == crate::asks::delivery::BLOCKED_OWNER_ACTION)
            .collect();
        assert_eq!(owner_items.len(), 1);
        assert!(!owner_items[0].passive, "actionable even for notify=passive");
        assert_eq!(state.notices.iter().filter(|n| n.method == crate::asks::wake::OWNER_ACTION_EVENT).count(), 1);
        // The idle requester got it as one event in the same tick.
        assert_eq!(report.events.len(), 1);
        let submitted = runtime.submitted.borrow();
        let (session, event) = submitted.last().unwrap();
        assert_eq!(session, REQUESTER);
        assert!(event.text.contains("blockedOwnerAction") && event.text.contains("only the owner"), "{}", event.text);
        drop(submitted);
        // Later ticks: no second item, no second notice.
        let turn = store.snapshot().unwrap().leases[REQUESTER].turn_id.clone().unwrap();
        crate::asks::observe::observe(&store, REQUESTER, "turn/completed", &json!({"turn": {"id": turn, "status": "completed"}}), now).unwrap();
        finish_turn(&runtime, &store, REQUESTER);
        assert!(tick(&world, &runtime, &store, now).unwrap().events.is_empty());
        let state = store.snapshot().unwrap();
        assert_eq!(state.deliveries.values().filter(|d| d.ref_id == crate::asks::delivery::BLOCKED_OWNER_ACTION).count(), 1);
        assert_eq!(state.notices.iter().filter(|n| n.method == crate::asks::wake::OWNER_ACTION_EVENT).count(), 1);

        // A failed wake recorded by the runtime does the same for a new ask.
        let second = store.with_state(|state| state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id).unwrap();
        let fresh = store
            .with_state(|state| crate::asks::wake::record_owner_action(state, TARGET, "human_runtime_route_required", Some("x"), now))
            .unwrap();
        assert_eq!(fresh, vec![second]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// r208 G: the owner stopping an ask's turn gives the requester an
    /// actionable `ownerStopped` item (even for notify=passive), delivered
    /// as an event when it is idle.
    #[test]
    fn owner_stop_of_an_ask_turn_wakes_the_requester() {
        let (world, dir) = world("owner-stop");
        let (store, store_dir) = temp_store("owner-stop");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let mut passive = new_ask("a", "x");
        passive.options.notify = Notify::Passive;
        let ask = store.with_state(|state| state.create_ask(passive, now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "interrupted"}}), now).unwrap();
        finish_turn(&runtime, &store, TARGET);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].attempts[0].interrupt_reason, Some(InterruptReason::OwnerStopped));
        let stopped: Vec<&DeliveryRecord> = state.deliveries.values().filter(|d| d.ref_id.starts_with("ownerStopped@")).collect();
        assert_eq!(stopped.len(), 1);
        assert!(!stopped[0].passive);
        let report = tick(&world, &runtime, &store, now).unwrap();
        assert_eq!(report.events.len(), 1);
        let submitted = runtime.submitted.borrow();
        let (session, event) = submitted.last().unwrap();
        assert_eq!(session, REQUESTER);
        assert!(event.text.contains("is now ownerStopped") && event.text.contains("The owner stopped the turn"), "{}", event.text);
        drop(submitted);
        // A requester stop is not an owner stop.
        let (store2, dir2) = temp_store("owner-stop2");
        let runtime2 = FakeRuntime::idle(&[TARGET]);
        let ask2 = store2.with_state(|state| state.create_ask(new_ask("b", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime2, &store2, now).unwrap();
        crate::asks::tools::close_ask(&store2, Some(REQUESTER), &ask2, "stop", now).unwrap();
        crate::asks::observe::observe(&store2, TARGET, "turn/completed", &json!({"turn": {"id": "turn-1", "status": "interrupted"}}), now).unwrap();
        assert!(!store2.snapshot().unwrap().deliveries.values().any(|d| d.ref_id.starts_with("ownerStopped@")));
        for path in [dir, store_dir, dir2] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// r208 E: a compaction observed during an attempt flags the answer
    /// `compacted` (metadata and requester flags) and steers one reminder
    /// with the brief into the turn.
    #[test]
    fn compaction_mid_attempt_flags_the_answer_and_reminds_once() {
        let (world, dir) = world("compaction");
        let (store, store_dir) = temp_store("compaction");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "Review the parser"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let observe = |method: &str, params: Value| crate::asks::observe::observe(&store, TARGET, method, &params, now).unwrap();
        observe("item/started", json!({"turnId": "turn-1", "item": {"type": "contextCompaction", "id": "c1"}}));
        assert!(!store.snapshot().unwrap().asks[&ask].attempts[0].compacted_mid_attempt, "started is not done");
        observe("item/completed", json!({"turnId": "turn-1", "item": {"type": "contextCompaction", "id": "c1"}}));
        // The same compaction's thread notification is not a second one.
        observe("thread/compacted", json!({"threadId": "t", "turnId": "turn-1"}));
        tick(&world, &runtime, &store, now).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let steered = runtime.steered.borrow();
        assert_eq!(steered.len(), 1, "reminded once");
        assert_eq!(steered[0].0, TARGET);
        assert_eq!(steered[0].1.event_type, render::REMINDER_EVENT);
        assert!(steered[0].1.text.contains(&crate::short_ids::agent_id(&ask)) && steered[0].1.text.contains("Review the parser"), "{}", steered[0].1.text);
        drop(steered);
        let record = store.snapshot().unwrap().asks[&ask].attempts[0].clone();
        assert!(record.compacted_mid_attempt && !record.compaction_reminder_pending);
        assert_eq!(record.compaction_reminders, 1);
        observe("item/completed", json!({"turnId": "turn-1", "item": {"type": "agentMessage", "text": "done"}}));
        observe("turn/completed", json!({"turn": {"id": "turn-1", "status": "completed"}}));
        let state = store.snapshot().unwrap();
        let answer = &state.answers[state.asks[&ask].answer_id.as_ref().unwrap()];
        assert!(answer.metadata.compacted);
        let delivery = state.deliveries.values().find(|d| d.kind == DeliveryKind::Answer).unwrap();
        let item = crate::asks::returns::return_item(&store, &state, delivery).unwrap();
        assert_eq!(crate::asks::returns::item_json(&item)["flags"], json!(["compacted"]));
        let (_, text) = render::return_text(&[item]);
        assert!(text.contains("context was compacted"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// The Linux-host case: a review ask runs, its target goes offline and is
    /// retired, the requester abandons it. The interrupt can never reach
    /// the target; the archived target settles the stop at once: the ask
    /// closes `stopped` with review `abandoned`, like a normal stopped ask
    /// (answer record, `cutex/ask/answered`, delivery), and frees the slot.
    #[test]
    fn abandoned_running_ask_to_an_archived_target_settles() {
        let (mut world, dir) = world("settle-archived");
        let (store, store_dir) = temp_store("settle-archived");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let mut review = new_ask("a", "x");
        review.options.review = true;
        let ask = store.with_state(|state| state.create_ask(review, now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/started", &json!({"turn": {"id": "turn-1"}}), now).unwrap();
        runtime.set(TARGET, |status| {
            status.online = false;
            status.active_turn_id = None;
        });
        let closed = crate::asks::tools::close_ask(&store, Some(REQUESTER), &ask, "abandoned", now).unwrap();
        assert_eq!(closed["stopping"], true);
        // Offline (not archived yet), inside the grace: the stop waits.
        tick(&world, &runtime, &store, now).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.asks[&ask].is_open());
        assert_eq!(state.asks[&ask].execution, ExecutionState::Running);
        assert_eq!(state.commands.len(), 1, "the stop is still queued");
        assert!(state.asks[&ask].attempts[0].stop_offline_since.is_some());
        assert!(runtime.interrupts.borrow().is_empty());
        // Retired: settles at once.
        world.directory.agents.get_mut(TARGET).unwrap().archived = true;
        let later = now + chrono::Duration::seconds(5);
        let report = tick(&world, &runtime, &store, later).unwrap();
        assert_eq!(report.interrupted, vec![ask.clone()]);
        let state = store.snapshot().unwrap();
        let record = &state.asks[&ask];
        assert_eq!(record.execution, ExecutionState::Interrupted);
        assert_eq!(record.closed, Some(CloseOutcome::Stopped));
        assert_eq!(record.review, Some(ReviewState::Abandoned));
        assert_eq!(record.attempts[0].state, AttemptState::Interrupted);
        assert_eq!(record.attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert!(record.attempts[0].stop_requested);
        assert!(record.attempts[0].stop_offline_since.is_none());
        assert!(state.leases.is_empty(), "the admission slot is free");
        assert_eq!(state.answers[record.answer_id.as_ref().unwrap()].status, AnswerStatus::Interrupted);
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/answered"));
        assert_eq!(state.deliveries.values().filter(|delivery| delivery.kind == DeliveryKind::Answer).count(), 1);
        // The queued stop is dropped on the next tick (attempt over).
        tick(&world, &runtime, &store, later).unwrap();
        assert!(store.snapshot().unwrap().commands.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// A stop pending on an offline target: a reconnectable runtime never
    /// settles; an offline, not reconnectable one settles only after it was
    /// seen so for the whole grace (`stopOfflineGraceSecs`, default 300 s);
    /// seeing it reconnectable in between starts the grace over.
    #[test]
    fn offline_stop_settles_after_the_grace_and_never_while_reconnecting() {
        let (world, dir) = world("settle-offline");
        let (store, store_dir) = temp_store("settle-offline");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let grace = chrono::Duration::seconds(crate::asks::budget::STOP_OFFLINE_GRACE_SECS as i64);
        let now = Utc::now();
        let (ask, next) = store
            .with_state(|state| {
                (state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id, state.create_ask(new_ask("b", "y"), now).unwrap().0.ask_id)
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        runtime.set(TARGET, |status| {
            status.online = false;
            status.reconnectable = true;
            status.active_turn_id = None;
        });
        crate::asks::tools::close_ask(&store, Some(REQUESTER), &ask, "stop", now).unwrap();
        let open = |at| {
            tick(&world, &runtime, &store, at).unwrap();
            let state = store.snapshot().unwrap();
            (state.asks[&ask].is_open(), state.asks[&ask].attempts[0].stop_offline_since.clone())
        };
        // Reconnecting: never settles, however long.
        assert_eq!(open(now), (true, None));
        assert_eq!(open(now + grace * 10), (true, None));
        // Gone for good: the grace starts at the first tick that sees it.
        runtime.set(TARGET, |status| status.reconnectable = false);
        let t1 = now + grace * 10 + chrono::Duration::seconds(1);
        assert_eq!(open(t1), (true, Some(crate::asks::now_rfc3339(t1))));
        assert!(open(t1 + grace - chrono::Duration::seconds(1)).0);
        // Reconnectable again just before the grace ends: starts over.
        runtime.set(TARGET, |status| status.reconnectable = true);
        assert_eq!(open(t1 + grace), (true, None));
        runtime.set(TARGET, |status| status.reconnectable = false);
        let t2 = t1 + grace + chrono::Duration::seconds(1);
        assert_eq!(open(t2), (true, Some(crate::asks::now_rfc3339(t2))));
        assert!(open(t2 + grace - chrono::Duration::seconds(1)).0);
        let report = tick(&world, &runtime, &store, t2 + grace).unwrap();
        assert_eq!(report.interrupted, vec![ask.clone()]);
        let state = store.snapshot().unwrap();
        assert_eq!(state.asks[&ask].closed, Some(CloseOutcome::Stopped));
        assert_eq!(state.asks[&ask].attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert!(state.leases.is_empty());
        // The other ask was never touched and is admitted once the target is back.
        assert!(state.asks[&next].is_open());
        assert_eq!(state.asks[&next].execution, ExecutionState::Queued);
        runtime.set(TARGET, |status| {
            status.online = true;
            status.runtime_generation = 2;
        });
        assert_eq!(tick(&world, &runtime, &store, t2 + grace).unwrap().admitted, vec![next]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// An offline target without a pending stop is left to reconciliation
    /// (it may come back and finish the turn); only an archived one settles.
    #[test]
    fn offline_running_ask_without_a_stop_is_not_settled() {
        let (world, dir) = world("settle-nostop");
        let (store, store_dir) = temp_store("settle-nostop");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store.with_state(|state| state.create_ask(new_ask("a", "x"), now).unwrap().0.ask_id).unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        runtime.set(TARGET, |status| status.online = false);
        tick(&world, &runtime, &store, now + chrono::Duration::hours(2)).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.asks[&ask].is_open());
        assert!(state.asks[&ask].attempts[0].stop_offline_since.is_none());
        assert_eq!(state.leases.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
