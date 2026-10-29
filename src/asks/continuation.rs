//! Bounded continuation transitions, embedded in the existing AskRecord.
//!
//! Call mutations under AskStore's exclusive AskState lock. The caller supplies
//! authenticated lease/authority/admission facts, persists this record and the
//! existing outbox in that transaction, then performs runtime IO outside it.
//! This module owns no store, thread, polling loop, or runtime connection.
//! The parent integration handoff documents shared-file wiring.

use chrono::{DateTime, Duration, Utc};
mod delegation_children;
use serde::{Deserialize, Serialize};

#[path = "continuation/runtime.rs"]
pub mod runtime;

pub const MAX_ROUNDS: u32 = 32;
pub const MAX_ELAPSED_SECONDS: i64 = 86_400;
pub const MIN_WAKE_SECONDS: u32 = 60;
pub const MAX_CHILDREN: usize = 64;
pub const MAX_NOTE_CHARS: usize = 16_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptIdentity {
    pub delegation_id: String,
    pub attempt_id: String,
    pub owner_session_id: String,
    pub runtime_generation: u64,
    pub parent_turn_id: String,
}

impl AttemptIdentity {
    fn valid(&self) -> bool {
        [
            &self.delegation_id,
            &self.attempt_id,
            &self.owner_session_id,
            &self.parent_turn_id,
        ]
        .iter()
        .all(|s| !s.trim().is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Actor {
    Owner,
    Agent { session_id: String },
}

/// Values come from the server's authenticated project/lease snapshot, never
/// request JSON. Rebuild this snapshot under the same authority/write fence.
pub struct Authority<'a> {
    pub actor: &'a Actor,
    pub current_director: Option<&'a str>,
}

impl Authority<'_> {
    fn may_reset(&self) -> bool {
        matches!(self.actor, Actor::Owner)
            || matches!(self.actor, Actor::Agent { session_id } if Some(session_id.as_str()) == self.current_director)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildKind {
    NativeAgent,
    Delegation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildRef {
    pub kind: ChildKind,
    pub id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Until {
    All,
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum WakeCondition {
    After {
        #[serde(rename = "delaySeconds")]
        delay_seconds: u32,
    },
    Children {
        children: Vec<ChildRef>,
        until: Until,
    },
}

/// Default false. A provider is supported only when the bridge can authenticate
/// both spawn/lineage and all terminal outcomes (including failure/cancellation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConditionSupport {
    pub native_children: bool,
    pub delegation_children: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalFact {
    pub source_event_id: String,
    pub outcome: ChildOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ChildOutcome {
    Completed,
    Failed { reason: String },
    Cancelled { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildFact {
    pub parent: AttemptIdentity,
    pub child: ChildRef,
    pub spawn_event_id: String,
    pub terminal: Option<TerminalFact>,
}

/// Not deserializable: only the runtime/lineage bridge may construct evidence.
/// Its caller must verify the bound transport's session + generation and the
/// actual provider spawn event or daemon-owned parent-attempt lineage record.
pub struct AuthenticatedSpawn {
    parent: AttemptIdentity,
    child: ChildRef,
    source_event_id: String,
}

impl AuthenticatedSpawn {
    pub(crate) fn from_verified_bridge(
        parent: AttemptIdentity,
        child: ChildRef,
        source_event_id: String,
    ) -> Self {
        Self {
            parent,
            child,
            source_event_id,
        }
    }
}

/// A completion references the *original* parent attempt/generation, even when
/// it arrives after that turn ended. Text or arbitrary agent IDs never qualify.
pub struct AuthenticatedTerminal {
    parent: AttemptIdentity,
    child: ChildRef,
    terminal: TerminalFact,
}

impl AuthenticatedTerminal {
    pub(crate) fn from_verified_bridge(
        parent: AttemptIdentity,
        child: ChildRef,
        terminal: TerminalFact,
    ) -> Self {
        Self {
            parent,
            child,
            terminal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Declared,
    Waiting,
    Ready,
    Resuming,
    Limited,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LimitReason {
    Rounds,
    Elapsed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitNotice {
    pub id: String,
    pub budget_epoch: u64,
    pub reason: LimitReason,
    pub owner: bool,
    pub director_session_id: Option<String>,
    pub enqueued: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Declaration {
    pub source: AttemptIdentity,
    pub note: String,
    pub wake: Option<WakeCondition>,
    pub revision: u64,
    pub yielded_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Submission {
    Unclaimed,
    /// Persist before IO. After a crash even this state is uncertain; never
    /// repeat submit automatically. Query the original native message ID.
    Reserved {
        attempt_id: String,
        runtime_generation: u64,
        native_message_id: String,
    },
    Unknown {
        attempt_id: String,
        runtime_generation: u64,
        native_message_id: String,
    },
    Accepted {
        attempt: AttemptIdentity,
        native_message_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wake {
    pub id: String,
    /// Reset approval starts a fresh epoch whose first admitted turn is round 1.
    pub increments_round: bool,
    pub submission: Submission,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationRecord {
    pub state: Phase,
    pub revision: u64,
    pub budget_epoch: u64,
    pub round_number: u32,
    pub started_at: DateTime<Utc>,
    pub deadline_at: DateTime<Utc>,
    pub declaration: Declaration,
    pub running_attempt: Option<AttemptIdentity>,
    pub next_wake_at: Option<DateTime<Utc>>,
    pub wake: Option<Wake>,
    pub limit_reason: Option<LimitReason>,
    pub limit_notice: Option<LimitNotice>,
    pub cancel_reason: Option<String>,
    pub reconciling: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageNote {
    pub source: AttemptIdentity,
    pub budget_epoch: u64,
    pub round_number: u32,
    pub declaration_note: String,
    /// Existing AskStore artifact, kept whole by the observer before transition.
    pub final_message_artifact: Option<String>,
    pub ended_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclareRequest {
    pub action_id: String,
    pub expected_attempt_id: String,
    pub expected_revision: Option<u64>,
    pub note: String,
    pub wake: Option<WakeCondition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResetRequest {
    pub action_id: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum ActionRequest {
    Declare { request: DeclareRequest },
    Reset { request: ResetRequest },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationReceipt {
    pub continuation: ContinuationRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionEntry {
    actor: Actor,
    id: String,
    request: ActionRequest,
    receipt: MutationReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetResetAudit {
    pub actor: Actor,
    pub action_id: String,
    pub previous_epoch: u64,
    pub budget_epoch: u64,
    pub at: DateTime<Utc>,
    pub previous_limit_notice: Option<LimitNotice>,
}

/// Add as a serde-defaulted field of AskRecord. Empty old records preserve the
/// old final-answer path. No separate service-level collection is introduced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationData {
    pub current: Option<ContinuationRecord>,
    pub stages: Vec<StageNote>,
    pub child_facts: Vec<ChildFact>,
    pub budget_resets: Vec<BudgetResetAudit>,
    actions: Vec<ActionEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidArguments,
    NotCurrentHandler,
    Conflict,
    RevisionConflict,
    ActionPayloadConflict,
    ConditionUnsupported,
    UnverifiedChild,
    EvidenceConflict,
    Forbidden,
    NotLimited,
    Guarded,
    StaleWake,
    Reconciling,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::NotCurrentHandler => "continuation_not_current_handler",
            Self::Conflict => "continuation_conflict",
            Self::RevisionConflict => "continuation_revision_conflict",
            Self::ActionPayloadConflict => "action_id_payload_conflict",
            Self::ConditionUnsupported => "continuation_condition_unsupported",
            Self::UnverifiedChild => "continuation_child_unverified",
            Self::EvidenceConflict => "continuation_evidence_conflict",
            Self::Forbidden => "forbidden",
            Self::NotLimited => "continuation_not_limited",
            Self::Guarded => "continuation_guarded",
            Self::StaleWake => "continuation_stale_wake",
            Self::Reconciling => "continuation_reconciling",
        }
    }
}

/// Live admission facts, rebuilt under the parent's existing fencing protocol.
/// Default fails closed. paused includes individual and project pause reasons;
/// stopped includes ancestor stop fences. online=false never starts a runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct Guards {
    pub stopped: bool,
    pub archived: bool,
    pub paused: bool,
    pub online: bool,
    pub permission_valid: bool,
    pub occurrence_valid: bool,
    pub running_turn: bool,
    pub submission_unknown: bool,
    pub restarting: bool,
}

impl Guards {
    fn cancelled(self) -> bool {
        self.stopped || self.archived
    }
    fn blocked(self) -> bool {
        self.paused
            || !self.online
            || !self.permission_valid
            || !self.occurrence_valid
            || self.running_turn
            || self.submission_unknown
            || self.restarting
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndKind {
    Normal,
    Interrupted,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndDisposition {
    /// Parent executes its unchanged normal answer/error path.
    DefaultAnswer,
    /// Parent marks attempt Yielded, keeps execution Running, releases only the
    /// exact lease, and creates no answer revision/requester result delivery.
    StageNote,
    AlreadyStaged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    None,
    Waiting,
    Ready,
    Held,
    Running,
    Limited,
    Cancelled,
    Reconciling,
}

impl ContinuationData {
    /// Only a verified definite native refusal/Absent status may release this
    /// reservation. An uncertain response never grants this permission.
    pub fn admission_definitely_refused(
        &mut self,
        wake_id: &str,
        message_id: &str,
    ) -> Result<bool, Error> {
        let record = self.current.as_mut().ok_or(Error::StaleWake)?;
        let wake = record
            .wake
            .as_mut()
            .filter(|wake| wake.id == wake_id)
            .ok_or(Error::StaleWake)?;
        match &wake.submission {
            Submission::Reserved {
                native_message_id, ..
            }
            | Submission::Unknown {
                native_message_id, ..
            } if native_message_id == message_id => {}
            Submission::Unclaimed => return Ok(false),
            _ => return Err(Error::StaleWake),
        }
        wake.submission = Submission::Unclaimed;
        record.reconciling = false;
        if !matches!(record.state, Phase::Cancelled | Phase::Limited) {
            record.state = Phase::Ready;
        }
        record.revision += 1;
        Ok(true)
    }
    pub fn is_empty(&self) -> bool {
        self.current.is_none()
            && self.stages.is_empty()
            && self.child_facts.is_empty()
            && self.actions.is_empty()
            && self.budget_resets.is_empty()
    }

    fn replay(
        &self,
        actor: &Actor,
        id: &str,
        request: &ActionRequest,
    ) -> Result<Option<MutationReceipt>, Error> {
        let Some(old) = self
            .actions
            .iter()
            .find(|entry| &entry.actor == actor && entry.id == id)
        else {
            return Ok(None);
        };
        if &old.request != request {
            return Err(Error::ActionPayloadConflict);
        }
        let mut receipt = old.receipt.clone();
        receipt.replayed = true;
        Ok(Some(receipt))
    }

    fn receipt(&mut self, actor: &Actor, id: &str, request: ActionRequest) -> MutationReceipt {
        let receipt = MutationReceipt {
            continuation: self.current.clone().expect("transition created record"),
            replayed: false,
        };
        self.actions.push(ActionEntry {
            actor: actor.clone(),
            id: id.into(),
            request,
            receipt: receipt.clone(),
        });
        receipt
    }

    /// Record a daemon-authenticated spawn/lineage fact even before declaration.
    pub fn note_spawn(&mut self, proof: AuthenticatedSpawn) -> Result<bool, Error> {
        if !proof.parent.valid()
            || proof.child.id.trim().is_empty()
            || proof.source_event_id.trim().is_empty()
        {
            return Err(Error::InvalidArguments);
        }
        if let Some(old) = self
            .child_facts
            .iter()
            .find(|f| f.parent == proof.parent && f.child == proof.child)
        {
            return if old.spawn_event_id == proof.source_event_id {
                Ok(false)
            } else {
                Err(Error::EvidenceConflict)
            };
        }
        // An event id cannot prove two different spawns in one authenticated context.
        if self
            .child_facts
            .iter()
            .any(|f| f.parent == proof.parent && f.spawn_event_id == proof.source_event_id)
        {
            return Err(Error::EvidenceConflict);
        }
        self.child_facts.push(ChildFact {
            parent: proof.parent,
            child: proof.child,
            spawn_event_id: proof.source_event_id,
            terminal: None,
        });
        Ok(true)
    }

    pub fn note_terminal(&mut self, proof: AuthenticatedTerminal) -> Result<bool, Error> {
        if proof.terminal.source_event_id.trim().is_empty() {
            return Err(Error::InvalidArguments);
        }
        let fact = self
            .child_facts
            .iter_mut()
            .find(|f| f.parent == proof.parent && f.child == proof.child)
            .ok_or(Error::UnverifiedChild)?;
        if let Some(old) = &fact.terminal {
            return if old == &proof.terminal {
                Ok(false)
            } else {
                Err(Error::EvidenceConflict)
            };
        }
        fact.terminal = Some(proof.terminal);
        Ok(true)
    }

    pub fn has_declaration_for(&self, context: &AttemptIdentity) -> bool {
        self.current.as_ref().is_some_and(|r| {
            r.state != Phase::Cancelled
                && r.declaration.source == *context
                && r.declaration.yielded_at.is_none()
        })
    }

    /// Parent's legacy awaits::register must call this before any mutation.
    pub fn check_await_conflict(&self, context: &AttemptIdentity) -> Result<(), Error> {
        if self.has_declaration_for(context) {
            Err(Error::Conflict)
        } else {
            Ok(())
        }
    }

    pub fn declare(
        &mut self,
        actor: &Actor,
        context: &AttemptIdentity,
        request: &DeclareRequest,
        awaiting_this_attempt: bool,
        guards: Guards,
        support: ConditionSupport,
        now: DateTime<Utc>,
    ) -> Result<MutationReceipt, Error> {
        if !context.valid()
            || !matches!(actor, Actor::Agent { session_id } if session_id == &context.owner_session_id)
        {
            return Err(Error::NotCurrentHandler);
        }
        let action = ActionRequest::Declare {
            request: request.clone(),
        };
        if let Some(replay) = self.replay(actor, &request.action_id, &action)? {
            return Ok(replay);
        }
        if request.action_id.trim().is_empty()
            || request.note.trim().is_empty()
            || request.note.chars().count() > MAX_NOTE_CHARS
        {
            return Err(Error::InvalidArguments);
        }
        if request.expected_attempt_id != context.attempt_id {
            return Err(Error::NotCurrentHandler);
        }
        // context must have been matched to the current lease + current attempt
        // by the parent. A running turn is required for declarations only.
        if guards.cancelled()
            || !guards.running_turn
            || !guards.occurrence_valid
            || !guards.permission_valid
            || !guards.online
            || guards.submission_unknown
            || guards.restarting
        {
            return Err(Error::Guarded);
        }
        if awaiting_this_attempt {
            return Err(Error::Conflict);
        }
        self.validate_condition(context, request.wake.as_ref(), support)?;
        let (revision, epoch, round, started, deadline) = match &self.current {
            None => {
                if request.expected_revision.is_some() {
                    return Err(Error::RevisionConflict);
                }
                (1, 1, 1, now, now + Duration::seconds(MAX_ELAPSED_SECONDS))
            }
            Some(old) => {
                if old.state == Phase::Cancelled {
                    return Err(Error::Guarded);
                }
                if old.running_attempt.as_ref() != Some(context) {
                    return Err(Error::NotCurrentHandler);
                }
                if self.has_declaration_for(context) {
                    if request.expected_revision != Some(old.revision) {
                        return Err(Error::RevisionConflict);
                    }
                } else if request
                    .expected_revision
                    .is_some_and(|revision| revision != old.revision)
                {
                    return Err(Error::RevisionConflict);
                }
                (
                    old.revision + 1,
                    old.budget_epoch,
                    old.round_number,
                    old.started_at,
                    old.deadline_at,
                )
            }
        };
        let notice = self.current.as_ref().and_then(|r| r.limit_notice.clone());
        let limit_reason = self.current.as_ref().and_then(|r| r.limit_reason);
        self.current = Some(ContinuationRecord {
            state: if limit_reason.is_some() {
                Phase::Limited
            } else {
                Phase::Declared
            },
            revision,
            budget_epoch: epoch,
            round_number: round,
            started_at: started,
            deadline_at: deadline,
            declaration: Declaration {
                source: context.clone(),
                note: request.note.clone(),
                wake: request.wake.clone(),
                revision,
                yielded_at: None,
            },
            running_attempt: Some(context.clone()),
            next_wake_at: None,
            wake: None,
            limit_reason,
            limit_notice: notice,
            cancel_reason: None,
            reconciling: false,
        });
        Ok(self.receipt(actor, &request.action_id, action))
    }

    fn validate_condition(
        &self,
        context: &AttemptIdentity,
        condition: Option<&WakeCondition>,
        support: ConditionSupport,
    ) -> Result<(), Error> {
        match condition {
            None => Ok(()),
            Some(WakeCondition::After { delay_seconds })
                if (MIN_WAKE_SECONDS..=MAX_ELAPSED_SECONDS as u32).contains(delay_seconds) =>
            {
                Ok(())
            }
            Some(WakeCondition::After { .. }) => Err(Error::InvalidArguments),
            Some(WakeCondition::Children { children, .. }) => {
                if children.is_empty() || children.len() > MAX_CHILDREN {
                    return Err(Error::InvalidArguments);
                }
                for (index, child) in children.iter().enumerate() {
                    if child.id.trim().is_empty() || children[..index].contains(child) {
                        return Err(Error::InvalidArguments);
                    }
                    if !match child.kind {
                        ChildKind::NativeAgent => support.native_children,
                        ChildKind::Delegation => support.delegation_children,
                    } {
                        return Err(Error::ConditionUnsupported);
                    }
                    if !self
                        .child_facts
                        .iter()
                        .any(|fact| fact.parent == *context && fact.child == *child)
                    {
                        return Err(Error::UnverifiedChild);
                    }
                }
                Ok(())
            }
        }
    }

    pub fn end_turn(
        &mut self,
        context: &AttemptIdentity,
        end: EndKind,
        final_message_artifact: Option<String>,
        guards: Guards,
        now: DateTime<Utc>,
        current_director: Option<&str>,
    ) -> EndDisposition {
        if self.stages.iter().any(|note| note.source == *context) {
            return EndDisposition::AlreadyStaged;
        }
        if !self.has_declaration_for(context) {
            // A resumed turn with no new declaration follows the old answer path.
            if self
                .current
                .as_ref()
                .is_some_and(|r| r.running_attempt.as_ref() == Some(context))
            {
                self.cancel("turn_ended_without_declaration");
            }
            return EndDisposition::DefaultAnswer;
        }
        if end != EndKind::Normal || guards.cancelled() || !guards.occurrence_valid {
            self.cancel("turn_did_not_yield_normally");
            return EndDisposition::DefaultAnswer;
        }
        let record = self.current.as_mut().expect("declaration exists");
        self.stages.push(StageNote {
            source: context.clone(),
            budget_epoch: record.budget_epoch,
            round_number: record.round_number,
            declaration_note: record.declaration.note.clone(),
            final_message_artifact,
            ended_at: now,
        });
        record.declaration.yielded_at = Some(now);
        record.running_attempt = None;
        record.reconciling = false;
        record.revision += 1;
        let delay = match record.declaration.wake {
            Some(WakeCondition::After { delay_seconds }) => delay_seconds,
            _ => MIN_WAKE_SECONDS,
        };
        record.next_wake_at = Some(now + Duration::seconds(i64::from(delay)));
        // Stable across scheduler repetitions and serialization/restart.
        record.wake = Some(Wake {
            id: format!(
                "continuation:{}:{}:{}",
                context.delegation_id, record.budget_epoch, record.declaration.revision
            ),
            increments_round: true,
            submission: Submission::Unclaimed,
        });
        if record.state != Phase::Limited {
            record.state = Phase::Waiting;
        }
        self.enforce_limits(now, current_director);
        EndDisposition::StageNote
    }

    /// Enforce wall time even while paused/running. This never emits an
    /// interrupt command or changes the actual current runtime turn.
    pub fn enforce_limits(&mut self, now: DateTime<Utc>, current_director: Option<&str>) -> bool {
        let Some(record) = self.current.as_mut() else {
            return false;
        };
        if matches!(record.state, Phase::Cancelled | Phase::Limited) {
            return false;
        }
        let reason = if now >= record.deadline_at {
            Some(LimitReason::Elapsed)
        } else if record.round_number >= MAX_ROUNDS {
            Some(LimitReason::Rounds)
        } else {
            None
        };
        let Some(reason) = reason else { return false };
        record.state = Phase::Limited;
        record.limit_reason = Some(reason);
        record.revision += 1;
        record.limit_notice = Some(LimitNotice {
            id: format!(
                "continuation-limit:{}:{}",
                record.declaration.source.delegation_id, record.budget_epoch
            ),
            budget_epoch: record.budget_epoch,
            reason,
            owner: true,
            director_session_id: current_director.map(str::to_owned),
            enqueued: false,
        });
        true
    }

    /// Parent MUST enqueue both recipients in its durable outbox in the SAME
    /// AskState transaction. Never mark enqueued and then perform delivery IO.
    pub fn enqueue_limit_notice(&mut self) -> Option<LimitNotice> {
        let notice = self.current.as_mut()?.limit_notice.as_mut()?;
        if notice.enqueued {
            return None;
        }
        notice.enqueued = true;
        Some(notice.clone())
    }

    pub fn cancel(&mut self, reason: &str) -> bool {
        let Some(record) = self.current.as_mut() else {
            return false;
        };
        if record.state == Phase::Cancelled {
            return false;
        }
        record.state = Phase::Cancelled;
        record.cancel_reason = Some(reason.into());
        record.next_wake_at = None;
        record.revision += 1;
        // Keep original wake/submission and stage notes for reconciliation/audit.
        true
    }

    pub fn evaluate(
        &mut self,
        guards: Guards,
        now: DateTime<Utc>,
        current_director: Option<&str>,
    ) -> Eligibility {
        if guards.cancelled() {
            self.cancel("stop_or_archive");
        }
        self.enforce_limits(now, current_director);
        let Some(record) = self.current.as_ref() else {
            return Eligibility::None;
        };
        if record.state == Phase::Cancelled {
            return Eligibility::Cancelled;
        }
        if record.state == Phase::Limited {
            return Eligibility::Limited;
        }
        if record.reconciling || guards.submission_unknown || guards.restarting {
            return Eligibility::Reconciling;
        }
        if record.running_attempt.is_some() {
            return Eligibility::Running;
        }
        if record
            .wake
            .as_ref()
            .is_some_and(|w| !matches!(w.submission, Submission::Unclaimed))
        {
            return Eligibility::Reconciling;
        }
        let due = record.next_wake_at.is_some_and(|due| now >= due);
        let condition_met = match &record.declaration.wake {
            Some(WakeCondition::Children { children, until }) => {
                let terminal = |child: &ChildRef| {
                    self.child_facts.iter().any(|f| {
                        f.parent == record.declaration.source
                            && f.child == *child
                            && f.terminal.is_some()
                    })
                };
                match until {
                    Until::All => children.iter().all(terminal),
                    Until::Any => children.iter().any(terminal),
                }
            }
            _ => true,
        };
        if !due || !condition_met {
            return Eligibility::Waiting;
        }
        let record = self.current.as_mut().expect("record exists");
        if record.state != Phase::Ready {
            record.state = Phase::Ready;
            record.revision += 1;
        }
        if guards.blocked() {
            Eligibility::Held
        } else {
            Eligibility::Ready
        }
    }

    /// Returns true exactly once: only that caller may perform the single
    /// runtime submission, after persisting its attempt+lease in the same lock.
    pub fn reserve_admission(
        &mut self,
        expected_revision: u64,
        wake_id: &str,
        next_attempt_id: &str,
        next_runtime_generation: u64,
        native_message_id: &str,
        guards: Guards,
        now: DateTime<Utc>,
        current_director: Option<&str>,
    ) -> Result<bool, Error> {
        if next_attempt_id.trim().is_empty() || native_message_id.trim().is_empty() {
            return Err(Error::InvalidArguments);
        }
        // Enforce gates/limits again immediately before reserving, including a
        // pause or stop that arrived after evaluate made the wake ready.
        if guards.cancelled() {
            self.cancel("stop_or_archive");
        }
        self.enforce_limits(now, current_director);
        let record = self.current.as_mut().ok_or(Error::StaleWake)?;
        if matches!(record.state, Phase::Cancelled | Phase::Limited) || guards.blocked() {
            return Err(Error::Guarded);
        }
        let wake = record.wake.as_mut().ok_or(Error::StaleWake)?;
        if wake.id != wake_id {
            return Err(Error::StaleWake);
        }
        if !matches!(wake.submission, Submission::Unclaimed) {
            return Ok(false);
        }
        if record.revision != expected_revision {
            return Err(Error::RevisionConflict);
        }
        if record.state != Phase::Ready {
            return Err(Error::Guarded);
        }
        if next_attempt_id == record.declaration.source.attempt_id {
            return Err(Error::NotCurrentHandler);
        }
        wake.submission = Submission::Reserved {
            attempt_id: next_attempt_id.into(),
            runtime_generation: next_runtime_generation,
            native_message_id: native_message_id.into(),
        };
        record.state = Phase::Resuming;
        record.revision += 1;
        Ok(true)
    }

    pub fn submission_unknown(&mut self, wake_id: &str) -> Result<(), Error> {
        let record = self.current.as_mut().ok_or(Error::StaleWake)?;
        let wake = record
            .wake
            .as_mut()
            .filter(|w| w.id == wake_id)
            .ok_or(Error::StaleWake)?;
        match &wake.submission {
            Submission::Reserved {
                attempt_id,
                runtime_generation,
                native_message_id,
            } => {
                wake.submission = Submission::Unknown {
                    attempt_id: attempt_id.clone(),
                    runtime_generation: *runtime_generation,
                    native_message_id: native_message_id.clone(),
                };
                record.reconciling = true;
                record.revision += 1;
                Ok(())
            }
            Submission::Unknown { .. } => Ok(()),
            _ => Err(Error::StaleWake),
        }
    }

    /// Apply a verified original-message admission result, including recovery
    /// queries. Count the new round once, regardless of duplicate observations.
    /// A late acceptance after stop/limit is audited without scheduling anything.
    pub fn admission_accepted(
        &mut self,
        wake_id: &str,
        attempt: &AttemptIdentity,
        native_message_id: &str,
    ) -> Result<bool, Error> {
        let record = self.current.as_mut().ok_or(Error::StaleWake)?;
        let wake = record
            .wake
            .as_mut()
            .filter(|w| w.id == wake_id)
            .ok_or(Error::StaleWake)?;
        if !attempt.valid()
            || attempt.owner_session_id != record.declaration.source.owner_session_id
            || attempt.delegation_id != record.declaration.source.delegation_id
            || attempt.attempt_id == record.declaration.source.attempt_id
            || (attempt.runtime_generation == record.declaration.source.runtime_generation
                && attempt.parent_turn_id == record.declaration.source.parent_turn_id)
        {
            return Err(Error::NotCurrentHandler);
        }
        match &wake.submission {
            Submission::Accepted {
                attempt: old,
                native_message_id: old_message,
            } => {
                return if old == attempt && old_message == native_message_id {
                    Ok(false)
                } else {
                    Err(Error::EvidenceConflict)
                };
            }
            Submission::Reserved {
                attempt_id,
                runtime_generation,
                native_message_id: message,
            }
            | Submission::Unknown {
                attempt_id,
                runtime_generation,
                native_message_id: message,
            } if attempt_id == &attempt.attempt_id
                && *runtime_generation == attempt.runtime_generation
                && message == native_message_id => {}
            _ => return Err(Error::StaleWake),
        }
        wake.submission = Submission::Accepted {
            attempt: attempt.clone(),
            native_message_id: native_message_id.into(),
        };
        if wake.increments_round {
            record.round_number += 1;
        }
        record.running_attempt = Some(attempt.clone());
        record.reconciling = false;
        record.revision += 1;
        Ok(true)
    }

    /// No replay on daemon restart: reserved IO is uncertain, and a declared
    /// turn must be reconciled by its authentic completion, not resubmitted.
    pub fn recover_after_restart(&mut self) {
        let Some(record) = self.current.as_mut() else {
            return;
        };
        let Some(wake) = &mut record.wake else {
            if record.running_attempt.is_some() && !record.reconciling {
                record.reconciling = true;
                record.revision += 1;
            }
            return;
        };
        if let Submission::Reserved {
            attempt_id,
            runtime_generation,
            native_message_id,
        } = &wake.submission
        {
            wake.submission = Submission::Unknown {
                attempt_id: attempt_id.clone(),
                runtime_generation: *runtime_generation,
                native_message_id: native_message_id.clone(),
            };
            record.reconciling = true;
            record.revision += 1;
        }
    }

    pub fn reset_budget(
        &mut self,
        authority: Authority<'_>,
        request: &ResetRequest,
        guards: Guards,
        now: DateTime<Utc>,
    ) -> Result<MutationReceipt, Error> {
        if !authority.may_reset() {
            return Err(Error::Forbidden);
        }
        let action = ActionRequest::Reset {
            request: request.clone(),
        };
        if let Some(replay) = self.replay(authority.actor, &request.action_id, &action)? {
            return Ok(replay);
        }
        if request.action_id.trim().is_empty() {
            return Err(Error::InvalidArguments);
        }
        if guards.cancelled() || guards.blocked() {
            return Err(Error::Guarded);
        }
        let record = self.current.as_mut().ok_or(Error::NotLimited)?;
        if record.revision != request.expected_revision {
            return Err(Error::RevisionConflict);
        }
        if record.state != Phase::Limited {
            return Err(Error::NotLimited);
        }
        if record.running_attempt.is_some()
            || record.reconciling
            || record
                .wake
                .as_ref()
                .is_some_and(|w| !matches!(w.submission, Submission::Unclaimed))
        {
            return Err(Error::Reconciling);
        }
        // A pending notice must have been atomically handed to the durable
        // outbox before reset can overwrite the active epoch's notice field.
        if record.limit_notice.as_ref().is_some_and(|n| !n.enqueued) {
            return Err(Error::Guarded);
        }
        let previous_epoch = record.budget_epoch;
        self.budget_resets.push(BudgetResetAudit {
            actor: authority.actor.clone(),
            action_id: request.action_id.clone(),
            previous_epoch,
            budget_epoch: previous_epoch + 1,
            at: now,
            previous_limit_notice: record.limit_notice.clone(),
        });
        record.budget_epoch += 1;
        record.round_number = 1;
        record.started_at = now;
        record.deadline_at = now + Duration::seconds(MAX_ELAPSED_SECONDS);
        record.state = Phase::Waiting;
        record.limit_reason = None;
        record.limit_notice = None;
        record.next_wake_at = Some(now + Duration::seconds(i64::from(MIN_WAKE_SECONDS)));
        record.wake = Some(Wake {
            id: format!(
                "continuation:{}:{}:reset",
                record.declaration.source.delegation_id, record.budget_epoch
            ),
            increments_round: false,
            submission: Submission::Unclaimed,
        });
        record.revision += 1;
        Ok(self.receipt(authority.actor, &request.action_id, action))
    }
}

#[cfg(test)]
#[path = "continuation/tests.rs"]
mod tests;
