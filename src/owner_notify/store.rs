//! Owner notify records and their store (contract §2–§3).
//!
//! Every mutation goes through [`OwnerNotifyStore::with_state`] (exclusive
//! file lock across read-modify-write). Operations on [`OwnerNotifyState`]
//! are pure and unit-tested without files. Changes queue Management v2
//! notices; the management process publishes them
//! ([`crate::management::v2::owner_notify::publish_pending_notices`]).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::asks::{new_id, now_rfc3339, parse_time};
use crate::config::owner_notify::OwnerNotifySettings;
use crate::notification_levels::{Decision, Evaluator, Level as NotificationLevel, Scope};

/// Final records are pruned after this long.
const RETENTION_DAYS: i64 = 30;
/// At most this many records are kept (oldest final ones go first).
const MAX_RECORDS: usize = 2000;
/// Idempotency records of tool calls are kept this long.
const ACTION_RETENTION_DAYS: i64 = 7;
/// Bound on queued notices while the management process is down.
const MAX_NOTICES: usize = 2000;
pub const MAX_TEXT_CHARS: usize = 4000;
pub const MAX_REPLY_CHARS: usize = 20_000;
pub const MAX_REF_CHARS: usize = 128;
/// `systemKind` (and event `system.safetyBlock`) of a safety block notice.
pub const SAFETY_BLOCK_KIND: &str = "safetyBlock";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    Info,
    Attention,
    Urgent,
}

impl Level {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "attention" => Some(Self::Attention),
            "urgent" => Some(Self::Urgent),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Attention => "attention",
            Self::Urgent => "urgent",
        }
    }
}

/// Who wrote the record: the agent (`owner_notify` tool) or cutex itself
/// (delivery failures, reminders; `agentId` is then the agent it is about).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    #[default]
    Agent,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Open,
    Acknowledged,
    Answered,
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Downgrade {
    pub from: Level,
    /// `rateLimit` | `urgentQuota`.
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub text: String,
    pub replied_at: String,
    pub client_user_message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerNotification {
    pub notification_id: String,
    pub agent_id: String,
    pub agent_name: Option<String>,
    /// Older records (no field) were all written by agents.
    #[serde(default)]
    pub origin: Origin,
    pub text: String,
    pub level: Level,
    pub requested_level: Level,
    pub downgraded: Option<Downgrade>,
    #[serde(default)]
    pub muted: bool,
    pub channels: Vec<String>,
    pub needs_reply: bool,
    pub delegation: Option<String>,
    pub job: Option<String>,
    pub turn_id: Option<String>,
    pub state: State,
    pub created_at: String,
    pub acknowledged_at: Option<String>,
    pub reply: Option<Reply>,
    pub closed_at: Option<String>,
    /// Notification level decided at creation (capability
    /// `notification-levels`; absent on older records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_level: Option<NotificationLevel>,
    /// The registered event that decided `notificationLevel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_event: Option<String>,
    /// Methods decided at creation (`needsYou`, `mark`, `desktop`, `phone`,
    /// `sound`, `bigAlert`); `needsYou` = the record counts in 需要你 while
    /// open. Absent on older records (then the r216 rule applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    /// Kind of a cutex notice (origin `system`): `quarantine`,
    /// `deliveryFailed`, `idleDelegation` (r217 – r218 also wrote `preActivationSkipped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_kind: Option<String>,
    /// A runtime safety check the agent's runtime declined under full
    /// access (`systemKind: safetyBlock`, capability
    /// `claude-safety-grants`, docs/safety-grants.md). Its `state` decides
    /// whether the record still needs the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_block: Option<SafetyBlock>,
}

/// One declined runtime safety check (Claude Code under full access) and
/// the owner's decision on it. Field values come from the runtime
/// (`thread/safetyCheck/blocked|updated`); `state` is `blocked` until the
/// owner decides, then `granted` (one exact retry within `grantSeconds`),
/// `used`, `dismissed` or `expired`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyBlock {
    pub block_id: String,
    /// `claude` (the only runtime with such checks so far).
    pub runtime: String,
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// The declined tool call (timeline item id).
    #[serde(default)]
    pub item_id: Option<String>,
    pub tool_name: String,
    /// The exact tool input the runtime declined.
    pub tool_input: Value,
    /// Display form (the shell command for `Bash`).
    pub command: String,
    /// The runtime's reason (Claude Code's own text).
    pub reason: String,
    pub state: String,
    pub blocked_at: String,
    #[serde(default)]
    pub decided_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub used_at: Option<String>,
    pub grant_seconds: u64,
}

impl SafetyBlock {
    pub const BLOCKED: &'static str = "blocked";

    /// Still waiting for the owner.
    pub fn is_open(&self) -> bool {
        self.state == Self::BLOCKED
    }
}

impl OwnerNotification {
    pub fn is_final(&self) -> bool {
        if let Some(block) = &self.safety_block {
            return !block.is_open();
        }
        match self.state {
            State::Answered | State::Dismissed => true,
            State::Acknowledged => !self.needs_reply,
            State::Open => false,
        }
    }

    /// In 需要你 (contract §2): an agent's needs-reply message until it is
    /// answered / dismissed; otherwise an open record whose decided methods
    /// include `needsYou`. Records without `actions` (written before
    /// notification levels) keep the r216 rule: open with a channel, never a
    /// cutex notice (`origin: system`).
    pub fn pending(&self) -> bool {
        // A safety block needs the owner until decided, also after the
        // owner read the agent (`mark_agent_read` only acknowledges it).
        if let Some(block) = &self.safety_block {
            return block.is_open() && self.actions.as_ref().is_some_and(|actions| actions.iter().any(|a| a == "needsYou"));
        }
        if self.needs_reply && self.origin == Origin::Agent {
            return matches!(self.state, State::Open | State::Acknowledged);
        }
        match &self.actions {
            Some(actions) => self.state == State::Open && actions.iter().any(|action| action == "needsYou"),
            None => self.origin != Origin::System && self.state == State::Open && !self.channels.is_empty(),
        }
    }

    /// OS notifications / the big alert still show: open agent messages, and
    /// open cutex notices the owner gave a channel (notification levels).
    pub fn alerting(&self) -> bool {
        self.state == State::Open && (self.origin != Origin::System || (self.actions.is_some() && !self.channels.is_empty()))
    }

    /// Reaches the phone (`messageToOwner`, webhook `agent.message_to_owner`):
    /// channel `phone`; cutex notices only when notification levels decided
    /// it (older system records may list the phone).
    pub fn to_phone(&self) -> bool {
        self.has_channel("phone") && (self.origin != Origin::System || self.actions.is_some())
    }

    /// The notification level: decided at creation, or derived for older
    /// records the way the former legacy preset mapped them.
    pub fn effective_notification_level(&self) -> NotificationLevel {
        self.notification_level.unwrap_or(match (self.origin, self.level) {
            (Origin::System, _) => NotificationLevel::Record,
            (_, _) if self.downgraded.as_ref().is_some_and(|d| d.reason == "rateLimit") => NotificationLevel::Record,
            (_, Level::Urgent) => NotificationLevel::Urgent,
            (_, Level::Attention) => NotificationLevel::NeedsYou,
            (_, Level::Info) => NotificationLevel::Inform,
        })
    }

    pub fn has_channel(&self, channel: &str) -> bool {
        self.channels.iter().any(|c| c == channel)
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A queued Management v2 event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub method: String,
    pub session: String,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRecord {
    pub result: Value,
    pub digest: String,
    pub at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerNotifyState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub notifications: BTreeMap<String, OwnerNotification>,
    /// `<caller>\u{1f}<action_id>` → the tool result.
    #[serde(default)]
    pub actions: BTreeMap<String, ActionRecord>,
    #[serde(default)]
    pub notices: Vec<Notice>,
}

/// What the agent asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewNotification {
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub text: String,
    pub level: Level,
    pub needs_reply: bool,
    pub delegation: Option<String>,
    pub job: Option<String>,
    pub turn_id: Option<String>,
}

/// An owner action error: (HTTP status, code, message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl OwnerError {
    fn not_found(id: &str) -> Self {
        Self { status: 404, code: "notification_not_found", message: format!("no owner notification {id} for this agent") }
    }
}

/// How an agent's message is delivered (contract §3 + notification levels).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// Effective agent level (after a downgrade).
    pub level: Level,
    pub downgraded: Option<Downgrade>,
    /// An agent (or project) mute applied: no channel fired.
    pub muted: bool,
    /// Notification level, methods and channels.
    pub decision: Decision,
}

/// The notification events an agent message matches: its (effective)
/// level, plus `ownerNotify.needsReply`; a rate-limited message matches
/// `ownerNotify.rateLimited` only.
pub fn agent_message_events(level: Level, needs_reply: bool, rate_limited: bool) -> Vec<String> {
    if rate_limited {
        return vec!["ownerNotify.rateLimited".to_string()];
    }
    let mut events = vec![format!("ownerNotify.{}", level.as_str())];
    if needs_reply {
        events.push("ownerNotify.needsReply".to_string());
    }
    events
}

/// The notification event of a cutex notice of `kind`.
pub fn system_notice_event(kind: Option<&str>, evaluator: &Evaluator) -> String {
    kind.map(|kind| format!("system.{kind}"))
        .filter(|event| evaluator.registry().event(event).is_some())
        .unwrap_or_else(|| "system.other".to_string())
}

/// Rate limits, then the notification level tables (contract §3).
pub fn decide_delivery(
    previous: &[&OwnerNotification],
    requested: Level,
    needs_reply: bool,
    settings: &OwnerNotifySettings,
    evaluator: &Evaluator,
    agent_id: &str,
    project: Option<Option<&str>>,
    now: DateTime<Utc>,
) -> Delivery {
    let override_ = settings.agent_overrides.get(agent_id).map(String::as_str);
    let hour_ago = now - Duration::hours(1);
    let recent: Vec<&&OwnerNotification> = previous
        .iter()
        .filter(|record| record.agent_id == agent_id)
        .filter(|record| parse_time(&record.created_at).is_some_and(|at| at > hour_ago))
        .collect();
    let mut level = requested;
    let mut downgraded = None;
    let mut rate_limited = false;
    if override_ != Some("force") {
        if recent.len() >= settings.rate_limit.per_hour as usize {
            // Also an info message: it loses its channels (event
            // ownerNotify.rateLimited, record in every built-in preset).
            downgraded = Some(Downgrade { from: requested, reason: "rateLimit".into() });
            level = Level::Info;
            rate_limited = true;
        } else if requested == Level::Urgent
            && recent.iter().filter(|record| record.level == Level::Urgent).count() >= settings.rate_limit.urgent_per_hour as usize
        {
            level = Level::Attention;
            downgraded = Some(Downgrade { from: Level::Urgent, reason: "urgentQuota".into() });
        }
    }
    let events = agent_message_events(level, needs_reply, rate_limited);
    let events: Vec<&str> = events.iter().map(String::as_str).collect();
    let decision = evaluator.decide(&events, Scope { agent: Some(agent_id), project });
    Delivery { level, downgraded, muted: decision.muted, decision }
}

/// Effective level, channels, downgrade and mute under the default tables.
pub fn resolve_delivery(
    previous: &[&OwnerNotification],
    requested: Level,
    settings: &OwnerNotifySettings,
    agent_id: &str,
    now: DateTime<Utc>,
) -> (Level, Vec<String>, Option<Downgrade>, bool) {
    let delivery = decide_delivery(previous, requested, false, settings, &Evaluator::with_owner(settings), agent_id, None, now);
    (delivery.level, delivery.decision.channels, delivery.downgraded, delivery.muted)
}

impl OwnerNotifyState {
    fn action_key(caller: &str, action_id: &str) -> String {
        format!("{caller}\u{1f}{action_id}")
    }

    /// A previous result of this (caller, action_id): `Ok(Some)` replay,
    /// `Err(())` the id was used for a different request.
    pub fn replay(&self, caller: &str, action_id: &str, digest: &str) -> Result<Option<Value>, ()> {
        match self.actions.get(&Self::action_key(caller, action_id)) {
            Some(record) if record.digest == digest => Ok(Some(record.result.clone())),
            Some(_) => Err(()),
            None => Ok(None),
        }
    }

    pub fn remember(&mut self, caller: &str, action_id: &str, digest: &str, result: &Value, now: DateTime<Utc>) {
        self.actions.insert(
            Self::action_key(caller, action_id),
            ActionRecord { result: result.clone(), digest: digest.to_string(), at: now_rfc3339(now) },
        );
    }

    fn notice(&mut self, method: &str, record: &OwnerNotification) {
        self.push_notice(method, &record.agent_id, json!({ "notification": record }));
    }

    fn push_notice(&mut self, method: &str, session: &str, params: Value) {
        self.notices.push(Notice { method: method.to_string(), session: session.to_string(), params });
        if self.notices.len() > MAX_NOTICES {
            let excess = self.notices.len() - MAX_NOTICES;
            self.notices.drain(..excess);
        }
    }

    /// Record a message from an agent (rate limits and settings applied).
    pub fn create(&mut self, new: NewNotification, settings: &OwnerNotifySettings, now: DateTime<Utc>) -> OwnerNotification {
        self.create_with_origin(new, Origin::Agent, settings, now)
    }

    /// Record a notice; `System` for notices cutex writes itself. Rate
    /// limits count records of the same origin only, so cutex's notices
    /// about an agent never use up that agent's own quota (and vice versa).
    /// `System` notices are records only (notification levels 记录): level
    /// `info`, no channels (no desktop toast, phone push or big alert).
    pub fn create_with_origin(
        &mut self,
        new: NewNotification,
        origin: Origin,
        settings: &OwnerNotifySettings,
        now: DateTime<Utc>,
    ) -> OwnerNotification {
        let evaluator = Evaluator::with_owner(settings);
        self.create_decided(new, origin, None, settings, &evaluator, None, now)
    }

    /// Record a notice with the owner's notification level tables
    /// (`evaluator`). `system_kind` names a cutex notice's kind (event
    /// `system.<kind>`); `project` is the sender's project when known
    /// (`Some(None)` = independent) for project mutes.
    #[allow(clippy::too_many_arguments)]
    pub fn create_decided(
        &mut self,
        new: NewNotification,
        origin: Origin,
        system_kind: Option<&str>,
        settings: &OwnerNotifySettings,
        evaluator: &Evaluator,
        project: Option<Option<&str>>,
        now: DateTime<Utc>,
    ) -> OwnerNotification {
        let previous: Vec<&OwnerNotification> = self.notifications.values().filter(|record| record.origin == origin).collect();
        let (level, downgraded, muted, decision) = if origin == Origin::System {
            // cutex's own notices: agent level info, never rate-limited or
            // replyable; their notification level is the owner's choice
            // (record in every built-in preset: no channel, no need).
            let event = system_notice_event(system_kind, evaluator);
            let decision = evaluator.decide(&[event.as_str()], Scope { agent: Some(&new.agent_id), project });
            (Level::Info, None, false, decision)
        } else {
            let delivery =
                decide_delivery(&previous, new.level, new.needs_reply, settings, evaluator, &new.agent_id, project, now);
            (delivery.level, delivery.downgraded, delivery.muted, delivery.decision)
        };
        let record = OwnerNotification {
            notification_id: new_id("ntf"),
            agent_id: new.agent_id,
            agent_name: new.agent_name,
            origin,
            text: new.text,
            level,
            requested_level: new.level,
            downgraded,
            muted,
            channels: decision.channels.clone(),
            needs_reply: new.needs_reply,
            delegation: new.delegation,
            job: new.job,
            turn_id: new.turn_id,
            state: State::Open,
            created_at: now_rfc3339(now),
            acknowledged_at: None,
            reply: None,
            closed_at: None,
            notification_level: Some(decision.level),
            notification_event: Some(decision.event.clone()),
            actions: Some(decision.actions.clone()),
            system_kind: if origin == Origin::System { system_kind.map(str::to_string) } else { None },
            safety_block: None,
        };
        self.notifications.insert(record.notification_id.clone(), record.clone());
        self.notice(super::CREATED_EVENT, &record);
        self.prune(now);
        record
    }

    fn get_for(&mut self, agent_id: &str, id: &str) -> Result<&mut OwnerNotification, OwnerError> {
        self.notifications
            .get_mut(id)
            .filter(|record| record.agent_id == agent_id)
            .ok_or_else(|| OwnerError::not_found(id))
    }

    /// The owner saw it: `open` → `acknowledged`. Returns (record, changed).
    pub fn acknowledge(&mut self, agent_id: &str, id: &str, now: DateTime<Utc>) -> Result<(OwnerNotification, bool), OwnerError> {
        let record = self.get_for(agent_id, id)?;
        if record.state != State::Open {
            return Ok((record.clone(), false));
        }
        record.state = State::Acknowledged;
        record.acknowledged_at = Some(now_rfc3339(now));
        let record = record.clone();
        self.notice(super::UPDATED_EVENT, &record);
        Ok((record, true))
    }

    /// The owner closed a needs-reply message without replying.
    pub fn dismiss(&mut self, agent_id: &str, id: &str, now: DateTime<Utc>) -> Result<(OwnerNotification, bool), OwnerError> {
        let record = self.get_for(agent_id, id)?;
        if record.is_final() {
            return Ok((record.clone(), false));
        }
        let at = now_rfc3339(now);
        record.acknowledged_at.get_or_insert_with(|| at.clone());
        record.state = if record.needs_reply { State::Dismissed } else { State::Acknowledged };
        if record.needs_reply {
            record.closed_at = Some(at);
        }
        let record = record.clone();
        self.notice(super::UPDATED_EVENT, &record);
        Ok((record, true))
    }

    /// A reply may be sent (record of this agent, not answered / dismissed).
    pub fn check_reply(&self, agent_id: &str, id: &str) -> Result<OwnerNotification, OwnerError> {
        let record = self
            .notifications
            .get(id)
            .filter(|record| record.agent_id == agent_id)
            .ok_or_else(|| OwnerError::not_found(id))?;
        if record.origin == Origin::System || !record.needs_reply {
            return Err(OwnerError {
                status: 409,
                code: "notification_not_replyable",
                message: if record.origin == Origin::System {
                    format!("owner notification {id} is a cutex notice, not a message from the agent; it takes no reply")
                } else {
                    format!("owner notification {id} does not ask for a reply")
                },
            });
        }
        if matches!(record.state, State::Answered | State::Dismissed) {
            return Err(OwnerError {
                status: 409,
                code: "notification_closed",
                message: format!("owner notification {id} was already answered or dismissed"),
            });
        }
        Ok(record.clone())
    }

    /// The owner's reply was handed to the agent.
    pub fn answered(&mut self, agent_id: &str, id: &str, reply: Reply, now: DateTime<Utc>) -> Result<OwnerNotification, OwnerError> {
        let record = self.get_for(agent_id, id)?;
        let at = now_rfc3339(now);
        record.acknowledged_at.get_or_insert_with(|| at.clone());
        record.state = State::Answered;
        record.reply = Some(reply);
        record.closed_at = Some(at);
        let record = record.clone();
        self.notice(super::UPDATED_EVENT, &record);
        Ok(record)
    }

    /// Record a runtime safety block as a cutex notice (`systemKind:
    /// safetyBlock`). Idempotent by block id (several processes may observe
    /// the same runtime event): returns (record, created).
    #[allow(clippy::too_many_arguments)]
    pub fn record_safety_block(
        &mut self,
        agent_id: &str,
        agent_name: Option<String>,
        block: SafetyBlock,
        settings: &OwnerNotifySettings,
        evaluator: &Evaluator,
        project: Option<Option<&str>>,
        now: DateTime<Utc>,
    ) -> (OwnerNotification, bool) {
        if let Some(existing) = self.safety_record(agent_id, &block.block_id) {
            return (existing.clone(), false);
        }
        let text = crate::asks::truncate_chars(
            &format!("Claude Code 安全检查拦下了一条命令：{}\n原因：{}", block.command, block.reason),
            MAX_TEXT_CHARS,
        );
        let new = NewNotification {
            agent_id: agent_id.to_string(),
            agent_name,
            text,
            level: Level::Attention,
            needs_reply: false,
            delegation: None,
            job: None,
            turn_id: block.turn_id.clone(),
        };
        let mut record = self.create_decided(new, Origin::System, Some(SAFETY_BLOCK_KIND), settings, evaluator, project, now);
        // create_decided queued `created` without the block: replace it.
        self.notices.pop();
        record.safety_block = Some(block);
        self.notifications.insert(record.notification_id.clone(), record.clone());
        self.notice(super::CREATED_EVENT, &record);
        (record, true)
    }

    /// The record of a safety block of this agent.
    pub fn safety_record(&self, agent_id: &str, block_id: &str) -> Option<&OwnerNotification> {
        self.notifications.values().find(|record| {
            record.agent_id == agent_id && record.safety_block.as_ref().is_some_and(|block| block.block_id == block_id)
        })
    }

    /// Apply a newer block state (the runtime's, or the owner's decision as
    /// the runtime confirmed it). A decided block closes the record.
    pub fn update_safety_block(
        &mut self,
        agent_id: &str,
        block: SafetyBlock,
        now: DateTime<Utc>,
    ) -> Result<(OwnerNotification, bool), OwnerError> {
        let id = self
            .safety_record(agent_id, &block.block_id)
            .map(|record| record.notification_id.clone())
            .ok_or_else(|| OwnerError {
                status: 404,
                code: "safety_block_not_found",
                message: format!("no safety block {} for this agent", block.block_id),
            })?;
        let record = self.get_for(agent_id, &id)?;
        if record.safety_block.as_ref() == Some(&block) {
            return Ok((record.clone(), false));
        }
        let at = now_rfc3339(now);
        if !block.is_open() {
            record.acknowledged_at.get_or_insert_with(|| at.clone());
            if record.state == State::Open {
                record.state = State::Acknowledged;
            }
            record.closed_at.get_or_insert(at);
        }
        record.safety_block = Some(block);
        let record = record.clone();
        self.notice(super::UPDATED_EVENT, &record);
        Ok((record, true))
    }

    /// The owner deleted the record (any state): it is removed, so lists,
    /// bootstrap and summary needs no longer return it. Returns the removed
    /// record; a second delete is `notification_not_found`.
    pub fn delete(&mut self, agent_id: &str, id: &str) -> Result<OwnerNotification, OwnerError> {
        self.get_for(agent_id, id)?;
        let record = self.notifications.remove(id).ok_or_else(|| OwnerError::not_found(id))?;
        self.push_notice(
            super::DELETED_EVENT,
            &record.agent_id,
            json!({ "notificationId": record.notification_id, "agentId": record.agent_id }),
        );
        Ok(record)
    }

    /// The owner read the agent (`POST /v2/summary/read`): its open records
    /// become acknowledged.
    pub fn mark_agent_read(&mut self, agent_id: &str, now: DateTime<Utc>) -> usize {
        let ids: Vec<String> = self
            .notifications
            .values()
            .filter(|record| record.agent_id == agent_id && record.state == State::Open)
            .map(|record| record.notification_id.clone())
            .collect();
        for id in &ids {
            let _ = self.acknowledge(agent_id, id, now);
        }
        ids.len()
    }

    /// Records newest first, optionally one agent's, by filter.
    pub fn list(&self, filter: &str, agent: Option<&str>, limit: usize) -> Vec<OwnerNotification> {
        let mut records: Vec<&OwnerNotification> = self
            .notifications
            .values()
            .filter(|record| agent.is_none_or(|agent| record.agent_id == agent))
            .filter(|record| match filter {
                "pending" => record.pending(),
                "alerting" => record.alerting(),
                _ => true,
            })
            .collect();
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.notification_id.cmp(&a.notification_id)));
        records.into_iter().take(limit).cloned().collect()
    }

    /// Drop old final records, cap the record count, expire idempotency keys.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(RETENTION_DAYS);
        self.notifications.retain(|_, record| {
            !record.is_final()
                || parse_time(record.closed_at.as_deref().or(record.acknowledged_at.as_deref()).unwrap_or(&record.created_at))
                    .is_none_or(|at| at > cutoff)
        });
        if self.notifications.len() > MAX_RECORDS {
            let mut ordered: Vec<(bool, String, String)> = self
                .notifications
                .values()
                .map(|record| (!record.is_final(), record.created_at.clone(), record.notification_id.clone()))
                .collect();
            ordered.sort();
            let excess = self.notifications.len() - MAX_RECORDS;
            for (_, _, id) in ordered.into_iter().take(excess) {
                self.notifications.remove(&id);
            }
        }
        let action_cutoff = now - Duration::days(ACTION_RETENTION_DAYS);
        self.actions
            .retain(|_, record| parse_time(&record.at).is_none_or(|at| at > action_cutoff));
    }
}

/// The file-locked store.
#[derive(Clone, Debug)]
pub struct OwnerNotifyStore {
    dir: PathBuf,
}

impl OwnerNotifyStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("owner-notify")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("owner-notify-v1.json")
    }

    fn lock_file(&self) -> anyhow::Result<std::fs::File> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("owner-notify-v1.lock"))?)
    }

    fn read_state(path: &std::path::Path) -> anyhow::Result<OwnerNotifyState> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(OwnerNotifyState::default()),
            Ok(bytes) => serde_json::from_slice(&bytes).context("owner notify store is not valid JSON"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(OwnerNotifyState::default()),
            Err(error) => Err(error).context("read owner notify store"),
        }
    }

    /// Read-only snapshot (empty when the store does not exist yet).
    pub fn snapshot(&self) -> anyhow::Result<OwnerNotifyState> {
        if !self.state_path().exists() {
            return Ok(OwnerNotifyState::default());
        }
        let lock = self.lock_file()?;
        lock.lock_shared().context("owner notify store lock")?;
        Self::read_state(&self.state_path())
    }

    /// Locked read-modify-write; saves only when the state changed.
    pub fn with_state<T>(&self, mutate: impl FnOnce(&mut OwnerNotifyState) -> T) -> anyhow::Result<T> {
        let lock = self.lock_file()?;
        lock.lock_exclusive().context("owner notify store lock")?;
        let path = self.state_path();
        let mut state = Self::read_state(&path)?;
        let before = serde_json::to_vec(&state)?;
        let result = mutate(&mut state);
        if serde_json::to_vec(&state)? != before {
            state.revision += 1;
            crate::config::atomic::write_private_pretty_json_atomic(&path, &state, "owner notify store")?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(agent: &str, level: Level, needs_reply: bool) -> NewNotification {
        NewNotification {
            agent_id: agent.into(),
            agent_name: Some("Kimi".into()),
            text: "Build is green".into(),
            level,
            needs_reply,
            delegation: None,
            job: None,
            turn_id: Some("turn-1".into()),
        }
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-08T10:00:00Z").unwrap().with_timezone(&Utc) + Duration::minutes(minutes)
    }

    #[test]
    fn channels_follow_level_settings_and_overrides() {
        let mut state = OwnerNotifyState::default();
        let mut settings = OwnerNotifySettings::default();
        // Default notification tables: info is a record, attention informs.
        let info = state.create(new("a", Level::Info, false), &settings, at(0));
        assert!(info.channels.is_empty());
        let attention = state.create(new("a", Level::Attention, false), &settings, at(0));
        assert_eq!(attention.channels, vec!["desktop"]);
        let urgent = state.create(new("a", Level::Urgent, true), &settings, at(1));
        assert_eq!(urgent.channels, vec!["desktop", "phone", "bigAlert"]);
        assert!(urgent.pending() && urgent.alerting());
        settings.agent_overrides.insert("b".into(), "mute".into());
        let muted = state.create(new("b", Level::Urgent, false), &settings, at(2));
        assert!(muted.muted && muted.channels.is_empty() && muted.pending(), "a mute keeps the need");
        assert_eq!(state.notices.len(), 4);
        assert_eq!(state.notices[0].method, super::super::CREATED_EVENT);
        assert_eq!(state.notices[0].session, "a");
    }

    #[test]
    fn rate_limits_downgrade_silently_and_force_bypasses() {
        let mut state = OwnerNotifyState::default();
        let mut settings = OwnerNotifySettings::default();
        settings.rate_limit.per_hour = 3;
        settings.rate_limit.urgent_per_hour = 1;
        let first = state.create(new("a", Level::Urgent, false), &settings, at(0));
        assert_eq!(first.level, Level::Urgent);
        let second = state.create(new("a", Level::Urgent, false), &settings, at(1));
        assert_eq!(second.level, Level::Attention);
        assert_eq!(second.downgraded.as_ref().unwrap().reason, "urgentQuota");
        assert_eq!(second.channels, vec!["desktop"]);
        state.create(new("a", Level::Info, false), &settings, at(2));
        let over = state.create(new("a", Level::Attention, true), &settings, at(3));
        assert_eq!(over.level, Level::Info);
        assert_eq!(over.requested_level, Level::Attention);
        assert!(over.channels.is_empty());
        assert_eq!(over.downgraded.as_ref().unwrap().reason, "rateLimit");
        assert!(over.pending(), "a needs-reply message stays pending even when quiet");
        // Another agent is not affected; after an hour the window resets.
        assert_eq!(state.create(new("b", Level::Urgent, false), &settings, at(4)).level, Level::Urgent);
        assert_eq!(state.create(new("a", Level::Urgent, false), &settings, at(70)).level, Level::Urgent);
        settings.agent_overrides.insert("a".into(), "force".into());
        for minute in 71..80 {
            assert_eq!(state.create(new("a", Level::Urgent, false), &settings, at(minute)).level, Level::Urgent);
        }
    }

    #[test]
    fn ack_reply_dismiss_and_read() {
        let mut state = OwnerNotifyState::default();
        let settings = OwnerNotifySettings::default();
        let plain = state.create(new("a", Level::Attention, false), &settings, at(0));
        let ask = state.create(new("a", Level::Urgent, true), &settings, at(1));
        assert_eq!(state.acknowledge("b", &plain.notification_id, at(2)).unwrap_err().code, "notification_not_found");
        let (acked, changed) = state.acknowledge("a", &plain.notification_id, at(2)).unwrap();
        assert!(changed && acked.is_final() && !acked.pending() && !acked.alerting());
        assert!(!state.acknowledge("a", &plain.notification_id, at(3)).unwrap().1);
        assert_eq!(state.mark_agent_read("a", at(3)), 1);
        let read = state.notifications[&ask.notification_id].clone();
        assert_eq!(read.state, State::Acknowledged);
        assert!(read.pending() && !read.alerting());
        state.check_reply("a", &ask.notification_id).unwrap();
        let reply = Reply { text: "go".into(), replied_at: now_rfc3339(at(4)), client_user_message_id: "x".into() };
        let answered = state.answered("a", &ask.notification_id, reply, at(4)).unwrap();
        assert_eq!(answered.state, State::Answered);
        assert!(!answered.pending());
        assert_eq!(state.check_reply("a", &ask.notification_id).unwrap_err().status, 409);
        let other = state.create(new("a", Level::Info, true), &settings, at(5));
        let (dismissed, _) = state.dismiss("a", &other.notification_id, at(6)).unwrap();
        assert_eq!(dismissed.state, State::Dismissed);
        assert_eq!(state.list("pending", None, 10).len(), 0);
        assert_eq!(state.list("all", Some("a"), 2).len(), 2);
        assert_eq!(state.list("all", None, 10)[0].notification_id, other.notification_id);
    }

    #[test]
    fn system_notices_take_no_reply_and_keep_their_own_quota() {
        let mut state = OwnerNotifyState::default();
        let mut settings = OwnerNotifySettings::default();
        settings.rate_limit.per_hour = 1;
        let agent = state.create(new("a", Level::Urgent, false), &settings, at(0));
        assert_eq!(agent.origin, Origin::Agent);
        assert_eq!(agent.to_json()["origin"], "agent");
        let system = state.create_with_origin(new("a", Level::Attention, true), Origin::System, &settings, at(1));
        assert_eq!(system.origin, Origin::System);
        assert!(system.downgraded.is_none(), "the agent's message does not use up cutex's quota");
        // A record only: info, no channels, never in 需要你 or alerting.
        assert_eq!((system.level, system.requested_level), (Level::Info, Level::Attention));
        assert!(system.channels.is_empty() && !system.muted);
        assert!(!system.pending() && !system.alerting());
        assert_eq!(state.list("pending", Some("a"), 10).len(), 1, "only the agent's message");
        assert_eq!(state.list("all", Some("a"), 10).len(), 2, "the notice stays listed");
        assert_eq!(system.to_json()["origin"], "system");
        let refused = state.check_reply("a", &system.notification_id).unwrap_err();
        assert_eq!((refused.status, refused.code), (409, "notification_not_replyable"));
        let plain = state.check_reply("a", &agent.notification_id).unwrap_err();
        assert_eq!((plain.status, plain.code), (409, "notification_not_replyable"));
        // Records written before the field existed are agent messages.
        let mut legacy = agent.to_json();
        legacy.as_object_mut().unwrap().remove("origin");
        let legacy: OwnerNotification = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.origin, Origin::Agent);
    }

    #[test]
    fn delete_removes_any_record_of_that_agent_and_queues_an_event() {
        let mut state = OwnerNotifyState::default();
        let settings = OwnerNotifySettings::default();
        let acked = state.create_with_origin(new("a", Level::Attention, false), Origin::System, &settings, at(0));
        state.acknowledge("a", &acked.notification_id, at(1)).unwrap();
        let waiting = state.create(new("a", Level::Info, true), &settings, at(2));
        state.notices.clear();
        assert_eq!(state.delete("b", &acked.notification_id).unwrap_err().code, "notification_not_found");
        let removed = state.delete("a", &acked.notification_id).unwrap();
        assert_eq!(removed.notification_id, acked.notification_id);
        assert!(!state.notifications.contains_key(&acked.notification_id));
        assert_eq!(state.notices.len(), 1);
        assert_eq!(state.notices[0].method, super::super::DELETED_EVENT);
        assert_eq!(state.notices[0].session, "a");
        assert_eq!(state.notices[0].params, json!({"notificationId": acked.notification_id, "agentId": "a"}));
        assert_eq!(state.delete("a", &acked.notification_id).unwrap_err().status, 404);
        state.delete("a", &waiting.notification_id).unwrap();
        assert!(state.list("all", Some("a"), 10).is_empty());
    }

    #[test]
    fn prune_drops_old_final_records_and_keeps_pending() {
        let mut state = OwnerNotifyState::default();
        let settings = OwnerNotifySettings::default();
        let old = state.create(new("a", Level::Info, false), &settings, at(0));
        let waiting = state.create(new("a", Level::Info, true), &settings, at(0));
        state.acknowledge("a", &old.notification_id, at(1)).unwrap();
        state.prune(at(60 * 24 * 31));
        assert!(!state.notifications.contains_key(&old.notification_id));
        assert!(state.notifications.contains_key(&waiting.notification_id));
    }

    #[test]
    fn store_round_trips_under_a_lock() {
        let dir = std::env::temp_dir().join(format!("cutex-owner-notify-{}", uuid::Uuid::new_v4().simple()));
        let store = OwnerNotifyStore::at(dir.join("owner-notify"));
        assert!(store.snapshot().unwrap().notifications.is_empty());
        let created = store
            .with_state(|state| state.create(new("a", Level::Info, false), &OwnerNotifySettings::default(), at(0)))
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.notifications[&created.notification_id], created);
        let _ = std::fs::remove_dir_all(dir);
    }
}
