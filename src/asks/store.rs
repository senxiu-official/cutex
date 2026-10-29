//! The ask store: one private, file-locked JSON document under
//! `<cutex runtime dir>/asks/`, shared by the Agent Bus process and the
//! management process (same pattern as `crate::approvals`), plus a
//! content-addressed artifact directory next to it (§8.6).
//!
//! Every mutation goes through [`AskStore::with_state`], which holds an
//! exclusive lock across read-modify-write and bumps `revision` when
//! anything changed (cheap change detection for waits). Operations on
//! [`AskState`] are pure and unit-tested without files.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::model::*;
use super::{new_id, now_rfc3339, parse_time};

/// Closed asks (and their answers, questions, deliveries and unreferenced
/// artifacts) are pruned after this long.
const CLOSED_RETENTION_DAYS: i64 = 30;
/// Idempotency records of finished tool actions are kept this long.
const ACTION_RETENTION_DAYS: i64 = 7;
/// Bound on queued Management v2 notices when the management process is
/// down (oldest dropped; clients re-read resources on reconnect).
const MAX_NOTICES: usize = 2000;

/// Runtime presence of one agent (§5.5: idle, busy, paused or offline,
/// with queue length).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    pub online: bool,
    pub busy: bool,
    pub paused: bool,
    /// Owner queued items (asks are counted from the store).
    pub owner_queue: usize,
    /// Online but not offering native exclusive admission (asks wait).
    #[serde(default)]
    pub asks_unsupported: bool,
    /// The running native turn (agent_tell wake loop guard, sender turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Runtime generation a lifecycle action in another process (Agent Bus:
    /// `member_runtime` / `agent_create`) proved online. The management
    /// process keeps the agent online for that generation while its live
    /// durable binding remains, even before it holds its own connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_generation: Option<u64>,
    /// Durable whole-agent admission gate. Older releases ignore this
    /// additive field while still decoding the known `presence` row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pause: Option<AgentPause>,
}

/// Durable whole-agent admission gate. Unlike [`AdmissionPause`], this
/// blocks every source of a new native turn, not only delegations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentPausePhase {
    Pausing,
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPause {
    pub cutex_session_id: String,
    pub phase: AgentPausePhase,
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_cutex_session_id: Option<String>,
}

impl Presence {
    pub fn status(&self) -> &'static str {
        match (self.online, self.paused, self.busy) {
            (false, _, _) => "offline",
            (true, true, _) => "paused",
            (true, false, true) => "busy",
            (true, false, false) => "idle",
        }
    }
}

/// An in-flight `agent_wait` (the wait registry, §5.8). Durable so the
/// deadlock guard and owner-steer cancellation work across processes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitRecord {
    pub wait_id: String,
    pub cutex_session_id: String,
    pub action_id: String,
    pub asks: Vec<String>,
    pub all: bool,
    pub started_at: String,
    pub deadline: String,
    /// The session's cancel epoch when the wait started.
    pub cancel_epoch: u64,
}

/// Work only the management process can do (it holds the runtime
/// connections). Drained by the scheduler driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum RuntimeCommand {
    ProjectPauseInterrupt { project_id: String, pause_id: String, cutex_session_id: String, runtime_generation: u64, turn_id: String },
    /// Gate-preserving interrupt of a running attempt (N2).
    StopAttempt { ask_id: String, attempt: u32 },
    /// Send the resolution of a question to the native pending request.
    RespondQuestion { question_id: String },
    /// N2: reopen the native external-input gate after an owner resume.
    ResumeGate { cutex_session_id: String },
    /// `delegation_stop(stop_jobs)`: cancel the delegation's bound jobs
    /// through the handler's runtime (`mcpServer/tool/call` on `cutex_job`,
    /// delegation-await contract §5).
    CancelJobs { ask_id: String, cutex_session_id: String, jobs: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedCommand {
    pub command_id: String,
    pub command: RuntimeCommand,
    pub created_at: String,
}

/// A Management v2 event to publish on each listed session stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub method: String,
    pub sessions: Vec<String>,
    pub params: Value,
}

/// Idempotency record of one mutating tool call (`action_id`, §6.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRecord {
    pub tool: String,
    pub request_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub at: String,
}

/// What an `action_id` lookup found.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionLookup {
    New,
    /// The same request already finished; return its result again.
    Replay(Value),
    /// The same request is in flight in another call.
    InFlight,
    /// The id was used for a different request.
    Conflict,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskState {
    /// Security-sensitive routing state is a separate SQLite singleton.
    /// Old readers reject its unknown row instead of dropping restrictions.
    #[serde(default)]
    pub routing: super::routing_policy::RoutingState,
    #[serde(default)]
    pub project_control: super::project_control::ProjectControlState,
    #[serde(default)]
    pub lineage: super::lineage::LineageState,
    /// Close may overtake an unacknowledged PUT. Keep its receipt under the same lock.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_cancellations: BTreeMap<String, (String, crate::peers::model::Op)>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub next_seq: u64,
    #[serde(default)]
    pub asks: BTreeMap<String, AskRecord>,
    #[serde(default)]
    pub answers: BTreeMap<String, AnswerRecord>,
    #[serde(default)]
    pub questions: BTreeMap<String, QuestionRecord>,
    #[serde(default)]
    pub deliveries: BTreeMap<String, DeliveryRecord>,
    #[serde(default)]
    pub events: BTreeMap<String, EventOutboxRecord>,
    /// Admission lease per target session.
    #[serde(default)]
    pub leases: BTreeMap<String, AdmissionLease>,
    /// Next-step results events in flight while `leases` is held by the
    /// requester's running turn (a delegation-event or ask turn): session →
    /// the record a next-step lease would be (holder `Event`, no turn). At
    /// most one per session; moved into `leases` as soon as that slot is
    /// free ([`super::returns::promote_next_step_leases`]). Additive: older
    /// releases ignore (and drop) it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub next_step_leases: BTreeMap<String, AdmissionLease>,
    #[serde(default)]
    pub pauses: BTreeMap<String, AdmissionPause>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactRecord>,
    #[serde(default)]
    pub waits: BTreeMap<String, WaitRecord>,
    /// Session → wait cancel epoch (owner steer / interrupt / input).
    #[serde(default)]
    pub wait_cancel: BTreeMap<String, u64>,
    /// `<caller>\u{1f}<action_id>` → idempotency record.
    #[serde(default)]
    pub actions: BTreeMap<String, ActionRecord>,
    #[serde(default)]
    pub commands: Vec<QueuedCommand>,
    #[serde(default)]
    pub notices: Vec<Notice>,
    /// Budget key (domain) → auto-started turn times in the last hour.
    #[serde(default)]
    pub auto_turns: BTreeMap<String, Vec<String>>,
    /// Runtime presence of agents for `agent_list` (written by the
    /// management process, which holds the runtime connections).
    #[serde(default)]
    pub presence: BTreeMap<String, Presence>,
    /// Agents the owner took offline on purpose (never auto-woken, §8.4).
    #[serde(default)]
    pub owner_offline: std::collections::BTreeSet<String>,
    /// Agents auto-woken for asks: session → (budget domain, when).
    #[serde(default)]
    pub woken: BTreeMap<String, (String, String)>,
    /// Offline targets cutex can't bring online (r208): target → the asks
    /// whose requester and the owner were told (`blockedOwnerAction`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owner_action: BTreeMap<String, Vec<String>>,
    /// `<requester>\u{1f}<target>` → the reason the requester was given an
    /// actionable `blockedOwnerAction` item for (one per pair and reason
    /// while the target stays blocked). Unknown to r232 and older (ignored,
    /// dropped on their save: at most one extra item).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owner_action_told: BTreeMap<String, String>,
    /// The delegation tool level of each session's facade, as last seen on
    /// a tool call (`X-Cutex-Facade`, delegation-await contract §6). Absent:
    /// a facade from before the header (hints name no optional tools).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facades: BTreeMap<String, FacadeSeen>,
}

/// Compare persisted values without encoding the complete history. Only peer
/// ask records contain normalizing/custom serializers (including nested wire
/// snapshots); their changed records still use exact serialized equality.
fn persisted_state_equal(before: &AskState, after: &AskState) -> anyhow::Result<bool> {
    // Exhaustive destructuring makes every new store field require an explicit
    // choice here. These fields use derived serialization; optional/empty skips
    // do not collapse distinct values of their Rust types.
    let AskState {
        peer_cancellations, revision, next_seq, asks, answers, questions,
        deliveries, events, leases, next_step_leases, pauses, artifacts, waits,
        wait_cancel, actions, commands, notices, auto_turns, presence,
        owner_offline, woken, owner_action, owner_action_told, facades,
        routing, project_control, lineage,
    } = before;
    if lineage != &after.lineage || project_control != &after.project_control || routing != &after.routing || peer_cancellations != &after.peer_cancellations
        || revision != &after.revision || next_seq != &after.next_seq
        || answers != &after.answers || questions != &after.questions
        || deliveries != &after.deliveries || events != &after.events
        || leases != &after.leases || next_step_leases != &after.next_step_leases
        || pauses != &after.pauses || artifacts != &after.artifacts
        || waits != &after.waits || wait_cancel != &after.wait_cancel
        || actions != &after.actions || commands != &after.commands
        || notices != &after.notices || auto_turns != &after.auto_turns
        || presence != &after.presence || owner_offline != &after.owner_offline
        || woken != &after.woken || owner_action != &after.owner_action
        || owner_action_told != &after.owner_action_told
        || facades != &after.facades || asks.len() != after.asks.len()
    {
        return Ok(false);
    }
    for (id, previous) in asks {
        let Some(current) = after.asks.get(id) else { return Ok(false); };
        if previous == current {
            continue;
        }
        let has_peer = |ask: &AskRecord| ask.remote.is_some() || ask.peer_inbound.is_some();
        if (!has_peer(previous) && !has_peer(current))
            || serde_json::to_vec(previous)? != serde_json::to_vec(current)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Make changed peer asks match the bytes their custom serializers persist.
///
/// Peer diagnostics and nested wire snapshots deliberately normalize remote
/// prose while serializing.  The SQLite writer caches the post-commit Rust
/// state, so caching the caller's unnormalized value would make an immediate
/// read disagree with the durable row.  Only structurally changed peer rows
/// take this bounded round trip; ordinary local asks and untouched history do
/// not.
fn canonicalize_changed_peer_asks(
    before: &AskState,
    after: &mut AskState,
) -> anyhow::Result<()> {
    let changed = after
        .asks
        .iter()
        .filter(|(id, ask)| {
            (ask.remote.is_some() || ask.peer_inbound.is_some())
                && before.asks.get(*id) != Some(*ask)
        })
        .map(|(id, ask)| Ok((id.clone(), serde_json::to_vec(ask)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (id, bytes) in changed {
        after.asks.insert(id, serde_json::from_slice(&bytes)?);
    }
    Ok(())
}

/// A facade level seen on a tool call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacadeSeen {
    pub level: u32,
    pub at: String,
}

/// A refusal that changed nothing (`no_write` with a code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskError {
    pub code: &'static str,
    pub detail: String,
    pub extra: Value,
}

impl AskError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into(), extra: Value::Null }
    }

    pub fn with(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "ok": false,
            "status": "no_write",
            "code": self.code,
            "detail": self.detail,
        });
        if let (Some(object), Some(extra)) = (value.as_object_mut(), self.extra.as_object()) {
            for (key, item) in extra {
                object.insert(key.clone(), item.clone());
            }
        }
        value
    }
}

impl std::fmt::Display for AskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

/// Everything needed to create an ask; validated by the caller (tools or
/// Management v2) except for store-level rules (dedupe, overload).
#[derive(Clone, Debug)]
pub struct NewAsk {
    /// Pre-chosen id (the gate binds the snapshot before creation).
    pub ask_id: Option<String>,
    pub action_id: String,
    pub requester: Requester,
    pub target_cutex_session_id: String,
    pub target_name: Option<String>,
    pub project_id: Option<String>,
    pub body: AskBody,
    /// Short label from the requester (`agent_delegate title`); default the
    /// brief's first line.
    pub title: Option<String>,
    pub options: AskOptions,
    pub follow_up_of: Option<String>,
    pub parent_ask: Option<String>,
    pub gate: Option<GateRecord>,
}

impl NewAsk {
    /// Digest of the immutable request.
    pub fn request_sha256(&self) -> String {
        crate::approvals::request_digest(&json!({
            "target": self.target_cutex_session_id,
            "textSha256": self.body.text_sha256,
            "attachments": self.body.attachments,
            "options": self.options,
            "followUpOf": self.follow_up_of,
        }))
    }
}

/// Filter for listings (`GET /v2/asks`, `agent_list`).
#[derive(Clone, Debug, Default)]
pub struct AskFilter {
    pub project: Option<String>,
    /// Session id, or `owner`.
    pub requester: Option<String>,
    pub target: Option<String>,
    /// `open`, `closed`, or an execution state name.
    pub state: Option<String>,
    /// Requester or target.
    pub involving: Option<String>,
}

fn execution_name(state: ExecutionState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl AskFilter {
    pub fn matches(&self, ask: &AskRecord) -> bool {
        let requester_key = ask.requester.session().unwrap_or(if ask.requester.is_owner(){"owner"}else{"peer"});
        self.project.as_deref().is_none_or(|project| ask.project_id.as_deref() == Some(project))
            && self.requester.as_deref().is_none_or(|requester| requester == requester_key)
            && self.target.as_deref().is_none_or(|target| target == ask.target_cutex_session_id)
            && self.involving.as_deref().is_none_or(|session| {
                session == requester_key || session == ask.target_cutex_session_id || ask.readable_by(session)
            })
            && self.state.as_deref().is_none_or(|state| match state {
                "open" => ask.is_open(),
                "closed" => !ask.is_open(),
                other => execution_name(ask.execution) == other,
            })
    }
}

/// Queue position (1-based) of an ask among the target's queued asks.
pub fn queue_position(state: &AskState, ask: &AskRecord) -> Option<usize> {
    if ask.remote.is_some() || ask.execution != ExecutionState::Queued {
        return None;
    }
    let ahead = state
        .asks
        .values()
        .filter(|other| {
            other.remote.is_none() && other.target_cutex_session_id == ask.target_cutex_session_id
                && other.execution == ExecutionState::Queued
                && other.seq < ask.seq
        })
        .count();
    Some(ahead + 1)
}

impl AskState {
    fn action_key(caller: &str, action_id: &str) -> String {
        format!("{caller}\u{1f}{action_id}")
    }

    /// Look up (and on `New`, reserve) an action id for a mutating tool.
    pub fn begin_action(&mut self, caller: &str, action_id: &str, tool: &str, request_sha256: &str, now: DateTime<Utc>) -> ActionLookup {
        let key = Self::action_key(caller, action_id);
        match self.actions.get(&key) {
            Some(record) if record.tool != tool || record.request_sha256 != request_sha256 => ActionLookup::Conflict,
            Some(ActionRecord { result: Some(result), .. }) => ActionLookup::Replay(result.clone()),
            Some(_) => ActionLookup::InFlight,
            None => {
                self.actions.insert(
                    key,
                    ActionRecord {
                        tool: tool.into(),
                        request_sha256: request_sha256.into(),
                        result: None,
                        at: now_rfc3339(now),
                    },
                );
                ActionLookup::New
            }
        }
    }

    /// Record the result of a reserved action (replayed on retry).
    pub fn finish_action(&mut self, caller: &str, action_id: &str, result: &Value) {
        if let Some(record) = self.actions.get_mut(&Self::action_key(caller, action_id)) {
            record.result = Some(result.clone());
        }
    }

    /// Release a reservation whose action did nothing (a retry may proceed).
    pub fn release_action(&mut self, caller: &str, action_id: &str) {
        let key = Self::action_key(caller, action_id);
        if self.actions.get(&key).is_some_and(|record| record.result.is_none()) {
            self.actions.remove(&key);
        }
    }

    pub fn notice(&mut self, method: &str, sessions: Vec<String>, params: Value) {
        let mut sessions = sessions;
        sessions.sort();
        sessions.dedup();
        self.notices.push(Notice { method: method.into(), sessions, params });
        if self.notices.len() > MAX_NOTICES {
            let excess = self.notices.len() - MAX_NOTICES;
            self.notices.drain(..excess);
        }
    }

    /// Sessions whose streams carry events about this ask, and the readers of
    /// its artifacts: the target, the original requester and (design 044) the
    /// current controller when a handover moved it. Never a question-only
    /// recipient (`questions: director`): it sees the brief nowhere.
    pub fn ask_sessions(ask: &AskRecord) -> Vec<String> {
        let mut sessions = if ask.remote.is_none() { vec![ask.target_cutex_session_id.clone()] } else { vec![] };
        if let Some(requester) = ask.requester.session() {
            sessions.push(requester.to_string());
        }
        if let Some(controller) = ask.controller().ok().flatten() {
            if !sessions.contains(&controller) {
                sessions.push(controller);
            }
        }
        sessions
    }

    /// The audience of the ask's question notices: [`Self::ask_sessions`] and
    /// the current question recipient (only the question, never the brief).
    pub fn question_sessions(ask: &AskRecord) -> Vec<String> {
        let mut sessions = Self::ask_sessions(ask);
        if let Some(recipient) = super::model::route::question_recipient(ask).ok().flatten() {
            if !sessions.contains(&recipient) {
                sessions.push(recipient);
            }
        }
        sessions
    }

    /// The ask with this requester-scoped action id.
    pub fn find_by_action(&self, requester: &Requester, action_id: &str) -> Option<&AskRecord> {
        self.asks
            .values()
            .find(|ask| crate::peers::asks::same_requester(&ask.requester,requester) && ask.action_id == action_id)
    }

    /// Queued (or under review) asks to a target, in FIFO order.
    pub fn waiting_for_target(&self, target: &str) -> Vec<&AskRecord> {
        let mut asks: Vec<&AskRecord> = self
            .asks
            .values()
            .filter(|ask| {
                (ask.remote.is_none() && ask.target_cutex_session_id == target)
                    && ask.remote.is_none()
                    && matches!(ask.execution, ExecutionState::Queued | ExecutionState::PendingReview)
            })
            .collect();
        asks.sort_by_key(|ask| ask.seq);
        asks
    }

    /// The record `create_ask` would write (no side effects; `seq` 0).
    pub fn inherited_hops(&self, parent: Option<&str>) -> u32 {
        let mut seen = std::collections::BTreeSet::new();
        let mut cursor = parent;
        let mut hops = 0;
        while let Some(id) = cursor {
            if !seen.insert(id) { break; }
            let Some(a) = self.asks.get(id) else { break; };
            hops = hops.max(a.hops).max(a.peer_inbound.as_ref().map_or(0, |i| i.request.hops));
            cursor = a.parent_ask.as_deref();
        }
        hops
    }
    pub fn provisional(new: &NewAsk, now: DateTime<Utc>) -> AskRecord {
        let gated = !new.requester.is_owner() && new.gate.as_ref().is_some_and(|gate| gate.required);
        let now_text = now_rfc3339(now);
        AskRecord {
            routing_approval_id: None,
            routing_request: None,
            routing_grant_id: None,
            source_lineage: None,
            bounded_continuation: Default::default(),
            hops: 0,
            remote: None,
            peer_inbound: None,
            ask_id: new.ask_id.clone().unwrap_or_else(|| new_id("ask")),
            action_id: new.action_id.clone(),
            request_sha256: new.request_sha256(),
            requester: new.requester.clone(),
            target_cutex_session_id: new.target_cutex_session_id.clone(),
            target_name: new.target_name.clone(),
            project_id: new.project_id.clone(),
            title: new.title.clone().unwrap_or_else(|| super::title_of(&new.body.text)),
            body: new.body.clone(),
            options: new.options.clone(),
            follow_up_of: new.follow_up_of.clone(),
            parent_ask: new.parent_ask.clone(),
            seq: 0,
            execution: if gated { ExecutionState::PendingReview } else { ExecutionState::Queued },
            hold: None,
            hold_since: None,
            review: None,
            gate: new.gate.clone(),
            attempts: Vec::new(),
            answer_id: None,
            target_reverted: false,
            closed: None,
            closed_at: None,
            created_at: now_text.clone(),
            updated_at: now_text,
            revision: 1,
            jobs: Vec::new(),
            awaiting: None,
            resume: None,
            revisions: Vec::new(),
            superseded_by: None,
            submit_retry: None,
            idle_reminded_at: None,
            quota_requeues: 0,
        }
    }

    /// Create an ask, or return the existing one for a retried `action_id`.
    /// `Ok((ask, created))`.
    pub fn create_ask(&mut self, new: NewAsk, now: DateTime<Utc>) -> Result<(AskRecord, bool), AskError> {
        let request_sha256 = new.request_sha256();
        if let Some(existing) = self.find_by_action(&new.requester, &new.action_id) {
            if existing.request_sha256 != request_sha256 {
                return Err(AskError::new(
                    "idempotency_conflict",
                    "this action_id was already used for a different delegation; use a new action_id",
                )
                .with(json!({"askId": existing.ask_id})));
            }
            return Ok((existing.clone(), false));
        }
        if self.waiting_for_target(&new.target_cutex_session_id).len() >= super::MAX_QUEUED_PER_TARGET {
            return Err(AskError::new(
                "target_overloaded",
                format!(
                    "the target already has {} waiting delegations; delegate to another agent or try later",
                    super::MAX_QUEUED_PER_TARGET
                ),
            ));
        }
        if let Some(previous) = new.follow_up_of.as_deref() {
            let Some(previous_ask) = self.asks.get(previous) else {
                return Err(AskError::new("ask_not_found", "the follow-up names no delegation"));
            };
            if previous_ask.requester.session() != new.requester.session()
                || previous_ask.target_cutex_session_id != new.target_cutex_session_id
            {
                return Err(AskError::new(
                    "invalid_arguments",
                    "a follow-up goes from the same requester to the same target as the delegation it continues",
                ));
            }
        }
        let mut ask = Self::provisional(&new, now);
        if ask.parent_ask.is_none() {
            ask.parent_ask=ask.requester.session().and_then(|caller|self.leases.get(caller))
                .and_then(|lease|match &lease.holder{LeaseHolder::Ask{ask_id,..}=>Some(ask_id.clone()),_=>None});
        }
        let lineage=super::lineage::capture_new(self,&ask,&super::routing_policy::integration::local_host_id())?;
        self.lineage.record(lineage.record)?;
        ask.source_lineage=lineage.source_lineage;
        self.next_seq += 1;
        ask.hops = self.inherited_hops(ask.parent_ask.as_deref());
        ask.seq = self.next_seq;
        // A follow-up supersedes a finished, still open predecessor (answered
        // and awaiting acceptance, or interrupted): it is closed
        // `superseded` with notices (delegation-await contract §4). A
        // running or waiting predecessor is left alone.
        if let Some(previous) = ask.follow_up_of.clone() {
            let supersede = self
                .asks
                .get(&previous)
                .is_some_and(|previous_ask| previous_ask.is_open() && previous_ask.execution.is_terminal());
            if supersede {
                let by = ask.ask_id.clone();
                self.update_ask(&previous, now, |previous_ask| {
                    if previous_ask.review == Some(ReviewState::AwaitingAcceptance) {
                        previous_ask.review = Some(ReviewState::ChangesRequested);
                    }
                    previous_ask.closed = Some(CloseOutcome::Superseded);
                    previous_ask.closed_at = Some(now_rfc3339(now));
                    previous_ask.superseded_by = Some(by);
                    previous_ask.awaiting = None;
                    previous_ask.resume = None;
                });
            }
        }
        for artifact in ask.body.attachments.iter().chain(ask.body.text_artifact.iter()) {
            if let Some(record) = self.artifacts.get_mut(artifact) {
                if !record.asks.contains(&ask.ask_id) {
                    record.asks.push(ask.ask_id.clone());
                }
                if !record.readers.contains(&ask.target_cutex_session_id) {
                    record.readers.push(ask.target_cutex_session_id.clone());
                }
            }
        }
        self.asks.insert(ask.ask_id.clone(), ask.clone());
        let sessions = Self::ask_sessions(&ask);
        self.notice("cutex/delegation/lineageChanged", sessions.clone(),
            json!({"projectId":ask.project_id,"resourceId":ask.ask_id,"revision":self.lineage.revision}));
        let mut created = serde_json::to_value(&ask).unwrap_or(Value::Null);
        if let Some(object) = created.as_object_mut() {
            object.insert("requester".into(), super::view::requester_value(&ask.requester));
        }
        self.notice("cutex/ask/created", sessions, json!({ "ask": created }));
        Ok((ask, true))
    }

    /// Mutate one ask; bumps its revision and records a `stateChanged`
    /// notice when execution, hold or review changed.
    pub fn update_ask<T>(
        &mut self,
        ask_id: &str,
        now: DateTime<Utc>,
        mutate: impl FnOnce(&mut AskRecord) -> T,
    ) -> Option<T> {
        let ask = self.asks.get_mut(ask_id)?;
        let before = (ask.execution, ask.hold.clone(), ask.review, ask.closed);
        let attempt_before = ask.current_attempt().map(|attempt| (attempt.n, attempt.turn_id.clone(), attempt.state));
        let stop_before = ask.current_attempt().and_then(|attempt| attempt.stop_intent.clone());
        let extras_before = (ask.jobs.clone(), ask.awaiting.clone(), ask.resume.clone(), ask.revisions.clone());
        let before_value = serde_json::to_value(&*ask).ok();
        let result = mutate(ask);
        if serde_json::to_value(&*ask).ok() == before_value {
            return Some(result);
        }
        ask.revision += 1;
        ask.updated_at = now_rfc3339(now);
        if before.1 != ask.hold {
            ask.hold_since = ask.hold.as_ref().map(|_| now_rfc3339(now));
        }
        let after = (ask.execution, ask.hold.clone(), ask.review, ask.closed);
        let attempt_after = ask.current_attempt().map(|attempt| (attempt.n, attempt.turn_id.clone(), attempt.state));
        let snapshot = ask.clone();
        // delegation-await §7: jobs, wait, resume and revisions changes
        // without a state change get `cutex/ask/updated` (clients refetch).
        let mut changed = Vec::new();
        if stop_before != snapshot.current_attempt().and_then(|attempt| attempt.stop_intent.clone()) {
            changed.push("stop");
        }
        if extras_before.0 != snapshot.jobs {
            changed.push("jobs");
        }
        if extras_before.1 != snapshot.awaiting {
            changed.push("await");
        }
        if extras_before.2 != snapshot.resume {
            changed.push("resume");
        }
        if extras_before.3 != snapshot.revisions {
            changed.push("revisions");
        }
        // r208: an attempt change (new attempt, turn bound, state) is a state
        // change too, so clients learn the turn to jump to.
        let state_changed = before.0 != after.0 || before.1 != after.1 || before.2 != after.2 || attempt_before != attempt_after;
        if !changed.is_empty() {
            self.notice(
                "cutex/ask/updated",
                Self::ask_sessions(&snapshot),
                json!({"askId": snapshot.ask_id, "revision": snapshot.revision, "changed": changed}),
            );
        }
        if state_changed {
            self.notice(
                "cutex/ask/stateChanged",
                Self::ask_sessions(&snapshot),
                json!({
                    "askId": snapshot.ask_id,
                    "execution": snapshot.execution,
                    "hold": snapshot.hold,
                    "review": snapshot.review,
                    "revision": snapshot.revision,
                    "attempt": snapshot.current_attempt().map(|attempt| json!({"n": attempt.n, "turnId": attempt.turn_id, "state": attempt.state})),
                }),
            );
        }
        if before.3.is_none() && after.3.is_some() {
            self.notice(
                "cutex/ask/closed",
                Self::ask_sessions(&snapshot),
                json!({"askId": snapshot.ask_id, "outcome": snapshot.closed, "revision": snapshot.revision}),
            );
        }
        Some(result)
    }

    /// Remove closed records past retention and expired idempotency
    /// records. Returns artifact ids that are no longer referenced.
    pub fn prune(&mut self, now: DateTime<Utc>) -> Vec<String> {
        self.prune_with(Some(CLOSED_RETENTION_DAYS), now)
    }

    /// Closed asks past `closed_retention_days` (archive them first).
    pub fn closed_before(&self, closed_retention_days: i64, now: DateTime<Utc>) -> Vec<String> {
        let closed_cutoff = now - chrono::Duration::days(closed_retention_days);
        self.asks
            .values()
            .filter(|ask| {
                if ask.remote.as_ref().is_some_and(|r| !r.sync.pending_ops.is_empty()) || ask.peer_inbound.as_ref().is_some_and(|i| !i.peer_deliveries.is_empty()) { return false; }
                ask.closed_at
                    .as_deref()
                    .and_then(parse_time)
                    .is_some_and(|closed| closed < closed_cutoff)
            })
            .map(|ask| ask.ask_id.clone())
            .collect()
    }

    /// [`Self::prune`] with the closed-ask retention of the settings;
    /// `None` keeps every ask (housekeeping only: the retention sweep
    /// archives closed asks before it removes them).
    pub fn prune_with(&mut self, closed_retention_days: Option<i64>, now: DateTime<Utc>) -> Vec<String> {
        let expired: Vec<String> = match closed_retention_days {
            Some(days) => self.closed_before(days, now),
            None => Vec::new(),
        };
        for ask_id in &expired {
            self.asks.remove(ask_id);
        }
        self.answers.retain(|_, answer| self.asks.contains_key(&answer.ask_id));
        self.questions.retain(|_, question| self.asks.contains_key(&question.ask_id));
        self.deliveries.retain(|_, delivery| self.asks.contains_key(&delivery.ask_id));
        let action_cutoff = now - chrono::Duration::days(ACTION_RETENTION_DAYS);
        self.actions
            .retain(|_, action| action.tool.starts_with(super::operations::PREFIX)
                || parse_time(&action.at).is_none_or(|at| at >= action_cutoff));
        let hour_ago = now - chrono::Duration::hours(1);
        for times in self.auto_turns.values_mut() {
            times.retain(|time| parse_time(time).is_some_and(|time| time >= hour_ago));
        }
        self.auto_turns.retain(|_, times| !times.is_empty());
        self.waits
            .retain(|_, wait| parse_time(&wait.deadline).is_none_or(|deadline| deadline + chrono::Duration::minutes(5) >= now));
        let mut orphaned = Vec::new();
        let asks = &self.asks;
        let answers = &self.answers;
        self.artifacts.retain(|artifact_id, record| {
            record.asks.retain(|ask| asks.contains_key(ask));
            let referenced = !record.asks.is_empty()
                || answers.values().any(|answer| answer.artifact_id.as_deref() == Some(artifact_id));
            // Fresh unattached uploads get a day to be attached.
            let fresh = parse_time(&record.created_at).is_some_and(|created| created + chrono::Duration::days(1) > now);
            if !referenced && !fresh {
                orphaned.push(artifact_id.clone());
            }
            referenced || fresh
        });
        orphaned
    }
}

fn encode_row<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(value).context("serialize ask SQLite row")
}

fn map_rows<T: Serialize>(
    rows: &mut crate::management::v2::hot_store::Rows,
    collection: &str,
    values: &BTreeMap<String, T>,
) -> anyhow::Result<()> {
    for (key, value) in values {
        rows.insert((collection.into(), key.clone()), encode_row(value)?);
    }
    Ok(())
}

fn decode_map<T: DeserializeOwned>(
    rows: &mut crate::management::v2::hot_store::Rows,
    collection: &str,
) -> anyhow::Result<BTreeMap<String, T>> {
    let keys = rows
        .keys()
        .filter(|(kind, _)| kind == collection)
        .cloned()
        .collect::<Vec<_>>();
    keys.into_iter()
        .map(|key| {
            let bytes = rows.remove(&key).expect("selected row exists");
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("decode ask SQLite row {}/{}", key.0, key.1))?;
            Ok((key.1, value))
        })
        .collect()
}

fn singleton<T: DeserializeOwned + Default>(
    rows: &mut crate::management::v2::hot_store::Rows,
    key: &str,
) -> anyhow::Result<T> {
    rows.remove(&("state".into(), key.into()))
        .map(|bytes| serde_json::from_slice(&bytes).with_context(|| format!("decode ask SQLite singleton {key}")))
        .transpose()
        .map(|value| value.unwrap_or_default())
}

pub(crate) fn ask_state_rows(state: &AskState) -> anyhow::Result<crate::management::v2::hot_store::Rows> {
    let mut rows = crate::management::v2::hot_store::Rows::new();
    macro_rules! maps {
        ($($field:ident),+ $(,)?) => { $(map_rows(&mut rows, stringify!($field), &state.$field)?;)+ };
    }
    maps!(
        peer_cancellations, asks, answers, questions, deliveries, events, leases,
        next_step_leases, pauses, artifacts, waits, wait_cancel, actions,
        auto_turns, presence, woken, owner_action, owner_action_told, facades,
    );
    for value in &state.owner_offline {
        rows.insert(("owner_offline".into(), value.clone()), b"true".to_vec());
    }
    for (key, value) in [
        ("revision", encode_row(&state.revision)?),
        ("next_seq", encode_row(&state.next_seq)?),
        ("commands", encode_row(&state.commands)?),
        ("notices", encode_row(&state.notices)?),
        ("routing", encode_row(&state.routing)?),
        ("project_control", encode_row(&state.project_control)?),
        ("lineage", encode_row(&state.lineage)?),
    ] {
        rows.insert(("state".into(), key.into()), value);
    }
    Ok(rows)
}

pub(crate) fn ask_state_from_rows(source: &crate::management::v2::hot_store::Rows) -> anyhow::Result<AskState> {
    ask_state_from_owned_rows(source.clone())
}

pub(crate) fn ask_state_from_owned_rows(mut rows: crate::management::v2::hot_store::Rows) -> anyhow::Result<AskState> {
    macro_rules! take_maps {
        ($($field:ident),+ $(,)?) => { $(let $field = decode_map(&mut rows, stringify!($field))?;)+ };
    }
    take_maps!(
        peer_cancellations, asks, answers, questions, deliveries, events, leases,
        next_step_leases, pauses, artifacts, waits, wait_cancel, actions,
        auto_turns, presence, woken, owner_action, owner_action_told, facades,
    );
    let owner_keys = rows
        .keys()
        .filter(|(kind, _)| kind == "owner_offline")
        .cloned()
        .collect::<Vec<_>>();
    let mut owner_offline = std::collections::BTreeSet::new();
    for key in owner_keys {
        let value: bool = serde_json::from_slice(&rows.remove(&key).expect("selected row exists"))?;
        anyhow::ensure!(value, "invalid false owner-offline ask row");
        owner_offline.insert(key.1);
    }
    let revision = singleton(&mut rows, "revision")?;
    let next_seq = singleton(&mut rows, "next_seq")?;
    let commands = singleton(&mut rows, "commands")?;
    let notices = singleton(&mut rows, "notices")?;
    let routing = singleton(&mut rows, "routing")?;
    let project_control = singleton(&mut rows, "project_control")?;
    let lineage = singleton(&mut rows, "lineage")?;
    anyhow::ensure!(rows.is_empty(), "unknown ask SQLite row collection");
    Ok(AskState {
        routing, project_control, lineage,
        peer_cancellations, revision, next_seq, asks, answers, questions,
        deliveries, events, leases, next_step_leases, pauses, artifacts, waits,
        wait_cancel, actions, commands, notices, auto_turns, presence,
        owner_offline, woken, owner_action, owner_action_told, facades,
    })
}

fn diff_map<T: Serialize + PartialEq>(
    changes: &mut Vec<crate::management::v2::hot_store::RowChange>,
    collection: &str,
    before: &BTreeMap<String, T>,
    after: &BTreeMap<String, T>,
) -> anyhow::Result<()> {
    let keys = before.keys().chain(after.keys()).collect::<std::collections::BTreeSet<_>>();
    for key in keys {
        match (before.get(key), after.get(key)) {
            (Some(old), Some(new)) if old == new => {}
            (_, Some(value)) => changes.push(crate::management::v2::hot_store::RowChange::Put {
                collection: collection.into(),
                key: key.clone(),
                value: encode_row(value)?,
            }),
            (Some(_), None) => changes.push(crate::management::v2::hot_store::RowChange::Delete {
                collection: collection.into(),
                key: key.clone(),
            }),
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

fn diff_singleton<T: Serialize + PartialEq>(
    changes: &mut Vec<crate::management::v2::hot_store::RowChange>,
    key: &str,
    before: &T,
    after: &T,
) -> anyhow::Result<()> {
    if before != after {
        changes.push(crate::management::v2::hot_store::RowChange::Put {
            collection: "state".into(),
            key: key.into(),
            value: encode_row(after)?,
        });
    }
    Ok(())
}

pub(crate) fn ask_state_changes(
    before: &AskState,
    after: &AskState,
) -> anyhow::Result<Vec<crate::management::v2::hot_store::RowChange>> {
    let mut changes = Vec::new();
    macro_rules! maps {
        ($($field:ident),+ $(,)?) => { $(diff_map(&mut changes, stringify!($field), &before.$field, &after.$field)?;)+ };
    }
    maps!(
        peer_cancellations, asks, answers, questions, deliveries, events, leases,
        next_step_leases, pauses, artifacts, waits, wait_cancel, actions,
        auto_turns, presence, woken, owner_action, owner_action_told, facades,
    );
    let keys = before.owner_offline.iter().chain(after.owner_offline.iter()).collect::<std::collections::BTreeSet<_>>();
    for key in keys {
        match (before.owner_offline.contains(key), after.owner_offline.contains(key)) {
            (false, true) => changes.push(crate::management::v2::hot_store::RowChange::Put {
                collection: "owner_offline".into(), key: key.clone(), value: b"true".to_vec(),
            }),
            (true, false) => changes.push(crate::management::v2::hot_store::RowChange::Delete {
                collection: "owner_offline".into(), key: key.clone(),
            }),
            _ => {}
        }
    }
    diff_singleton(&mut changes, "revision", &before.revision, &after.revision)?;
    diff_singleton(&mut changes, "next_seq", &before.next_seq, &after.next_seq)?;
    diff_singleton(&mut changes, "commands", &before.commands, &after.commands)?;
    diff_singleton(&mut changes, "notices", &before.notices, &after.notices)?;
    diff_singleton(&mut changes, "routing", &before.routing, &after.routing)?;
    diff_singleton(&mut changes, "project_control", &before.project_control, &after.project_control)?;
    diff_singleton(&mut changes, "lineage", &before.lineage, &after.lineage)?;
    // Every r266 write installs a row unknown to old readers. This also guards
    // continuation/lineage fields nested in otherwise familiar ask rows.
    if !changes.is_empty() && before.routing == after.routing {
        changes.push(crate::management::v2::hot_store::RowChange::Put {
            collection: "state".into(), key: "routing".into(), value: encode_row(&after.routing)?,
        });
    }
    Ok(changes)
}

/// The store on disk.
static WHOLE_STATE_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// One store only; the optional raw-row companion is capped at 64 MiB/100k rows.
type AskStateCache = Option<(PathBuf, u64, std::sync::Arc<AskState>, Option<crate::management::v2::hot_store::Rows>)>;

#[path = "store_diagnostics.rs"]
pub mod diagnostics;

// Declared before the OS lock guard in with_selection, so error/unwind paths
// also close the file before reclaiming the draft and old cached generation.
#[derive(Default)]
struct WriteReclaim {
    before: Option<std::sync::Arc<AskState>>,
    draft: Option<AskState>,
    replaced: AskStateCache,
}

#[cfg(test)]
impl Drop for WriteReclaim {
    fn drop(&mut self) { diagnostics::test_reclaim(self.draft.as_ref()); }
}
static ASK_STATE_CACHE: std::sync::Mutex<AskStateCache> = std::sync::Mutex::new(None);
#[path = "store/row_reuse.rs"]
mod row_reuse;
#[cfg(test)]
#[path = "store/read_perf_tests.rs"]
mod read_perf_tests;
// The cache mutex protects publication, not IO. Serialize cold refreshes
// separately: otherwise every reader seeing a new generation decodes all
// retained asks while holding a shared file lock, starving the next writer.
// Callers acquire the file lock before this guard, and this guard before the
// cache mutex. Hot cache hits do not acquire it.
static ASK_STATE_REFRESH: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "store_contention_tests.rs"]
mod contention_tests;
#[cfg(test)]
#[path = "store/reader_safety_tests.rs"]
mod reader_safety_tests;

#[cfg(test)]
thread_local! {
    static THREAD_WHOLE_STATE_COPIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_whole_state_copy() {
    WHOLE_STATE_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    THREAD_WHOLE_STATE_COPIES.with(|count| count.set(count.get() + 1));
}

/// Owned whole-state copies ([`AskStore::snapshot`], [`AskStore::peek`],
/// [`AskStore::with_state`]) made on the calling thread (tests run in
/// parallel, so assertions use this instead of the process-wide count).
#[cfg(test)]
pub(crate) fn whole_state_copies_on_this_thread() -> u64 {
    THREAD_WHOLE_STATE_COPIES.with(|count| count.get())
}

/// How many owned whole-state copies of the ask store this process made so
/// far. A cheap diagnostic counter for performance tests: the idle scheduler
/// pass must read the shared cached state ([`AskStore::inspect`],
/// [`AskStore::shared`]) and leave it unchanged.
pub fn whole_state_copy_count() -> u64 {
    WHOLE_STATE_COPIES.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
pub struct AskStore {
    dir: PathBuf,
    queue_root: PathBuf,
    host_queue: bool,
    cache_generation: bool,
}

impl AskStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let runtime = crate::config::paths::runtime_dir()?;
        let mut store = Self::with_queue(runtime.join("asks"), runtime.join("management-v2"), true);
        // Unit tests replace the process HOME repeatedly and remove each
        // fixture on Windows; keeping that fixture DB open would pin it.
        // The benchmark below enables this explicitly for one scoped store.
        store.cache_generation = !cfg!(test);
        Ok(store)
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let queue_root = dir.join("owner-input");
        Self::with_queue(dir, queue_root, false)
    }

    /// Open the host-wide store rooted at an arbitrary runtime directory.
    /// Offline discovery/rollback tools use this instead of resolving the
    /// current process home.
    pub(crate) fn at_runtime(runtime: impl Into<PathBuf>) -> Self {
        let runtime = runtime.into();
        Self::with_queue(runtime.join("asks"), runtime.join("management-v2"), true)
    }

    pub(crate) fn with_queue(dir: PathBuf, queue_root: PathBuf, host_queue: bool) -> Self {
        Self { dir, queue_root, host_queue, cache_generation: false }
    }

    pub fn queue_repository(&self) -> anyhow::Result<crate::management::v2::user_input::UserInputRepository> {
        crate::management::v2::user_input::UserInputRepository::open_with_asks(self.queue_root.clone(), self.dir.clone(), self.host_queue)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("asks-v1.json")
    }

    fn database_path(&self) -> PathBuf {
        if self.host_queue {
            self.queue_root.join(crate::management::v2::hot_store::DATABASE_FILE)
        } else {
            self.dir.join(crate::management::v2::hot_store::DATABASE_FILE)
        }
    }

    /// Explicit history access. Scheduler, admission and summary must use
    /// hot snapshots; only history/detail/search combine permanent cold rows.
    pub fn history_snapshot(&self) -> anyhow::Result<AskState> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::History, diagnostics::Mode::Shared).context("ask history lock")?;
        let (hot, _) = self.read_state_shared()?;
        if super::archive::persisted_cutoff(&self.database_path())?.is_none() {
            return Ok((*hot).clone());
        }
        ask_state_from_rows(&super::archive::history_rows(&self.database_path())?)
    }

    pub fn archived_identities(&self) -> anyhow::Result<Vec<crate::management::v2::hot_store::RowKey>> {
        if !self.archive_configured()? {return Ok(vec![]);}
        super::archive::identities(&self.database_path())
    }

    pub fn archived_by_action(&self,requester:&Requester,action:&str) -> anyhow::Result<Option<AskRecord>> {
        if !self.archive_configured()? {return Ok(None);}
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::History, diagnostics::Mode::Shared).context("archived ask action lookup lock")?;
        super::archive::find_by_action(&self.database_path(),requester,action)
    }

    pub fn archived_asks(&self) -> anyhow::Result<Vec<(AskRecord,String)>> {
        if !self.archive_configured()? {return Ok(vec![]);}
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::History, diagnostics::Mode::Shared).context("archived asks list lock")?;
        super::archive::list_asks(&self.database_path())
    }

    pub fn archived_at(&self,id:&str) -> anyhow::Result<Option<String>> {
        if !self.archive_configured()? {return Ok(None);}
        super::archive::archived_at(&self.database_path(),id)
    }

    /// Projectors need one ask's metadata, never all of its answers or every
    /// archived owner sharing an artifact. Return an immutable owned record.
    pub fn lookup_ask(&self,id:&str) -> anyhow::Result<Option<AskRecord>> {
        std::fs::create_dir_all(&self.dir)?;
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Lookup, diagnostics::Mode::Shared).context("ask record read lock")?;
        let (hot,_)=self.read_state_shared()?;
        if let Some(ask)=hot.asks.get(id) {return Ok(Some(ask.clone()));}
        if !self.archive_configured()? {return Ok(None);}
        super::archive::record(&self.database_path(),"asks",id)
    }

    pub fn all_ask_ids(&self) -> anyhow::Result<std::collections::BTreeSet<String>> {
        if !self.archive_configured()? {return Ok(self.snapshot()?.asks.into_keys().collect());}
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::History, diagnostics::Mode::Shared).context("ask reference inventory lock")?;
        super::archive::all_ask_ids(&self.database_path())
    }

    /// Cold-aware resource lookup. Admission never calls this; normal tools
    /// without historical resource arguments decode only the small hot set.
    pub fn lookup_snapshot(&self,inputs:&[&str]) -> anyhow::Result<AskState> {
        if inputs.is_empty() {return self.snapshot();}
        std::fs::create_dir_all(&self.dir)?;
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Lookup, diagnostics::Mode::Shared).context("ask history lookup lock")?;
        self.lookup_snapshot_locked(inputs)
    }

    /// Read-only tool preflight: wait at most `timeout` for a contended OS lock.
    /// This bounds lock acquisition only; decoding/SQLite are not a deadline.
    pub(crate) fn lookup_snapshot_with_timeout(&self, inputs: &[&str], timeout: std::time::Duration) -> anyhow::Result<Option<AskState>> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        let started = std::time::Instant::now();
        let waiter = diagnostics::Waiter::new(diagnostics::Operation::Preflight, diagnostics::Mode::Shared);
        loop {
            match fs2::FileExt::try_lock_shared(&lock) {
                Ok(()) => {
                    let _lock = waiter.acquired(lock);
                    return self.lookup_snapshot_locked(inputs).map(Some);
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                        let remaining = timeout.saturating_sub(started.elapsed());
                        if remaining.is_zero() { drop(lock); return Ok(None); }
                        std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
                }
                Err(error) => { drop(lock); return Err(error).context("ask preflight lookup lock"); },
            }
        }
    }

    fn lookup_snapshot_locked(&self, inputs: &[&str]) -> anyhow::Result<AskState> {
        let (hot,_)=self.read_state_shared()?;
        if inputs.is_empty() {
            note_whole_state_copy(); // Same whole-state copy as snapshot/read_state.
            return Ok((*hot).clone());
        }
        if !self.archive_configured()? {return Ok((*hot).clone());}
        let mut rows=ask_state_rows(&hot)?;
        let mut related=inputs.to_vec();
        for (id,artifact) in &hot.artifacts {
            if inputs.iter().any(|input|super::archive::identity_matches(input,id)) {
                related.extend(artifact.asks.iter().map(String::as_str));
            }
        }
        for (key,value) in super::archive::matching_rows(&self.database_path(),&related)? {
            anyhow::ensure!(rows.insert(key,value).is_none(),"hot/archive identity conflict");
        }
        ask_state_from_rows(&rows)
    }

    /// First startup and later incremental sweeps share one persisted cutoff.
    /// None means the first activation day's local midnight, not a rolling age.
    pub fn archive_before(&self, requested: Option<chrono::DateTime<Utc>>, now: chrono::DateTime<Utc>) -> anyhow::Result<super::archive::MoveReport> {
        if self.host_queue {
            let runtime=self.dir.parent().context("asks runtime directory missing")?;
            let roster=crate::agent_management::AgentManagementStore::open(runtime.join("agent-management/v1")).map_err(anyhow::Error::new)?;
            let sidecar=crate::director_address::Store::at(runtime.join("director-address"));
            return self.archive_before_with_authority(&roster,&sidecar,requested,now);
        }
        self.archive_before_inner(requested,now,&std::collections::BTreeSet::new())
    }

    /// Standalone service/offline entry. Never use with_role_action here:
    /// its optional maintenance itself takes the asks lock. Raw lock order
    /// is authority mutation fence -> asks exclusive -> SQLite writer.
    pub(crate) fn archive_before_with_authority(&self,roster:&crate::agent_management::AgentManagementStore,
        sidecar:&crate::director_address::Store,requested:Option<chrono::DateTime<Utc>>,now:chrono::DateTime<Utc>) -> anyhow::Result<super::archive::MoveReport> {
        let authority=roster.lock_mutations().map_err(anyhow::Error::new)?;
        self.archive_before_guarded(&authority,sidecar,requested,now)
    }

    /// Guard-aware entry for a caller already holding the non-reentrant
    /// authority fence. Sidecar errors retain every record, never mean empty.
    pub(crate) fn archive_before_guarded(&self,_authority:&std::fs::File,sidecar:&crate::director_address::Store,
        requested:Option<chrono::DateTime<Utc>>,now:chrono::DateTime<Utc>) -> anyhow::Result<super::archive::MoveReport> {
        let ownership=sidecar.snapshot().map_err(|error|anyhow::anyhow!("{error}"))?;
        self.archive_before_inner(requested,now,&super::archive::sidecar_roots(&ownership))
    }

    fn archive_before_inner(&self,requested:Option<chrono::DateTime<Utc>>,now:chrono::DateTime<Utc>,
        sidecar:&std::collections::BTreeSet<String>) -> anyhow::Result<super::archive::MoveReport> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Archive, diagnostics::Mode::Exclusive).context("ask archive lock")?;
        let (initial,initial_generation)=self.read_state_shared()?;
        if self.archive_configured()? {
            let missing:Vec<&str>=sidecar.iter().filter(|id|!initial.asks.contains_key(*id)).map(String::as_str).collect();
            if initial_generation!=0&&!missing.is_empty() {
                super::archive::promote_many(&self.database_path(),initial_generation,&missing)?;
            }
        }
        let (hot,generation)=self.read_state_shared()?;
        let persisted=super::archive::persisted_cutoff(&self.database_path())?;
        let cutoff=match (persisted,requested) {
            (Some(old),Some(new)) => {anyhow::ensure!(old==new,"ask archive cutoff is fixed; explicit stopped reconfiguration required");old},
            (Some(old),None) => old,
            (None,Some(new)) => new,
            (None,None) => super::archive::activation_cutoff(now,None)?,
        };
        if generation==0 {
            super::archive::initialize_empty_policy(&self.database_path(),cutoff)?;
            return Ok(super::archive::MoveReport::default());
        }
        let keys=super::archive::plan_with_protected_roots(&hot,cutoff,sidecar)?;
        super::archive::move_rows(&self.database_path(),generation,&keys,cutoff,now)
    }

    pub fn archive_configured(&self) -> anyhow::Result<bool> {
        Ok(super::archive::persisted_cutoff(&self.database_path())?.is_some())
    }

    pub fn configure_archive_cutoff(&self,cutoff:chrono::DateTime<Utc>,daemon_stopped:bool) -> anyhow::Result<()> {
        anyhow::ensure!(daemon_stopped,"stop every cutex writer before changing the fixed archive cutoff");
        std::fs::create_dir_all(&self.dir)?;
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Archive, diagnostics::Mode::Exclusive).context("ask archive policy lock")?;
        self.read_state_shared()?;
        super::archive::configure_cutoff(&self.database_path(),cutoff)
    }

    /// Restore a historical ask/attachment before a mutation. Called outside
    /// every with_state/inspect closure, never from a nested store lock.
    pub fn promote_history(&self, identity: &str) -> anyhow::Result<()> {
        if super::archive::persisted_cutoff(&self.database_path())?.is_none() {return Ok(());}
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Archive, diagnostics::Mode::Exclusive).context("ask history promotion lock")?;
        let (_,generation)=self.read_state_shared()?;
        if generation==0 {return Ok(());}
        super::archive::promote(&self.database_path(),generation,identity)?;
        Ok(())
    }

    /// The outgoing binary must perform this before replacing it with
    /// r261/r260. The SQLite transaction restores latest data, not a backup.
    pub fn restore_archived_for_rollback(&self, daemon_stopped: bool) -> anyhow::Result<super::archive::MoveReport> {
        anyhow::ensure!(daemon_stopped,"stop every cutex writer before asks archive rollback");
        if super::archive::persisted_cutoff(&self.database_path())?.is_none() {return Ok(Default::default());}
        let lock=self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Archive, diagnostics::Mode::Exclusive).context("ask archive rollback lock")?;
        let (_,generation)=self.read_state_shared()?;
        if generation==0 {return Ok(Default::default());}
        super::archive::restore_all(&self.database_path(),generation,true)
    }

    fn store_generation(&self) -> anyhow::Result<Option<u64>> {
        let database = self.database_path();
        if self.cache_generation {
            crate::management::v2::hot_store::cached_generation(&database, "asks")
        } else {
            crate::management::v2::hot_store::generation(&database, "asks")
        }
    }

    fn artifact_path(&self, artifact_id: &str) -> anyhow::Result<PathBuf> {
        let digest = artifact_id
            .strip_prefix("art-")
            .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
            .context("invalid artifact id")?;
        Ok(self.dir.join("artifacts").join(format!("{digest}.blob")))
    }

    /// Reads the state file. Callers hold the store lock (shared or
    /// exclusive). The parsed state is cached by file identity: every writer
    /// replaces the file atomically under the exclusive lock, so an unchanged
    /// stamp means unchanged content. The management scheduler reads the
    /// store a dozen times per 1 s tick; parsing the 0.7 MB file each time
    /// read ~11 MB/s on the Linux host (2026-10-09).
    fn read_state(&self) -> anyhow::Result<AskState> {
        let (shared, _) = self.read_state_shared()?;
        note_whole_state_copy();
        let _clone = diagnostics::stage(diagnostics::Stage::Clone);
        Ok((*shared).clone())
    }

    fn read_state_shared(&self) -> anyhow::Result<(std::sync::Arc<AskState>, u64)> {
        let database = self.database_path();
        let generation = self.store_generation()?;
        if let Some(generation) = generation {
            let cache = ASK_STATE_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cached_path, cached_generation, state, _)) = cache.as_ref() {
                if cached_path == &database && cached_generation == &generation {
                    return Ok((std::sync::Arc::clone(state), generation));
                }
            }
        }
        let refresh_wait = diagnostics::stage(diagnostics::Stage::RefreshWait);
        let _refresh = ASK_STATE_REFRESH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(refresh_wait);
        // Another reader may have installed this generation while we waited.
        // Probe again rather than reusing an earlier observation (presentation
        // snapshots can refresh without holding a shared file lock).
        if let Some(generation) = self.store_generation()? {
            let cache = ASK_STATE_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cached_path, cached_generation, state, _)) = cache.as_ref() {
                if cached_path == &database && cached_generation == &generation {
                    return Ok((std::sync::Arc::clone(state), generation));
                }
            }
        }
        let sqlite_read = diagnostics::stage(diagnostics::Stage::SqliteSnapshot);
        let loaded = crate::management::v2::hot_store::load_or_migrate(
            &database,
            "asks",
            &self.state_path(),
            |bytes| {
                let state: AskState = serde_json::from_slice(bytes).context("ask store is corrupt")?;
                ask_state_rows(&state)
            },
            |rows| ask_state_from_rows(rows).map(|_| ()),
        )?;
        drop(sqlite_read);
        let decode = diagnostics::stage(diagnostics::Stage::Decode);
        // Take only the companion bytes; readers can still acquire the parsed
        // Arc while we decode/compare outside the publication mutex. The rows
        // and parsed state always belong to the SAME previously committed generation.
        let (before, prior_rows) = {
            let mut cache = ASK_STATE_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match cache.as_mut() {
                Some((path, generation, state, rows)) if path == &database && *generation <= loaded.generation =>
                    (Some(std::sync::Arc::clone(state)), rows.take()),
                _ => (None, None),
            }
        };
        let previous = before.as_deref().zip(prior_rows.as_ref());
        let (state, rows) = if row_reuse::fits(&loaded.rows) {
            (row_reuse::decode(&loaded.rows, previous)?, Some(loaded.rows))
        } else {
            // Oversized stores keep the original consuming decoder and no
            // additional retained bytes (also frees raw rows progressively).
            (ask_state_from_owned_rows(loaded.rows)?, None)
        };
        let state = std::sync::Arc::new(state);
        drop(prior_rows);
        drop(decode);
        if loaded.exists {
            let before = before.as_deref();
            let mut sessions = std::collections::BTreeSet::new();
            for (id, ask) in &state.asks {
                if before.and_then(|before| before.asks.get(id)) != Some(ask) { sessions.insert(ask.target_cutex_session_id.clone()); }
            }
            for (id, delivery) in &state.deliveries {
                if before.and_then(|before| before.deliveries.get(id)) != Some(delivery) {
                    sessions.insert(delivery.requester_cutex_session_id.clone());
                    if let Some(to) = &delivery.routed_to { sessions.insert(to.clone()); }
                }
            }
            if let Some(before) = before {
                for (id, ask) in &before.asks {
                    if !state.asks.contains_key(id) { sessions.insert(ask.target_cutex_session_id.clone()); }
                }
                for (id, delivery) in &before.deliveries {
                    if !state.deliveries.contains_key(id) { sessions.insert(delivery.requester_cutex_session_id.clone()); }
                }
                for session in state.pauses.keys().chain(before.pauses.keys())
                    .chain(state.presence.keys()).chain(before.presence.keys())
                    .chain(state.leases.keys()).chain(before.leases.keys()) {
                    if state.pauses.get(session) != before.pauses.get(session)
                        || state.presence.get(session).and_then(|p| p.agent_pause.as_ref())
                            != before.presence.get(session).and_then(|p| p.agent_pause.as_ref())
                        || state.leases.get(session) != before.leases.get(session) { sessions.insert(session.clone()); }
                }
            }
            let publication = diagnostics::stage(diagnostics::Stage::CachePublish);
            let replaced = {
                let mut cache = ASK_STATE_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                // Presentation reads may overlap a writer. Never replace its
                // newer published generation with this earlier complete snapshot.
                if cache.as_ref().is_some_and(|(path, generation, _, _)| path == &database && *generation > loaded.generation) {
                    None
                } else {
                    cache.replace((database, loaded.generation, std::sync::Arc::clone(&state), rows))
                }
            };
            drop(publication);
            drop(replaced); // potentially large deallocation is outside the mutex
            for session in sessions { crate::management::v2::admission::changed(&session); }
        }
        Ok((state, loaded.generation))
    }

    /// Like [`Self::snapshot`] but never creates anything: `None` when the
    /// store does not exist yet (hot paths outside asks).
    pub fn peek(&self) -> anyhow::Result<Option<AskState>> {
        if !self.state_path().exists()
            && self.store_generation()?.is_none()
        {
            return Ok(None);
        }
        self.snapshot().map(Some)
    }

    /// Read-only snapshot (shared lock).
    pub fn snapshot(&self) -> anyhow::Result<AskState> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreRead);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        timing.stage(Stage::LockWait);
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Snapshot, diagnostics::Mode::Shared).context("ask store lock")?;
        timing.stage(Stage::StoreRead);
        let result = self.read_state();
        timing.status(if result.is_ok() { 200 } else { 500 });
        result
    }

    /// Existing-store inspection while another lifecycle lock is held. Never
    /// waits for an asks writer (which may itself need the lifecycle lock),
    /// and never creates or migrates store files.
    pub fn try_snapshot_existing(&self) -> anyhow::Result<AskState> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join("asks-v1.lock"))?;
        let waiter = diagnostics::Waiter::new(diagnostics::Operation::Preflight, diagnostics::Mode::Shared);
        let acquired = fs2::FileExt::try_lock_shared(&lock);
        if let Err(error) = acquired { drop(lock); return Err(error).context("ask store busy; cannot prove no obligations"); }
        let _lock = waiter.acquired(lock);
        let database = self.database_path();
        if database.exists() {
            if let Some(loaded) = crate::management::v2::hot_store::verified_rows(&database, "asks")? {
                return ask_state_from_rows(&loaded.rows);
            }
        }
        let bytes = std::fs::read(self.state_path())?;
        serde_json::from_slice(&bytes).context("ask store is corrupt")
    }

    /// Read-only shared snapshot (shared lock): the cached parsed state
    /// itself, no copy. Hot read paths (the per-second scheduler pass) use
    /// this; [`Self::snapshot`] copies the whole store (17 MB of JSON on the
    /// Linux host, 2026-10-15) and is for callers that need an owned value.
    pub fn shared(&self) -> anyhow::Result<std::sync::Arc<AskState>> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreRead);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        timing.stage(Stage::LockWait);
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Shared, diagnostics::Mode::Shared).context("ask store lock")?;
        timing.stage(Stage::StoreRead);
        let result = self.read_state_shared().map(|(state, _)| state);
        timing.status(if result.is_ok() { 200 } else { 500 });
        result
    }

    /// Like [`Self::peek`] without the copy: `None` when the store does not
    /// exist yet.
    pub fn peek_shared(&self) -> anyhow::Result<Option<std::sync::Arc<AskState>>> {
        if !self.state_path().exists()
            && self.store_generation()?.is_none()
        {
            return Ok(None);
        }
        self.shared().map(Some)
    }

    /// Read without cloning the complete cached state, retaining the ask shared
    /// lock through the callback for an existing store. Do not reenter the ask
    /// store or acquire another ask-store lock. Derived owner-queue writes are
    /// permitted only in the established ask -> owner lock order. An absent
    /// store stays absent. Use `project_snapshot` only for side-effect-free reads.
    pub fn inspect<T>(&self, read: impl FnOnce(&AskState) -> T) -> anyhow::Result<T> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreRead);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        if !self.state_path().exists()
            && self.store_generation()?.is_none()
        {
            timing.stage(Stage::Dispatch);
            let result = read(&AskState::default());
            timing.status(200);
            return Ok(result);
        }
        let lock = self.lock_file()?;
        timing.stage(Stage::LockWait);
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Inspect, diagnostics::Mode::Shared).context("ask store lock")?;
        timing.stage(Stage::StoreRead);
        let (state, _) = self.read_state_shared()?;
        timing.stage(Stage::Dispatch);
        let _dispatch = diagnostics::stage(diagnostics::Stage::Dispatch);
        let result = read(&state);
        timing.status(200);
        Ok(result)
    }

    /// Pure projection of one committed snapshot; no cross-store serialization.
    /// Unlike `inspect`, verified SQLite reads do not hold the ask OS lock.
    /// The callback must only derive an owned value: no side effects, other
    /// store locks, authority decisions or writes based on this old snapshot.
    /// Subsequent actions must revalidate under their existing write/CAS guard.
    /// Keep `inspect` for owner admission and every derived-write callback.
    pub(crate) fn project_snapshot<T>(&self, read: impl FnOnce(&AskState) -> T) -> anyhow::Result<T> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreRead);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        let generation = self.store_generation()?;
        if generation.is_none() && !self.state_path().exists() {
            timing.stage(Stage::Dispatch);
            let result = read(&AskState::default());
            timing.status(200);
            return Ok(result);
        }
        // Legacy migration retains its existing coordination. Never acquire
        // a writer slot just to read an already verified SQLite generation.
        let _legacy_lock = if generation.is_none() {
            let lock = self.lock_file()?;
            timing.stage(Stage::LockWait);
            let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::LegacyProjection, diagnostics::Mode::Shared).context("ask store projection migration lock")?;
            Some(_lock)
        } else { None };
        timing.stage(Stage::StoreRead);
        let (state, _) = self.read_state_shared()?;
        timing.stage(Stage::Dispatch);
        let result = read(&state);
        timing.status(200);
        Ok(result)
    }

    /// Presentation only: writers replace the state file atomically. A
    /// display may use the previous complete version while a write is in
    /// progress, and must not take a nested store lock during rendering.
    /// Never use this snapshot for authority, mutations or request digests.
    pub(crate) fn display_snapshot(&self) -> anyhow::Result<AskState> {
        self.read_state()
    }

    fn lock_file(&self) -> anyhow::Result<std::fs::File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("asks-v1.lock"))?)
    }

    /// Locked read-modify-write; saves (and bumps `revision`) only when the
    /// state changed.
    pub fn with_state<T>(&self, mutate: impl FnOnce(&mut AskState) -> T) -> anyhow::Result<T> {
        self.with_selection(&[],None,mutate)
    }

    /// Promotion and mutation share ONE file lock, so the incremental mover
    /// cannot cold-move a closed predecessor between lookup and update.
    pub fn with_records<T>(&self, records:&[&str],mutate:impl FnOnce(&mut AskState)->T) -> anyhow::Result<T> {
        self.with_selection(records,None,mutate)
    }

    pub fn with_new_ask_state<T>(&self,new:&NewAsk,mutate:impl FnOnce(&mut AskState)->T) -> anyhow::Result<T> {
        let records:Vec<&str>=new.follow_up_of.iter().chain(new.parent_ask.iter())
            .chain(new.body.attachments.iter()).chain(new.body.text_artifact.iter()).map(String::as_str).collect();
        self.with_selection(&records,Some((&new.requester,&new.action_id)),mutate)
    }

    fn with_selection<T>(&self,records:&[&str],action:Option<(&Requester,&str)>,mutate:impl FnOnce(&mut AskState)->T) -> anyhow::Result<T> {
        use crate::management::v2::control_timing::{Path, Stage, Trace};
        let mut timing = Trace::new(Path::AsksStoreWrite);
        timing.status(500);
        timing.stage(Stage::ContextRead);
        std::fs::create_dir_all(&self.dir)?;
        let mut reclaim = WriteReclaim::default();
        let lock = self.lock_file()?;
        timing.stage(Stage::LockWait);
        let lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Write, diagnostics::Mode::Exclusive).context("ask store lock")?;
        timing.stage(Stage::StoreRead);
        if (!records.is_empty() || action.is_some()) && self.archive_configured()? {
            let mut identities:Vec<String>=records.iter().map(|id|(*id).to_owned()).collect();
            if let Some((requester,key))=action {
                if let Some(ask)=super::archive::find_by_action(&self.database_path(),requester,key)? {identities.push(ask.ask_id);}
            }
            if !identities.is_empty() {
                let (_,generation)=self.read_state_shared()?;
                if generation!=0 {super::archive::promote_many(&self.database_path(),generation,&identities.iter().map(String::as_str).collect::<Vec<_>>())?;}
            }
        }
        let (before, generation) = self.read_state_shared()?;
        reclaim.before = Some(before);
        let before = reclaim.before.as_ref().expect("loaded state");
        note_whole_state_copy();
        reclaim.draft = Some({
            let _clone = diagnostics::stage(diagnostics::Stage::Clone);
            (**before).clone()
        });
        let state = reclaim.draft.as_mut().expect("cloned draft");
        timing.stage(Stage::Dispatch);
        let result = {
            let _dispatch = diagnostics::stage(diagnostics::Stage::Dispatch);
            mutate(state)
        };
        timing.stage(Stage::StoreWrite);
        {
            let _peers = diagnostics::stage(diagnostics::Stage::PeerProjection);
            super::returns::promote_next_step_leases(state);
            crate::peers::asks::queue_snapshots(state, before);
            canonicalize_changed_peer_asks(before, state)?;
        }
        let unchanged = {
            let _equality = diagnostics::stage(diagnostics::Stage::Equality);
            persisted_state_equal(before, state)?
        };
        if !unchanged {
            {
                let _owner = diagnostics::stage(diagnostics::Stage::OwnerSync);
                self.queue_repository()?.sync_asks(before, state)?;
            }
            state.revision += 1;
            let diff = diagnostics::stage(diagnostics::Stage::RowDiff);
            let changes = ask_state_changes(before, state)?;
            let prior_rows = {
                let mut cache = ASK_STATE_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.as_mut().filter(|(path, cached_generation, _, _)|
                    path == &self.database_path() && *cached_generation == generation)
                    .and_then(|(_, _, _, rows)| rows.take())
            };
            // Reuse the exact bytes committed by hot_store. On a failed commit
            // discard the companion; the last published parsed state is untouched.
            let rows = prior_rows.and_then(|rows| row_reuse::changed(rows, &changes));
            drop(diff);
            let commit = diagnostics::stage(diagnostics::Stage::SqliteCommit);
            let next = crate::management::v2::hot_store::commit(
                &self.database_path(),
                "asks",
                generation,
                changes,
            )?;
            drop(commit);
            let _publication = diagnostics::stage(diagnostics::Stage::CachePublish);
            reclaim.replaced = ASK_STATE_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner).replace((
                self.database_path(),
                next,
                std::sync::Arc::new(reclaim.draft.take().expect("committed draft")),
                rows,
            ));
        }
        // Owner sync -> SQLite commit -> cache publication remain serialized.
        // Only destruction of the old generation / unchanged draft moves out.
        let released = lock.release();
        drop(reclaim);
        drop(released); // slow diagnostics include reclamation, after OS unlock
        timing.status(200);
        Ok(result)
    }

    /// Offline rollback export for a release that still reads asks-v1.json.
    /// The caller must stop every cutex process sharing this home first.
    pub(crate) fn verify_legacy_export(&self) -> anyhow::Result<bool> {
        super::archive::require_legacy_compatible(&self.database_path())?;
        let Some(loaded) = crate::management::v2::hot_store::verified_rows(
            &self.database_path(),
            "asks",
        )? else {
            return Ok(false);
        };
        ask_state_from_rows(&loaded.rows)?;
        Ok(true)
    }

    /// Offline rollback export for a release that still reads asks-v1.json.
    /// The caller must stop every cutex process sharing this home first.
    pub fn export_legacy_json(&self) -> anyhow::Result<bool> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_file()?;
        let _lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Export, diagnostics::Mode::Exclusive).context("ask store rollback lock")?;
        super::archive::require_legacy_compatible(&self.database_path())?;
        let Some(loaded) = crate::management::v2::hot_store::verified_rows(&self.database_path(), "asks")? else {
            return Ok(false);
        };
        let state = ask_state_from_rows(&loaded.rows)?;
        let path = self.state_path();
        let already_equal = if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(existing) = serde_json::from_slice::<AskState>(&bytes) {
                persisted_state_equal(&state, &existing)?
            } else {
                false
            }
        } else {
            false
        };
        if !already_equal {
            crate::config::atomic::write_private_pretty_json_atomic(&path, &state, "asks rollback export")?;
        }
        let checked: AskState = serde_json::from_slice(&std::fs::read(&path)?)?;
        anyhow::ensure!(persisted_state_equal(&state, &checked)?, "ask rollback export verification failed");
        crate::management::v2::hot_store::handoff_to_legacy(
            &self.database_path(),
            "asks",
            loaded.generation,
        )?;
        Ok(true)
    }

    /// Store bytes in the artifact store (content-addressed; idempotent) and
    /// record who may read them. Returns the artifact id.
    pub fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: &str,
        readers: &[String],
        ask_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        let artifact_id = format!("art-{}", super::sha256_hex(bytes));
        let path = self.artifact_path(&artifact_id)?;
        if !path.exists() {
            std::fs::create_dir_all(path.parent().context("artifact dir")?)?;
            crate::config::atomic::write_private_bytes_atomic(&path, bytes)?;
        }
        self.with_records(&[&artifact_id],|state| {
            let record = state
                .artifacts
                .entry(artifact_id.clone())
                .or_insert_with(|| ArtifactRecord {
                    artifact_id: artifact_id.clone(),
                    bytes: bytes.len(),
                    media_type: media_type.into(),
                    readers: Vec::new(),
                    asks: Vec::new(),
                    created_at: now_rfc3339(now),
                });
            for reader in readers {
                if !record.readers.contains(reader) {
                    record.readers.push(reader.clone());
                }
            }
            if let Some(ask_id) = ask_id {
                if !record.asks.iter().any(|ask| ask == ask_id) {
                    record.asks.push(ask_id.to_string());
                }
            }
        })?;
        Ok(artifact_id)
    }

    /// Artifact bytes for `reader` (`None` = the owner). Unknown and
    /// unreadable artifacts look the same (`None`).
    pub fn read_artifact(&self, artifact_id: &str, reader: Option<&str>) -> anyhow::Result<Option<(ArtifactRecord, Vec<u8>)>> {
        std::fs::create_dir_all(&self.dir)?;
        let lock=self.lock_file()?;
        let lock = diagnostics::LockGuard::acquire(lock, diagnostics::Operation::Artifact, diagnostics::Mode::Shared).context("artifact metadata read lock")?;
        let (state,_)=self.read_state_shared()?;
        let cold=self.archive_configured()?;
        let record=match state.artifacts.get(artifact_id) {
            Some(record)=>Some(record.clone()),
            None if cold=>super::archive::record::<ArtifactRecord>(&self.database_path(),"artifacts",artifact_id)?,
            None=>None,
        };
        let Some(record) = record else {
            return Ok(None);
        };
        let needs_ask_acl=reader.is_some_and(|reader|!record.readers.iter().any(|allowed|allowed==reader));
        let mut related=Vec::new();
        if needs_ask_acl {
            related.extend(record.asks.iter().filter_map(|id|state.asks.get(id).cloned()));
            if cold {
                let missing:Vec<&str>=record.asks.iter().filter(|id|!state.asks.contains_key(*id)).map(String::as_str).collect();
                related.extend(super::archive::records::<AskRecord>(&self.database_path(),"asks",&missing)?);
            }
        }
        // Current controller resolution can acquire the authority guard;
        // never invert role-action -> asks by holding this file lock there.
        drop(lock);
        if let Some(reader)=reader {
            if needs_ask_acl&&!related.iter().any(|ask|(ask.remote.is_none()&&ask.target_cutex_session_id==reader)||ask.readable_by(reader)) {return Ok(None);}
        }
        match std::fs::read(self.artifact_path(artifact_id)?) {
            Ok(bytes) => Ok(Some((record, bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Prune records and delete orphaned artifact blobs.
    pub fn prune(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        self.prune_with(Some(CLOSED_RETENTION_DAYS), now)
    }

    /// [`AskState::prune_with`] and delete orphaned artifact blobs.
    pub fn prune_with(&self, closed_retention_days: Option<i64>, now: DateTime<Utc>) -> anyhow::Result<()> {
        if super::archive::persisted_cutoff(&self.database_path())?.is_some() {
            // Archive owns all user records and blobs once enabled. The old
            // retention implementation deletes metadata/orphans and cannot
            // see cold references. Never send it an incomplete hot state.
            self.archive_before(None,now)?;
            return Ok(());
        }
        let orphaned = self.with_state(|state| state.prune_with(closed_retention_days, now))?;
        for artifact_id in orphaned {
            if let Ok(path) = self.artifact_path(&artifact_id) {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn new_ask_lineage_notice_is_minimal_routed_and_not_repeated_on_replay() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, created) = state.create_ask(new_ask("lineage-notice", "synthetic"), now).unwrap();
        assert!(created);
        let notices: Vec<_> = state.notices.iter().filter(|notice| notice.method == "cutex/delegation/lineageChanged").collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].params, json!({"projectId":ask.project_id,"resourceId":ask.ask_id,"revision":state.lineage.revision}));
        assert_eq!(notices[0].sessions.iter().collect::<std::collections::BTreeSet<_>>(),
            AskState::ask_sessions(&ask).iter().collect::<std::collections::BTreeSet<_>>());
        assert_eq!(notices[0].params.as_object().unwrap().len(), 3);
        let revision = state.lineage.revision;
        let (replayed, created) = state.create_ask(new_ask("lineage-notice", "synthetic"), now).unwrap();
        assert!(!created);
        assert_eq!(replayed.ask_id, ask.ask_id);
        assert_eq!(state.lineage.revision, revision);
        assert_eq!(state.notices.iter().filter(|notice| notice.method == "cutex/delegation/lineageChanged").count(), 1);
    }

    pub const REQUESTER: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
    pub const TARGET: &str = "cutex.00000000-0000-0000-0000-0000000000b1";

    pub fn temp_store(label: &str) -> (AskStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-asks-{label}-{}", uuid::Uuid::new_v4().simple()));
        (AskStore::at(&dir), dir)
    }

    pub fn body(text: &str) -> AskBody {
        AskBody {
            text: text.into(),
            text_artifact: None,
            text_bytes: text.len(),
            text_sha256: crate::asks::sha256_hex(text.as_bytes()),
            attachments: Vec::new(),
        }
    }

    pub fn new_ask(action_id: &str, text: &str) -> NewAsk {
        NewAsk {
            ask_id: None,
            action_id: action_id.into(),
            requester: Requester::Agent { cutex_session_id: REQUESTER.into(), name: Some("Eva".into()) },
            target_cutex_session_id: TARGET.into(),
            target_name: Some("Kimi".into()),
            project_id: Some("alpha".into()),
            body: body(text),
            title: None,
            options: AskOptions::default(),
            follow_up_of: None,
            parent_ask: None,
            gate: None,
        }
    }

    fn mixed_peer_state() -> AskState {
        use crate::peers::model as peer;
        let mut state = AskState::default();
        let (mut ask, _) = state.create_ask(new_ask("peer-equality", "hello"), Utc::now()).unwrap();
        let request = peer::Create {
            requester: peer::Requester { cutex_session_id: REQUESTER.into(), name: Some("Worker Name".into()), caps: Default::default() },
            target: TARGET.into(), title: "hello".into(), text: "hello".into(),
            options: ask.options.clone(), hops: 0, request_sha256: String::new(),
        };
        ask.ask_id = "peer-outbound".into();
        ask.remote = Some(peer::Remote {
            peer_id: "peer-fixture".into(), label: "Fixture".into(), target_name: "worker".into(),
            request: request.clone(), remote_state: None, late_answer: None,
            sync: peer::Sync {
                local_terminal: false, reconcile_at: None, interrupt_reason: None,
                state: "queued".into(), last_remote_revision: 0, pending_ops: vec![],
                last_error: Some("temporary failure".into()), next_attempt_at: None,
                last_received_at: now_rfc3339(Utc::now()), failures: 0, accepted: false,
            },
        });
        state.asks.insert(ask.ask_id.clone(), ask.clone());
        ask.ask_id = "peer-inbound".into();
        ask.remote = None;
        ask.peer_inbound = Some(peer::Inbound {
            dead_letters: vec![], queued_at: None, result_undeliverable: false,
            delivery_stopped: false, last_error: Some("temporary failure".into()),
            request, peer_deliveries: vec![peer::Event {
                event_id: "peer-event".into(), ask_id: ask.ask_id.clone(), revision: 1,
                snapshot: peer::Snapshot {
                    ask_id: ask.ask_id.clone(), revision: 1, execution: ExecutionState::Queued,
                    hold: None, closed: None, review: None, answer: None, text: None,
                    interrupt_reason: None, last_error: Some("temporary failure".into()), remote_state: None,
                },
            }], received_ops: vec![], next_attempt_at: None, failures: 0,
        });
        state.asks.insert(ask.ask_id.clone(), ask);
        state.notices.clear();
        state
    }

    fn assert_persisted_comparison(before: &AskState, after: &AskState, expected: bool) {
        // Preserve the previous mutation guard, including structurally equal
        // values, and independently compare its whole-document result.
        let legacy_equal = before == after || serde_json::to_vec(before).unwrap() == serde_json::to_vec(after).unwrap();
        assert_eq!(legacy_equal, expected);
        assert_eq!(persisted_state_equal(before, after).unwrap(), expected);
        assert_eq!(persisted_state_equal(after, before).unwrap(), expected);
    }

    #[test]
    fn persisted_equality_preserves_peer_normalization_and_real_changes() {
        let before = mixed_peer_state();
        assert_persisted_comparison(&before, &before, true);
        let mut normalized = before.clone();
        let remote = normalized.asks.get_mut("peer-outbound").unwrap().remote.as_mut().unwrap();
        remote.sync.last_error = Some("  temporary\n failure\t".into());
        remote.request.requester.name = Some(" Worker\nName ".into());
        let inbound = normalized.asks.get_mut("peer-inbound").unwrap().peer_inbound.as_mut().unwrap();
        inbound.last_error = Some("temporary\r\n failure".into());
        inbound.peer_deliveries[0].snapshot.last_error = Some("temporary\t failure".into());
        assert_persisted_comparison(&before, &normalized, true);
        let mut real_peer_change = normalized.clone();
        real_peer_change.asks.get_mut("peer-outbound").unwrap().remote.as_mut().unwrap().sync.accepted = true;
        assert_persisted_comparison(&before, &real_peer_change, false);
        let mut local_change = normalized.clone();
        local_change.asks.values_mut().find(|ask| ask.remote.is_none() && ask.peer_inbound.is_none()).unwrap().title.push('!');
        assert_persisted_comparison(&before, &local_change, false);
        let mut non_ask_change = normalized;
        non_ask_change.presence.insert(TARGET.into(), Presence { online: true, ..Default::default() });
        assert_persisted_comparison(&before, &non_ask_change, false);
        let mut removed_peer = before.clone();
        removed_peer.asks.get_mut("peer-outbound").unwrap().remote = None;
        assert_persisted_comparison(&before, &removed_peer, false);
        let mut replaced_key = before.clone();
        let ask = replaced_key.asks.remove("peer-outbound").unwrap();
        replaced_key.asks.insert("replacement".into(), ask);
        assert_persisted_comparison(&before, &replaced_key, false);
    }

    #[test]
    fn persisted_equality_write_preserves_peer_noop_bytes_and_persists_local_changes() {
        let (store, dir) = temp_store("mixed-peer-equality");
        store.with_state(|state| *state = mixed_peer_state()).unwrap();
        let database = store.database_path();
        let generation = crate::management::v2::hot_store::generation(&database, "asks").unwrap();
        let revision = store.snapshot().unwrap().revision;
        store.with_state(|state| {
            state.asks.get_mut("peer-outbound").unwrap().remote.as_mut().unwrap().sync.last_error = Some(" temporary\n failure ".into());
        }).unwrap();
        assert_eq!(crate::management::v2::hot_store::generation(&database, "asks").unwrap(), generation);
        store.with_state(|state| { state.wait_cancel.insert(TARGET.into(), 1); }).unwrap();
        let after = store.snapshot().unwrap();
        assert_eq!(after.revision, revision + 1);
        assert_eq!(after.wait_cancel[TARGET], 1);
        assert_eq!(after.asks["peer-outbound"].remote.as_ref().unwrap().sync.last_error.as_deref(), Some("temporary failure"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "private synthetic 14 MB mixed-peer comparison benchmark"]
    fn benchmark_persisted_equality_mixed_peer_history() {
        let mut before = mixed_peer_state();
        let payload = "synthetic history ".repeat(850);
        for i in 0..1000 {
            before.actions.insert(format!("action-{i}"), ActionRecord {
                tool: "fixture".into(), request_sha256: "digest".into(),
                result: Some(json!({"history": payload})), at: "2026-10-13T00:00:00Z".into(),
            });
        }
        let bytes = serde_json::to_vec(&before).unwrap().len();
        assert!(bytes >= 14_000_000);
        let mut real_change = before.clone();
        real_change.asks.values_mut().find(|ask| ask.remote.is_none() && ask.peer_inbound.is_none()).unwrap().title.push('!');
        let mut normalized_noop = before.clone();
        normalized_noop.asks.get_mut("peer-outbound").unwrap().remote.as_mut().unwrap().sync.last_error = Some(" temporary\n failure ".into());
        for (label, after) in [("real-local-change", &real_change), ("peer-normalization-noop", &normalized_noop), ("unchanged", &before)] {
            let rounds = 20;
            let start = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(before != *after && serde_json::to_vec(&before).unwrap() != serde_json::to_vec(after).unwrap());
            }
            let legacy = start.elapsed();
            let start = std::time::Instant::now();
            for _ in 0..rounds { std::hint::black_box(!persisted_state_equal(&before, after).unwrap()); }
            println!("mixed-peer persisted comparison: bytes={bytes} case={label} rounds={rounds} legacy_ms={:.3} record_ms={:.3}", legacy.as_secs_f64() * 1000.0, start.elapsed().as_secs_f64() * 1000.0);
        }
    }

    #[test]
    fn create_is_idempotent_on_action_id_and_bounded_per_target() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (first, created) = state.create_ask(new_ask("a1", "Review the parser"), now).unwrap();
        assert!(created);
        assert_eq!(first.execution, ExecutionState::Queued);
        assert_eq!(first.title, "Review the parser");
        let (again, created) = state.create_ask(new_ask("a1", "Review the parser"), now).unwrap();
        assert!(!created);
        assert_eq!(again.ask_id, first.ask_id);
        let conflict = state.create_ask(new_ask("a1", "Something else"), now).unwrap_err();
        assert_eq!(conflict.code, "idempotency_conflict");
        for index in 1..crate::asks::MAX_QUEUED_PER_TARGET {
            state.create_ask(new_ask(&format!("b{index}"), "x"), now).unwrap();
        }
        let overloaded = state.create_ask(new_ask("overflow", "x"), now).unwrap_err();
        assert_eq!(overloaded.code, "target_overloaded");
        assert_eq!(overloaded.to_json()["status"], "no_write");
        let second = state.find_by_action(&new_ask("b1", "x").requester, "b1").unwrap().clone();
        assert_eq!(queue_position(&state, &second), Some(2));
        assert_eq!(state.notices.iter().filter(|notice| notice.method == "cutex/ask/created").count(), 50);
    }

    #[test]
    fn gated_agent_asks_wait_for_review_and_owner_asks_never_do() {
        let mut state = AskState::default();
        let now = Utc::now();
        let mut gated = new_ask("g", "do it");
        gated.gate = Some(GateRecord {
            required: true,
            differences: vec![],
            snapshot_sha256: "s".into(),
            inputs: Value::Null,
            approval_id: None,
            admitted_with_approval: false,
            stale_reason: None,
        });
        assert_eq!(state.create_ask(gated.clone(), now).unwrap().0.execution, ExecutionState::PendingReview);
        let mut owner = gated;
        owner.action_id = "o".into();
        owner.requester = Requester::Owner;
        assert_eq!(state.create_ask(owner, now).unwrap().0.execution, ExecutionState::Queued);
    }

    #[test]
    fn follow_up_requests_changes_on_a_review_ask() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("r", "draft"), now).unwrap();
        state.update_ask(&ask.ask_id, now, |ask| {
            ask.execution = ExecutionState::Answered;
            ask.review = Some(ReviewState::AwaitingAcceptance);
        });
        let mut follow = new_ask("f", "again, with tests");
        follow.follow_up_of = Some(ask.ask_id.clone());
        state.create_ask(follow, now).unwrap();
        assert_eq!(state.asks[&ask.ask_id].review, Some(ReviewState::ChangesRequested));
        let mut stray = new_ask("s", "x");
        stray.follow_up_of = Some("ask-missing".into());
        assert_eq!(state.create_ask(stray, now).unwrap_err().code, "ask_not_found");
    }

    #[test]
    fn actions_replay_conflict_and_release() {
        let mut state = AskState::default();
        let now = Utc::now();
        assert_eq!(state.begin_action("me", "x", "agent_tell", "d1", now), ActionLookup::New);
        assert_eq!(state.begin_action("me", "x", "agent_tell", "d1", now), ActionLookup::InFlight);
        assert_eq!(state.begin_action("me", "x", "agent_tell", "d2", now), ActionLookup::Conflict);
        state.finish_action("me", "x", &json!({"ok": true}));
        assert_eq!(state.begin_action("me", "x", "agent_tell", "d1", now), ActionLookup::Replay(json!({"ok": true})));
        assert_eq!(state.begin_action("other", "x", "agent_tell", "d2", now), ActionLookup::New);
        state.release_action("other", "x");
        assert_eq!(state.begin_action("other", "x", "agent_tell", "d3", now), ActionLookup::New);
    }

    #[test]
    fn update_emits_state_and_close_notices_and_prune_drops_old_closed_asks() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("p", "x"), now).unwrap();
        state.notices.clear();
        state.update_ask(&ask.ask_id, now, |ask| ask.activity_noop());
        assert!(state.notices.is_empty());
        state.update_ask(&ask.ask_id, now, |ask| {
            ask.execution = ExecutionState::Cancelled;
            ask.closed = Some(CloseOutcome::Cancelled);
            ask.closed_at = Some(now_rfc3339(now - chrono::Duration::days(31)));
        });
        let methods: Vec<&str> = state.notices.iter().map(|notice| notice.method.as_str()).collect();
        assert_eq!(methods, ["cutex/ask/stateChanged", "cutex/ask/closed"]);
        assert_eq!(state.asks[&ask.ask_id].revision, 2);
        state.prune(now);
        assert!(state.asks.is_empty());
    }

    impl AskRecord {
        fn activity_noop(&mut self) {}
    }

    #[test]
    fn artifacts_are_content_addressed_and_access_controlled() {
        let (store, dir) = temp_store("artifacts");
        let now = Utc::now();
        let id = store.put_artifact(b"long brief", "text/plain", &[REQUESTER.into()], None, now).unwrap();
        assert_eq!(id, store.put_artifact(b"long brief", "text/plain", &[], None, now).unwrap());
        assert_eq!(store.read_artifact(&id, Some(REQUESTER)).unwrap().unwrap().1, b"long brief");
        assert!(store.read_artifact(&id, Some(TARGET)).unwrap().is_none());
        assert!(store.read_artifact(&id, None).unwrap().is_some());
        // Attached to an ask, the target may read it.
        let mut ask = new_ask("att", "see attachment");
        ask.body.attachments = vec![id.clone()];
        store.with_state(|state| state.create_ask(ask, now).unwrap()).unwrap();
        assert!(store.read_artifact(&id, Some(TARGET)).unwrap().is_some());
        assert!(store.read_artifact("art-zz", None).unwrap().is_none());
        let revision = store.snapshot().unwrap().revision;
        assert!(revision >= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_reads_follow_writes_from_this_and_other_writers() {
        let (store, dir) = temp_store("read-cache");
        let now = Utc::now();
        store.with_state(|state| state.create_ask(new_ask("one", "first"), now).unwrap()).unwrap();
        let first = store.snapshot().unwrap();
        assert_eq!(first.asks.len(), 1);
        assert_eq!(store.snapshot().unwrap(), first, "unchanged file reads the same state");
        // A no-op mutation does not save (revision unchanged).
        store.with_state(|_| ()).unwrap();
        assert_eq!(store.snapshot().unwrap().revision, first.revision);
        // Another store handle / process advances the SQLite generation.
        let other = AskStore::at(&dir);
        other.with_state(|state| state.create_ask(new_ask("two", "second"), now).unwrap()).unwrap();
        let second = store.snapshot().unwrap();
        assert_eq!(second.asks.len(), 2);
        assert_eq!(second.revision, first.revision + 1);
        let third_writer = AskStore::at(&dir);
        third_writer.with_state(|state| { state.wait_cancel.insert("external".into(), 10); }).unwrap();
        assert_eq!(store.snapshot().unwrap().revision, second.revision + 1);
        assert_eq!(store.snapshot().unwrap().wait_cancel["external"], 10);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_json_migrates_once_and_offline_export_contains_latest_state() {
        let (store, dir) = temp_store("legacy-migrate-export");
        std::fs::create_dir_all(&dir).unwrap();
        let mut legacy = AskState::default();
        legacy.create_ask(new_ask("legacy", "before migration"), Utc::now()).unwrap();
        legacy.notices.clear();
        let backup = serde_json::to_vec_pretty(&legacy).unwrap();
        std::fs::write(dir.join("asks-v1.json"), &backup).unwrap();

        let loaded = store.snapshot().unwrap();
        assert_eq!(loaded.asks.len(), 1);
        assert_eq!(std::fs::read(dir.join("asks-v1.json")).unwrap(), backup);
        store.with_state(|state| { state.wait_cancel.insert(TARGET.into(), 7); }).unwrap();
        // Hot writes do not rewrite the rollback backup.
        assert_eq!(std::fs::read(dir.join("asks-v1.json")).unwrap(), backup);
        store.export_legacy_json().unwrap();
        assert_eq!(
            crate::management::v2::hot_store::generation(&store.database_path(), "asks").unwrap(),
            None,
            "rollback handoff makes the old JSON authoritative",
        );
        let mut exported: AskState = serde_json::from_slice(&std::fs::read(dir.join("asks-v1.json")).unwrap()).unwrap();
        assert_eq!(exported.wait_cancel[TARGET], 7);
        exported.wait_cancel.insert(TARGET.into(), 9);
        crate::config::atomic::write_private_pretty_json_atomic(
            &dir.join("asks-v1.json"),
            &exported,
            "simulated r256 ask write",
        )
        .unwrap();
        assert_eq!(AskStore::at(&dir).snapshot().unwrap().wait_cancel[TARGET], 9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_legacy_json_fails_closed_without_verified_sqlite_store() {
        let (store, dir) = temp_store("legacy-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("asks-v1.json"), b"{\"asks\":").unwrap();
        assert!(store.snapshot().unwrap_err().to_string().contains("ask store is corrupt"));
        assert_eq!(
            crate::management::v2::hot_store::generation(&store.database_path(), "asks").unwrap(),
            None,
        );
        assert_eq!(std::fs::read(dir.join("asks-v1.json")).unwrap(), b"{\"asks\":");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Synthetic large-store CPU/lock-work comparison. Run explicitly; no
    /// model, live daemon or owner data. Timings are observations, not CI gates.
    #[test]
    #[ignore]
    fn benchmark_large_store_send_probes() {
        eprintln!("items,json_bytes,legacy_agent_list_us,sqlite_agent_list_us,legacy_mutation_ms,sqlite_mutation_ms");
        for items in [1_000, 10_000] {
            let (mut store, dir) = temp_store(&format!("large-send-probes-{items}"));
            store.cache_generation = true;
            let mut baseline = AskState::default();
            for index in 0..items {
                baseline.actions.insert(format!("synthetic-{index}"), ActionRecord {
                    tool: "synthetic".into(),
                    request_sha256: "fixture".into(),
                    result: Some(json!({"text": "x".repeat(4_000)})),
                    at: "fixture".into(),
                });
            }
            store.with_state(|state| *state = baseline.clone()).unwrap();
            let legacy = dir.join("legacy-benchmark.json");
            let initial = serde_json::to_vec_pretty(&baseline).unwrap();
            std::fs::write(&legacy, &initial).unwrap();

            let started = std::time::Instant::now();
            for _ in 0..100 {
                assert!(crate::platform::file_stamp::FileStamp::of(&legacy).is_some());
            }
            let legacy_list_us = started.elapsed().as_micros();
            let started = std::time::Instant::now();
            for _ in 0..100 {
                assert!(!store.inspect(|state| state.pauses.contains_key("probe")).unwrap());
            }
            let sqlite_list_us = started.elapsed().as_micros();

            let mut legacy_state = baseline;
            let started = std::time::Instant::now();
            for index in 0..3 {
                legacy_state.actions.get_mut("synthetic-0").unwrap().at = format!("legacy-{index}");
                crate::config::atomic::write_private_pretty_json_atomic(
                    &legacy,
                    &legacy_state,
                    "legacy whole-store benchmark",
                )
                .unwrap();
            }
            let legacy_mutation_ms = started.elapsed().as_millis();
            let started = std::time::Instant::now();
            for index in 0..3 {
                store.with_state(|state| {
                    state.actions.get_mut("synthetic-0").unwrap().at = format!("sqlite-{index}");
                }).unwrap();
            }
            let sqlite_mutation_ms = started.elapsed().as_millis();
            eprintln!(
                "{items},{},{legacy_list_us},{sqlite_list_us},{legacy_mutation_ms},{sqlite_mutation_ms}",
                initial.len()
            );
            crate::management::v2::hot_store::clear_cached_generation();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

}
