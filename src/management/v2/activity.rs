use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use serde::Deserialize;
use serde::Serialize;

#[cfg(test)]
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::observability::{
    sanitize_visible_output, ObservationAssociation, SafeOutputClass, SafeOutputProjection,
    SafeToolCallClass, SafeToolCallProjection, SafeToolCallStatus,
};

use super::model::EventEnvelope;
use super::model::EventSource;
use super::model::NativeMessageKind;
use super::model::MAX_SAFE_SEQUENCE;

const ACTIVITY_STATE_FILE: &str = "session-activity-state.json";
const OUTPUT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

static ACTIVITY_RECORDER: OnceLock<ActivityRecorder> = OnceLock::new();

/// Pure conversation projection shared by SessionResource and agent_list.
/// observedAt is a read timestamp, never persisted as conversation activity.
pub fn agent_projection(activity:Option<&SessionActivityState>,online:Option<bool>,busy:bool,generation:Option<u64>,now:chrono::DateTime<chrono::Utc>)->serde_json::Value {
    let last=activity.and_then(SessionActivityState::conversation_at).filter(|s|DateTime::parse_from_rfc3339(s).is_ok());
    let known=last.is_some();
    let busy_since=activity.filter(|a|busy&&generation.is_some()&&a.runtime_generation==generation)
        .and_then(|a|a.last_turn_started_at.as_deref());
    serde_json::json!({"lastActivityAt":last,"activitySource":if known{"conversation"}else{"unknown"},
        "observedAt":now.to_rfc3339(),"busySince":busy_since,
        "activityStatus":match online{Some(false)=>"offline",Some(true) if busy=>"busy",Some(true) if known=>"idle",_=>"unknown"},
        "idleThresholdSeconds":7200})
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub runtime_generation: Option<u64>,
    #[serde(default)]
    pub last_output_at: Option<String>,
    #[serde(default)]
    pub last_output_completed_at: Option<String>,
    #[serde(default)]
    pub last_turn_completed_at: Option<String>,
    /// Generation of this completion, independent of later output/start events.
    /// Legacy stores lack this evidence and cannot establish cache warmth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_completed_generation: Option<u64>,
    #[serde(default)]
    pub last_file_change_at: Option<String>,
    /// Last native `turn/started` (the owner's or an agent's input starting a turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_started_at: Option<String>,
    /// Last real conversation activity: a turn started, output, a tool call,
    /// a file change, or a turn that ended other than interrupted. Runtime
    /// lifecycle (start / stop / restart, reconnects, the interrupt of a
    /// stale turn on resume) and session metadata never move it. Absent in
    /// stores written before `session-conversation-activity`: see
    /// [`SessionActivityState::conversation_at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_conversation_at: Option<String>,
    #[serde(default)]
    pub last_output: Option<SafeOutputProjection>,
    #[serde(default)]
    pub last_tool_call: Option<SafeToolCallProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_output_order: Option<ActivityOrder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_tool_call_order: Option<ActivityOrder>,
    /// Turn of `last_output` / `last_tool_call` (the event's `params.turnId`),
    /// so a history revert can drop a preview of a removed turn
    /// ([`forget_reverted_turns`]). Absent in stores of older binaries and
    /// for events without a turn id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_output_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_tool_call_turn_id: Option<String>,
    /// The last turn that ended `failed` (capability `turn-error-surface`),
    /// cleared by a later `completed` turn, kept by an `interrupted` one.
    /// Absent in stores of older binaries (= never failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_error: Option<LastTurnError>,
}

/// Longest `lastTurnError.message` (characters).
pub const TURN_ERROR_MESSAGE_CHARS: usize = 2000;
/// Largest serialized `codexErrorInfo` kept; a larger one keeps its `code`.
const TURN_ERROR_INFO_BYTES: usize = 2048;
/// One owner notice per profile per rolling hour for the unauthorized family.
pub const TURN_ERROR_NOTICE_INTERVAL: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// `activity.lastTurnError` of a session (capability `turn-error-surface`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LastTurnError {
    #[serde(default)]
    pub turn_id: Option<String>,
    pub message: String,
    pub code: String,
    #[serde(default)]
    pub codex_error_info: serde_json::Value,
    pub at: String,
    pub consecutive: u64,
    /// Profile the failed turn ran on (the runtime's launch binding): counts
    /// the sessions of a profile for the owner notice. Store only, never in
    /// the session resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
}

impl LastTurnError {
    /// `code` is `unauthorized` or `refresh_token_reused`.
    pub fn is_unauthorized_family(&self) -> bool {
        is_unauthorized_code(&self.code)
    }

    fn profile_key(&self) -> Option<&str> {
        self.profile_id.as_deref().or(self.profile_name.as_deref())
    }

    /// The `activity.lastTurnError` object of the session resource.
    pub fn resource(&self) -> serde_json::Value {
        serde_json::json!({
            "turnId": self.turn_id,
            "message": self.message,
            "code": self.code,
            "codexErrorInfo": self.codex_error_info,
            "at": self.at,
            "consecutive": self.consecutive,
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        parse_timestamp(&self.at).context("invalid lastTurnError.at")?;
        anyhow::ensure!(self.consecutive >= 1 && self.consecutive <= MAX_SAFE_SEQUENCE,
            "lastTurnError.consecutive is outside 1..JSON-safe");
        anyhow::ensure!(self.message.chars().count() <= TURN_ERROR_MESSAGE_CHARS, "lastTurnError.message is too long");
        anyhow::ensure!(!self.code.is_empty() && self.code.len() <= 128, "invalid lastTurnError.code");
        Ok(())
    }
}

/// `unauthorized` or `refresh_token_reused`.
pub fn is_unauthorized_code(code: &str) -> bool {
    matches!(code, "unauthorized" | "refresh_token_reused")
}

/// The stable `code` of a failed turn's error (contract turn-error-surface):
/// `refresh_token_reused` (the message says the refresh token was already
/// used, or the info / details name `refresh_token_reused`), else
/// `unauthorized` (info `unauthorized` or an `httpStatusCode` 401), else
/// the info string, the single key of an info object, or `other`.
pub fn classify_turn_error(
    message: &str,
    codex_error_info: &serde_json::Value,
    additional_details: &serde_json::Value,
) -> String {
    use serde_json::Value;
    let names_reused =
        |value: &Value| !value.is_null() && value.to_string().to_ascii_lowercase().contains("refresh_token_reused");
    if message.to_lowercase().contains("refresh token was already used")
        || names_reused(codex_error_info)
        || names_reused(additional_details)
    {
        return "refresh_token_reused".into();
    }
    fn has_status_401(value: &Value) -> bool {
        match value {
            Value::Object(map) => map
                .iter()
                .any(|(key, inner)| (key == "httpStatusCode" && inner.as_u64() == Some(401)) || has_status_401(inner)),
            Value::Array(items) => items.iter().any(has_status_401),
            _ => false,
        }
    }
    if codex_error_info.as_str() == Some("unauthorized") || has_status_401(codex_error_info) {
        return "unauthorized".into();
    }
    let code = match codex_error_info {
        Value::String(code) => code.trim().to_string(),
        Value::Object(map) if map.len() == 1 => map.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    };
    if code.is_empty() || code.len() > 128 {
        "other".into()
    } else {
        code
    }
}

/// A `turn/completed` with `turn.status == "failed"`, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnFailure {
    turn_id: Option<String>,
    message: String,
    code: String,
    codex_error_info: serde_json::Value,
}

impl TurnFailure {
    fn from_turn(turn: &serde_json::Value) -> Self {
        use serde_json::Value;
        let error = turn.get("error").cloned().unwrap_or(Value::Null);
        let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
        let info = error.get("codexErrorInfo").cloned().unwrap_or(Value::Null);
        let details = error.get("additionalDetails").cloned().unwrap_or(Value::Null);
        let code = classify_turn_error(message, &info, &details);
        let info = if info.to_string().len() > TURN_ERROR_INFO_BYTES { Value::String(code.clone()) } else { info };
        Self {
            turn_id: turn.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string),
            message: message.chars().take(TURN_ERROR_MESSAGE_CHARS).collect(),
            code,
            codex_error_info: info,
        }
    }
}

/// An owner notice a failed turn of the unauthorized family asks for (at
/// most one per profile per rolling hour, [`TURN_ERROR_NOTICE_INTERVAL`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnErrorNotice {
    pub cutex_session_id: String,
    pub turn_id: Option<String>,
    pub code: String,
    pub consecutive: u64,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    /// Sessions of the profile whose `lastTurnError` is of the unauthorized
    /// family now (this one included).
    pub affected_sessions: usize,
}

impl TurnErrorNotice {
    /// The owner notice text (Chinese, as the other cutex notices).
    pub fn text(&self) -> String {
        let profile = self.profile_name.as_deref().or(self.profile_id.as_deref()).unwrap_or("（未知）");
        let what = if self.code == "refresh_token_reused" {
            "登录令牌刷新失败，是并发刷新冲突；下一条消息会自动重试。"
        } else {
            "登录未通过（unauthorized）；如果反复出现，可能需要重新登录这个配置。"
        };
        format!(
            "配置「{profile}」上的一轮对话失败：{what}这个配置下目前有 {} 个会话的上一轮因登录失败。",
            self.affected_sessions
        )
    }
}

impl SessionActivityState {
    /// When the agent last had real conversation activity (resource field
    /// `lastConversationAt`). The maintained `last_conversation_at` once
    /// written; before that (stores of older binaries) the newest of the
    /// older conversation fields, all fed by native turn / item events only.
    pub fn conversation_at(&self) -> Option<String> {
        if self.last_conversation_at.is_some() {
            return self.last_conversation_at.clone();
        }
        let mut newest: Option<(DateTime<chrono::FixedOffset>, &str)> = None;
        for value in [
            self.last_turn_started_at.as_deref(),
            self.last_output_at.as_deref(),
            self.last_output_completed_at.as_deref(),
            self.last_turn_completed_at.as_deref(),
            self.last_file_change_at.as_deref(),
            self.last_output.as_ref().map(|output| output.updated_at.as_str()),
            self.last_tool_call.as_ref().map(|tool| tool.updated_at.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(at) = parse_timestamp(value) {
                if newest.is_none_or(|(current, _)| at > current) {
                    newest = Some((at, value));
                }
            }
        }
        newest.map(|(_, value)| value.to_string())
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.revision > MAX_SAFE_SEQUENCE {
            anyhow::bail!("session activity revision exceeds the JSON-safe range");
        }
        if self
            .runtime_generation
            .is_some_and(|generation| generation == 0 || generation > MAX_SAFE_SEQUENCE)
        {
            anyhow::bail!("session activity runtime generation is outside the JSON-safe range");
        }
        for (name, value) in [
            ("lastOutputAt", self.last_output_at.as_deref()),
            (
                "lastOutputCompletedAt",
                self.last_output_completed_at.as_deref(),
            ),
            (
                "lastTurnCompletedAt",
                self.last_turn_completed_at.as_deref(),
            ),
            ("lastFileChangeAt", self.last_file_change_at.as_deref()),
            ("lastTurnStartedAt", self.last_turn_started_at.as_deref()),
            ("lastConversationAt", self.last_conversation_at.as_deref()),
        ] {
            if let Some(value) = value {
                parse_timestamp(value)
                    .with_context(|| format!("invalid session activity {name}"))?;
            }
        }
        if let Some(output) = self.last_output.as_ref() {
            output
                .validate()
                .context("invalid last output projection")?;
        }
        if let Some(tool) = self.last_tool_call.as_ref() {
            tool.validate()
                .context("invalid last tool-call projection")?;
        }
        if let Some(error) = self.last_turn_error.as_ref() {
            error.validate()?;
        }
        if self.last_output.is_some() != self.last_output_order.is_some()
            || self.last_tool_call.is_some() != self.last_tool_call_order.is_some()
        {
            anyhow::bail!("observability projection is missing its durable order fence");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ActivityOrder {
    runtime_generation: u64,
    stream_id: String,
    sequence: u64,
    received_at: String,
}

impl ActivityOrder {
    fn from_envelope(envelope: &EventEnvelope, runtime_generation: u64) -> Self {
        Self {
            runtime_generation,
            stream_id: envelope.stream_id.clone(),
            sequence: envelope.sequence,
            received_at: envelope.received_at.clone(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.runtime_generation == 0
            || self.runtime_generation > MAX_SAFE_SEQUENCE
            || self.sequence == 0
            || self.sequence > MAX_SAFE_SEQUENCE
            || self.stream_id.is_empty()
            || self.stream_id.len() > 512
        {
            anyhow::bail!("invalid session activity order fence");
        }
        parse_timestamp(&self.received_at)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(test)]
struct ActivityStore {
    #[serde(default = "activity_store_version")]
    version: u8,
    #[serde(default)]
    sessions: std::collections::BTreeMap<String, SessionActivityState>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    deleted_sessions: std::collections::BTreeSet<String>,
    /// Profile key (id, else name; "" = unknown) → `receivedAt` of the last
    /// unauthorized-family turn-error owner notice: the rolling-hour limit
    /// survives a restart. One entry per profile, older ones pruned.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    turn_error_notices: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
impl Default for ActivityStore {
    fn default() -> Self {
        Self {
            version: activity_store_version(),
            sessions: Default::default(),
            deleted_sessions: Default::default(),
            turn_error_notices: Default::default(),
        }
    }
}

#[cfg(test)]
fn activity_store_version() -> u8 {
    1
}

#[derive(Debug, Default)]
struct OutputCheckpoint {
    runtime_generation: Option<u64>,
    latest_at: Option<String>,
    last_attempt_at: Option<String>,
}

#[derive(Debug)]
struct ActivityRecorder {
    output_checkpoint_interval: Duration,
    output: Mutex<HashMap<String, OutputCheckpoint>>,
}

impl ActivityRecorder {
    fn new(output_checkpoint_interval: Duration) -> Self {
        Self {
            output_checkpoint_interval,
            output: Mutex::new(HashMap::new()),
        }
    }

    fn record_output_locked(
        &self, root: &Path, session: &str, generation: u64, at: &str,
        output: &mut HashMap<String, OutputCheckpoint>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(generation > 0 && generation <= MAX_SAFE_SEQUENCE,
            "activity event requires a positive JSON-safe runtime generation");
        parse_timestamp(at).context("invalid live activity timestamp")?;
        let checkpoint = output.entry(session.to_string()).or_default();
        match checkpoint.runtime_generation {
            Some(current) if generation < current => return Ok(()),
            Some(current) if generation > current => {
                *checkpoint = OutputCheckpoint { runtime_generation: Some(generation), ..Default::default() };
            }
            None => checkpoint.runtime_generation = Some(generation),
            Some(_) => {}
        }
        merge_timestamp(&mut checkpoint.latest_at, at)?;
        let latest = checkpoint.latest_at.as_deref().context("output checkpoint missing timestamp")?;
        if !checkpoint_due(checkpoint.last_attempt_at.as_deref(), latest, self.output_checkpoint_interval)? {
            return Ok(());
        }
        checkpoint.last_attempt_at = Some(latest.to_string());
        update_activity_at(root, session, generation, |state| {
            let changed = merge_timestamp(&mut state.last_output_at, latest)?;
            Ok(merge_timestamp(&mut state.last_conversation_at, latest)? | changed)
        })?;
        Ok(())
    }

    fn record_live_at(&self, root: &Path, pending: &super::model::PendingEvent, at: &str) -> anyhow::Result<()> {
        if pending.source != EventSource::AppServer { return Ok(()); }
        let Some(native) = &pending.native else { return Ok(()) };
        if native.kind != NativeMessageKind::Notification
            || native.message["method"] != "item/agentMessage/delta"
            || !native.message.pointer("/params/delta").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()) {
            return Ok(());
        }
        let generation = pending.correlation.runtime_generation.context("live activity missing runtime generation")?;
        let mut output = self.output.lock().map_err(|_| anyhow::anyhow!("session activity recorder lock was poisoned"))?;
        self.record_output_locked(root, &pending.cutex_session_id, generation, at, &mut output)
    }

    #[cfg(test)]
    fn record_at(&self, root: &Path, envelope: &EventEnvelope) -> anyhow::Result<()> {
        self.record_at_for(root, envelope, None, None).map(|_| ())
    }

    /// Records the event; `profile_id` / `profile_name` = the profile the
    /// runtime runs on (kept with a failed turn's `lastTurnError`). Returns
    /// the owner notice a failed turn of the unauthorized family asks for,
    /// already counted against its profile's rolling hour.
    fn record_at_for(
        &self,
        root: &Path,
        envelope: &EventEnvelope,
        profile_id: Option<&str>,
        profile_name: Option<&str>,
    ) -> anyhow::Result<Option<TurnErrorNotice>> {
        let Some(event) = activity_event(envelope) else {
            return Ok(None);
        };
        let mut notice = None;
        let runtime_generation = envelope
            .correlation
            .runtime_generation
            .filter(|generation| *generation > 0 && *generation <= MAX_SAFE_SEQUENCE)
            .context("activity event requires a positive JSON-safe runtime generation")?;
        parse_timestamp(&envelope.received_at).context("invalid activity event receivedAt")?;

        let mut output = self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("session activity recorder lock was poisoned"))?;
        match event {
            ActivityEvent::OutputDelta => {
                self.record_output_locked(root, &envelope.cutex_session_id, runtime_generation,
                    &envelope.received_at, &mut output)?;
            }
            ActivityEvent::OutputCompleted {
                has_output,
                class,
                display_text,
            } => {
                let pending_at = output
                    .get(&envelope.cutex_session_id)
                    .filter(|checkpoint| checkpoint.runtime_generation == Some(runtime_generation))
                    .and_then(|checkpoint| checkpoint.latest_at.clone());
                let output_at =
                    pending_at.or_else(|| has_output.then(|| envelope.received_at.clone()));
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let mut changed = merge_optional_timestamp(
                            &mut state.last_output_at,
                            output_at.as_deref(),
                        )?;
                        changed |= merge_timestamp(
                            &mut state.last_output_completed_at,
                            &envelope.received_at,
                        )?;
                        changed |= merge_timestamp(
                            &mut state.last_conversation_at,
                            &envelope.received_at,
                        )?;
                        if let Some(display_text) = display_text.as_deref() {
                            changed |= update_last_output(
                                state,
                                envelope,
                                runtime_generation,
                                class,
                                display_text,
                            )?;
                        }
                        Ok(changed)
                    },
                )?;
                clear_completed_output_checkpoint(
                    &mut output,
                    &envelope.cutex_session_id,
                    runtime_generation,
                );
            }
            ActivityEvent::TurnStarted => {
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let changed =
                            merge_timestamp(&mut state.last_turn_started_at, &envelope.received_at)?;
                        Ok(merge_timestamp(&mut state.last_conversation_at, &envelope.received_at)?
                            | changed)
                    },
                )?;
            }
            ActivityEvent::TurnCompleted { interrupted, completed, failure } => {
                let pending_at = output
                    .get(&envelope.cutex_session_id)
                    .filter(|checkpoint| checkpoint.runtime_generation == Some(runtime_generation))
                    .and_then(|checkpoint| checkpoint.latest_at.clone());
                let failure = failure.map(|mut failure| {
                    if failure.turn_id.is_none() {
                        failure.turn_id = event_turn_id(envelope);
                    }
                    failure
                });
                let profile = (profile_id.map(str::trim).filter(|id| !id.is_empty()).map(str::to_string),
                    profile_name.map(str::trim).filter(|name| !name.is_empty()).map(str::to_string));
                notice = update_activity_with_store_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        // An older turn end (redelivered, out of order) does
                        // not move the turn error of a newer one.
                        let newest = match state.last_turn_completed_at.as_deref() {
                            Some(current) => parse_timestamp(&envelope.received_at)? >= parse_timestamp(current)?,
                            None => true,
                        };
                        let mut changed = false;
                        let mut recorded = None;
                        if newest {
                            let (error_changed, fresh) = apply_turn_outcome(
                                state, completed, failure.as_ref(), &envelope.received_at, profile)?;
                            changed |= error_changed;
                            recorded = fresh;
                        }
                        changed |= merge_optional_timestamp(
                            &mut state.last_output_at,
                            pending_at.as_deref(),
                        )?;
                        changed |= merge_timestamp(
                            &mut state.last_turn_completed_at,
                            &envelope.received_at,
                        )?;
                        if state.last_turn_completed_at.as_deref() == Some(envelope.received_at.as_str())
                            && state.last_turn_completed_generation != Some(runtime_generation) {
                            state.last_turn_completed_generation = Some(runtime_generation);
                            changed = true;
                        }
                        // Output the turn produced is conversation; its end
                        // only when not interrupted: a resumed runtime
                        // interrupts a turn left in progress by the restart.
                        changed |= merge_optional_timestamp(
                            &mut state.last_conversation_at,
                            pending_at.as_deref(),
                        )?;
                        if !interrupted {
                            changed |= merge_timestamp(
                                &mut state.last_conversation_at,
                                &envelope.received_at,
                            )?;
                        }
                        Ok((changed, recorded))
                    },
                    |store, recorded| match recorded {
                        Some(recorded) if recorded.is_unauthorized_family() => {
                            decide_turn_error_notice(store, &envelope.cutex_session_id, &recorded)
                        }
                        _ => Ok((false, None)),
                    },
                )?;
                clear_completed_output_checkpoint(
                    &mut output,
                    &envelope.cutex_session_id,
                    runtime_generation,
                );
            }
            ActivityEvent::FileChangeCompleted => {
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let mut changed =
                            merge_timestamp(&mut state.last_file_change_at, &envelope.received_at)?;
                        changed |=
                            merge_timestamp(&mut state.last_conversation_at, &envelope.received_at)?;
                        changed |= update_last_tool_call(
                            state,
                            envelope,
                            runtime_generation,
                            SafeToolCallClass::FileChange,
                            SafeToolCallStatus::Finished,
                        )?;
                        Ok(changed)
                    },
                )?;
            }
            ActivityEvent::ToolLifecycle { class, status } => {
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let changed = update_last_tool_call(
                            state,
                            envelope,
                            runtime_generation,
                            class,
                            status,
                        )?;
                        Ok(merge_timestamp(&mut state.last_conversation_at, &envelope.received_at)?
                            | changed)
                    },
                )?;
            }
        }
        Ok(notice)
    }
}

/// `lastTurnError` after a turn end (contract turn-error-surface): a
/// `completed` turn clears it, a `failed` one sets it (`consecutive` counts
/// the failed turns since the last completed one; the same turn reported
/// again keeps its count), any other end leaves it. Returns (changed, the
/// error of a newly failed turn).
fn apply_turn_outcome(
    state: &mut SessionActivityState,
    completed: bool,
    failure: Option<&TurnFailure>,
    at: &str,
    (profile_id, profile_name): (Option<String>, Option<String>),
) -> anyhow::Result<(bool, Option<LastTurnError>)> {
    if completed {
        return Ok((state.last_turn_error.take().is_some(), None));
    }
    let Some(failure) = failure else {
        return Ok((false, None));
    };
    let previous = state.last_turn_error.as_ref();
    let same_turn = previous.is_some_and(|previous| previous.turn_id.is_some() && previous.turn_id == failure.turn_id);
    let consecutive = match previous {
        Some(previous) if same_turn => previous.consecutive,
        Some(previous) => previous.consecutive.saturating_add(1).min(MAX_SAFE_SEQUENCE),
        None => 1,
    };
    // The same turn reported again keeps when it failed and, when the
    // redelivery does not name one, the profile it ran on.
    let kept = previous.filter(|_| same_turn);
    let error = LastTurnError {
        turn_id: failure.turn_id.clone(),
        message: failure.message.clone(),
        code: failure.code.clone(),
        codex_error_info: failure.codex_error_info.clone(),
        at: kept.map_or_else(|| at.to_string(), |kept| kept.at.clone()),
        consecutive,
        profile_id: profile_id.or_else(|| kept.and_then(|kept| kept.profile_id.clone())),
        profile_name: profile_name.or_else(|| kept.and_then(|kept| kept.profile_name.clone())),
    };
    error.validate()?;
    if same_turn && previous == Some(&error) {
        return Ok((false, None));
    }
    state.last_turn_error = Some(error.clone());
    Ok((true, (!same_turn).then_some(error)))
}

/// The owner notice for `recorded` (unauthorized family) of session `id`,
/// unless its profile had one within [`TURN_ERROR_NOTICE_INTERVAL`] (by the
/// events' `receivedAt`). Returns (store changed, notice).
fn decide_turn_error_notice(
    store: &rusqlite::Connection,
    id: &str,
    recorded: &LastTurnError,
) -> anyhow::Result<(bool, Option<TurnErrorNotice>)> {
    let now = parse_timestamp(&recorded.at)?;
    let key = recorded.profile_key().unwrap_or_default().to_string();
    // This metadata is bounded by profiles, independent of session history.
    // Use chrono rather than SQLite Julian floats to preserve subsecond fences.
    let cutoff = (now - TURN_ERROR_NOTICE_INTERVAL).to_rfc3339();
    let expired = {
        // SQL selects expiry candidates; chrono below decides the exact fence.
        // One second of slack covers SQLite's coarser timestamp precision.
        let mut statement=store.prepare("SELECT profile,at FROM turn_error_notices WHERE julianday(at) IS NULL OR julianday(at)<=julianday(?1,'+1 second')")?;
        let rows=statement.query_map([&cutoff], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().filter(|(_,at)| !parse_timestamp(at).is_ok_and(|last|
            now.signed_duration_since(last) < TURN_ERROR_NOTICE_INTERVAL)).map(|(key,_)|key).collect::<Vec<_>>()
    };
    for key in expired { store.execute("DELETE FROM turn_error_notices WHERE profile=?1", [&key])?; }
    // RFC3339 timestamps are compared by instant, including legacy offsets.
    let previous: Option<String> = {
        use rusqlite::OptionalExtension;
        store.query_row("SELECT at FROM turn_error_notices WHERE profile=?1", [&key], |r| r.get(0)).optional()?
    };
    if previous.as_deref().and_then(|at| parse_timestamp(at).ok())
        .is_some_and(|last| now.signed_duration_since(last) < TURN_ERROR_NOTICE_INTERVAL) {
        return Ok((false, None));
    }
    store.execute("INSERT INTO turn_error_notices VALUES(?1,?2) ON CONFLICT(profile) DO UPDATE SET at=excluded.at", rusqlite::params![key, recorded.at])?;
    let affected_sessions: i64 = store.query_row(
        "SELECT count(*) FROM session_rows WHERE deleted=0 AND json_extract(CAST(data AS TEXT),'$.lastTurnError.code') IN ('unauthorized','refresh_token_reused') AND coalesce(json_extract(CAST(data AS TEXT),'$.lastTurnError.profileId'),json_extract(CAST(data AS TEXT),'$.lastTurnError.profileName'),'')=?1",
        [&key], |r| r.get(0))?;
    Ok((true, Some(TurnErrorNotice {
        cutex_session_id: id.to_string(),
        turn_id: recorded.turn_id.clone(),
        code: recorded.code.clone(),
        consecutive: recorded.consecutive,
        profile_id: recorded.profile_id.clone(),
        profile_name: recorded.profile_name.clone(),
        affected_sessions: usize::try_from(affected_sessions)?,
    })))
}

/// Owner notice kind `system.turnAuthFailed` (notification level tables).
pub const TURN_ERROR_NOTICE_KIND: &str = "turnAuthFailed";

fn send_turn_error_notice(notice: TurnErrorNotice) {
    let session = notice.cutex_session_id.clone();
    // Off the runtime event path: the owner notify store and the event
    // journal are not touched while an event completes.
    let spawned = std::thread::Builder::new().name("cutex-turn-error-notice".into()).spawn(move || {
        crate::documents::notify_owner_for_turn(
            TURN_ERROR_NOTICE_KIND,
            &notice.cutex_session_id,
            None,
            notice.text(),
            None,
            notice.turn_id.clone(),
        );
    });
    if let Err(error) = spawned {
        eprintln!("turn error notice for {session} not sent: {error}");
    }
}

fn clear_completed_output_checkpoint(
    output: &mut HashMap<String, OutputCheckpoint>,
    cutex_session_id: &str,
    runtime_generation: u64,
) {
    if output
        .get(cutex_session_id)
        .and_then(|checkpoint| checkpoint.runtime_generation)
        .is_some_and(|pending_generation| pending_generation <= runtime_generation)
    {
        output.remove(cutex_session_id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActivityEvent {
    OutputDelta,
    OutputCompleted {
        has_output: bool,
        class: SafeOutputClass,
        display_text: Option<String>,
    },
    TurnStarted,
    TurnCompleted {
        interrupted: bool,
        /// `turn.status == "completed"`: clears `lastTurnError`.
        completed: bool,
        /// `turn.status == "failed"`: sets `lastTurnError`.
        failure: Option<TurnFailure>,
    },
    FileChangeCompleted,
    ToolLifecycle {
        class: SafeToolCallClass,
        status: SafeToolCallStatus,
    },
}

pub fn record_activity_event(envelope: &EventEnvelope) -> anyhow::Result<()> {
    record_activity_event_for_profile(envelope, None, None)
}

/// As [`record_activity_event`] for a runtime launched on the profile
/// `profile_id` / `profile_name`: a turn that fails with an unauthorized-family
/// error also notifies the owner (at most once per profile per rolling hour).
pub fn record_activity_event_for_profile(
    envelope: &EventEnvelope,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    let notice = ACTIVITY_RECORDER
        .get_or_init(|| ActivityRecorder::new(OUTPUT_CHECKPOINT_INTERVAL))
        .record_at_for(&root, envelope, profile_id, profile_name)?;
    if let Some(notice) = notice {
        send_turn_error_notice(notice);
    }
    Ok(())
}

/// A passive output signal shares the existing one-second checkpoint cadence;
/// it has no journal sequence/cursor and never creates an output preview.
pub fn record_live_output_signal(pending: &super::model::PendingEvent) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    ACTIVITY_RECORDER.get_or_init(|| ActivityRecorder::new(OUTPUT_CHECKPOINT_INTERVAL))
        .record_live_at(&root, pending, &chrono::Utc::now().to_rfc3339())
}

pub(super) fn activity_repository(root: &Path) -> anyhow::Result<super::session_row_store::Repository<SessionActivityState>> {
    super::session_row_store::Repository::open(root, "session-activity", ACTIVITY_STATE_FILE, SessionActivityState::validate)
}

/// Kit entry point. The CLI requires --daemon-stopped; domains switch separately.
pub fn import_session_rows(store: &str) -> anyhow::Result<()> {
    let root=runtime_dir()?.join("management-v2");
    match store {
        "session-activity" => {activity_repository(&root)?;},
        "session-projection" => {super::session::projection_repository(&root)?;},
        _ => anyhow::bail!("unknown S7 session store"),
    }
    Ok(())
}

/// Point read: unrelated sessions are never parsed or materialized.
pub fn session_activity(id: &str) -> anyhow::Result<Option<SessionActivityState>> {
    activity_repository(&runtime_dir()?.join("management-v2"))?.get(id)
}

fn activity_event(envelope: &EventEnvelope) -> Option<ActivityEvent> {
    if envelope.source != EventSource::AppServer {
        return None;
    }
    let native = envelope.native.as_ref()?;
    if native.kind != NativeMessageKind::Notification {
        return None;
    }
    let method = native.message.get("method")?.as_str()?;
    let params = native.message.get("params")?;
    match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|delta| !delta.is_empty())
            .then_some(ActivityEvent::OutputDelta),
        "item/completed" => {
            let item = params.get("item")?;
            match item.get("type").and_then(serde_json::Value::as_str)? {
                "agentMessage" => Some(ActivityEvent::OutputCompleted {
                    has_output: item
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    class: output_class(item.get("phase").and_then(serde_json::Value::as_str)),
                    display_text: item
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .and_then(sanitize_visible_output),
                }),
                "fileChange"
                    if item.get("status").and_then(serde_json::Value::as_str)
                        == Some("completed")
                        && item
                            .get("changes")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|changes| !changes.is_empty()) =>
                {
                    Some(ActivityEvent::FileChangeCompleted)
                }
                "fileChange" => None,
                item_type => tool_class(item_type).map(|class| ActivityEvent::ToolLifecycle {
                    class,
                    status: completed_tool_status(
                        item.get("status").and_then(serde_json::Value::as_str),
                    ),
                }),
            }
        }
        "item/started" => {
            let item = params.get("item")?;
            match item.get("type").and_then(serde_json::Value::as_str)? {
                item_type => tool_class(item_type).map(|class| ActivityEvent::ToolLifecycle {
                    class,
                    status: SafeToolCallStatus::Started,
                }),
            }
        }
        "item/commandExecution/outputDelta" => Some(ActivityEvent::ToolLifecycle {
            class: SafeToolCallClass::Command,
            status: SafeToolCallStatus::Progress,
        }),
        "item/mcpToolCall/progress" => Some(ActivityEvent::ToolLifecycle {
            class: SafeToolCallClass::McpTool,
            status: SafeToolCallStatus::Progress,
        }),
        "item/dynamicToolCall/progress" => Some(ActivityEvent::ToolLifecycle {
            class: SafeToolCallClass::DynamicTool,
            status: SafeToolCallStatus::Progress,
        }),
        "item/collabAgentToolCall/progress" => Some(ActivityEvent::ToolLifecycle {
            class: SafeToolCallClass::CollaborationTool,
            status: SafeToolCallStatus::Progress,
        }),
        "turn/started" => Some(ActivityEvent::TurnStarted),
        "turn/completed" => {
            let status = params.pointer("/turn/status").and_then(serde_json::Value::as_str);
            Some(ActivityEvent::TurnCompleted {
                interrupted: status == Some("interrupted"),
                completed: status == Some("completed"),
                failure: (status == Some("failed"))
                    .then(|| params.get("turn").map(TurnFailure::from_turn))
                    .flatten(),
            })
        }
        _ => None,
    }
}

fn output_class(phase: Option<&str>) -> SafeOutputClass {
    match phase {
        // `partial_answer` (native r52+, set by the Responses API): stable
        // answer text that may be followed by more output or tools. Owner
        // decision D3: progress, like commentary; only `final_answer` is the
        // visible final reply.
        Some("commentary" | "partial_answer") => SafeOutputClass::Progress,
        Some("final_answer") => SafeOutputClass::FinalVisible,
        _ => SafeOutputClass::Unclassified,
    }
}

fn tool_class(item_type: &str) -> Option<SafeToolCallClass> {
    Some(match item_type {
        "commandExecution" => SafeToolCallClass::Command,
        "mcpToolCall" => SafeToolCallClass::McpTool,
        "dynamicToolCall" => SafeToolCallClass::DynamicTool,
        "collabAgentToolCall" => SafeToolCallClass::CollaborationTool,
        "fileChange" => SafeToolCallClass::FileChange,
        "imageView" => SafeToolCallClass::ImageView,
        _ => return None,
    })
}

fn completed_tool_status(status: Option<&str>) -> SafeToolCallStatus {
    match status {
        Some("failed" | "declined" | "cancelled" | "error") => SafeToolCallStatus::Failed,
        Some("inProgress" | "in_progress" | "running") => SafeToolCallStatus::Progress,
        // `item/completed` itself is authoritative completion when older
        // producers omit the redundant item status.
        _ => SafeToolCallStatus::Finished,
    }
}

fn update_last_output(
    state: &mut SessionActivityState,
    envelope: &EventEnvelope,
    runtime_generation: u64,
    class: SafeOutputClass,
    display_text: &str,
) -> anyhow::Result<bool> {
    let order = ActivityOrder::from_envelope(envelope, runtime_generation);
    if let Some(current) = state.last_output_order.as_ref() {
        if !activity_order_is_newer(&order, current)? {
            return Ok(false);
        }
    }
    let projection = SafeOutputProjection {
        association: observation_association(envelope),
        class,
        display_text: display_text.to_string(),
        updated_at: envelope.received_at.clone(),
        runtime_generation,
    };
    projection.validate()?;
    order.validate()?;
    state.last_output = Some(projection);
    state.last_output_order = Some(order);
    state.last_output_turn_id = event_turn_id(envelope);
    Ok(true)
}

fn update_last_tool_call(
    state: &mut SessionActivityState,
    envelope: &EventEnvelope,
    runtime_generation: u64,
    class: SafeToolCallClass,
    status: SafeToolCallStatus,
) -> anyhow::Result<bool> {
    let order = ActivityOrder::from_envelope(envelope, runtime_generation);
    if let Some(current) = state.last_tool_call_order.as_ref() {
        if !activity_order_is_newer(&order, current)? {
            return Ok(false);
        }
    }
    let projection = SafeToolCallProjection {
        association: observation_association(envelope),
        class,
        status,
        display_text: class.display_text().to_string(),
        updated_at: envelope.received_at.clone(),
        runtime_generation,
    };
    projection.validate()?;
    order.validate()?;
    state.last_tool_call = Some(projection);
    state.last_tool_call_order = Some(order);
    state.last_tool_call_turn_id = event_turn_id(envelope);
    Ok(true)
}

/// The native turn of an item event (correlation, else `params.turnId`), if any.
fn event_turn_id(envelope: &EventEnvelope) -> Option<String> {
    envelope
        .correlation
        .turn_id
        .as_deref()
        .or_else(|| envelope.native.as_ref()?.message.pointer("/params/turnId")?.as_str())
        .filter(|turn| !turn.is_empty())
        .map(str::to_string)
}

/// After a history revert removed `removed_turn_ids`: `lastOutput` /
/// `lastToolCall` must not show what the agent no longer said or did. A
/// preview of a removed turn, or of an unknown turn (stores written before
/// turn ids were kept), is cleared; the next output / tool call of the
/// remaining conversation sets it again. Timestamps (`lastOutputAt`,
/// `lastConversationAt`, ...) stay: they describe when the agent was active,
/// which a revert does not undo. History items carry no time of their own,
/// so the previews are not recomputed from the remaining turns.
pub fn forget_reverted_turns(cutex_session_id: &str, removed_turn_ids: &[&str]) -> anyhow::Result<()> {
    forget_reverted_turns_at(&runtime_dir()?.join("management-v2"), cutex_session_id, removed_turn_ids)
}

fn forget_reverted_turns_at(root: &Path, id: &str, removed_turn_ids: &[&str]) -> anyhow::Result<()> {
    if removed_turn_ids.is_empty() {
        return Ok(());
    }
    let removed = |turn: &Option<String>| turn.as_deref().is_none_or(|turn| removed_turn_ids.contains(&turn));
    activity_repository(root)?.update(id, |_, row| {
        let Some(state) = row.as_mut() else { return Ok(()); };
        let mut changed = false;
        if state.last_output.is_some() && removed(&state.last_output_turn_id) {
            (state.last_output, state.last_output_order, state.last_output_turn_id) = (None, None, None);
            changed = true;
        }
        if state.last_tool_call.is_some() && removed(&state.last_tool_call_turn_id) {
            (state.last_tool_call, state.last_tool_call_order, state.last_tool_call_turn_id) = (None, None, None);
            changed = true;
        }
        // A failed turn the revert removed has nothing left to show.
        if state
            .last_turn_error
            .as_ref()
            .and_then(|error| error.turn_id.as_deref())
            .is_some_and(|turn| removed_turn_ids.contains(&turn))
        {
            state.last_turn_error = None;
            changed = true;
        }
        if changed {
            state.revision = state
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_SEQUENCE)
                .context("session activity revision exhausted")?;
            state.validate()?;

        }
        Ok(())
    })
}

fn observation_association(envelope: &EventEnvelope) -> ObservationAssociation {
    // r219: the Task Service is gone, so live activity is always session-scoped.
    ObservationAssociation::session(envelope.cutex_session_id.clone())
}

fn activity_order_is_newer(
    candidate: &ActivityOrder,
    current: &ActivityOrder,
) -> anyhow::Result<bool> {
    candidate.validate()?;
    current.validate()?;
    if candidate.runtime_generation != current.runtime_generation {
        return Ok(candidate.runtime_generation > current.runtime_generation);
    }
    if candidate.stream_id == current.stream_id {
        return Ok(candidate.sequence > current.sequence);
    }
    Ok(parse_timestamp(&candidate.received_at)? > parse_timestamp(&current.received_at)?)
}

fn checkpoint_due(
    checkpoint_at: Option<&str>,
    latest_at: &str,
    interval: Duration,
) -> anyhow::Result<bool> {
    let Some(checkpoint_at) = checkpoint_at else {
        return Ok(true);
    };
    let checkpoint_at = parse_timestamp(checkpoint_at)?;
    let latest_at = parse_timestamp(latest_at)?;
    let elapsed = latest_at.signed_duration_since(checkpoint_at);
    Ok(elapsed.to_std().is_ok_and(|elapsed| elapsed >= interval))
}

fn merge_optional_timestamp(
    current: &mut Option<String>,
    candidate: Option<&str>,
) -> anyhow::Result<bool> {
    match candidate {
        Some(candidate) => merge_timestamp(current, candidate),
        None => Ok(false),
    }
}

fn merge_timestamp(current: &mut Option<String>, candidate: &str) -> anyhow::Result<bool> {
    let candidate_time = parse_timestamp(candidate)?;
    if let Some(value) = current {
        if candidate_time <= parse_timestamp(value)? {
            return Ok(false);
        }
    }
    *current = Some(candidate.to_string());
    Ok(true)
}

fn parse_timestamp(value: &str) -> anyhow::Result<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(value).map_err(anyhow::Error::from)
}

fn update_activity_at(
    root: &Path,
    cutex_session_id: &str,
    runtime_generation: u64,
    update: impl FnOnce(&mut SessionActivityState) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    update_activity_with_store_at(
        root,
        cutex_session_id,
        runtime_generation,
        |state| Ok((update(state)?, ())),
        |_, ()| Ok((false, None::<()>)),
    )
    .map(|_| ())
}

/// As [`update_activity_at`]; `then` runs after the session's update in the
/// same locked read-modify-write (store-level state such as the turn-error
/// notice limit) and returns (changed, result). `None` when the event was
/// fenced off (deleted session, older runtime generation).
fn update_activity_with_store_at<U, T>(
    root: &Path,
    cutex_session_id: &str,
    runtime_generation: u64,
    update: impl FnOnce(&mut SessionActivityState) -> anyhow::Result<(bool, U)>,
    then: impl FnOnce(&rusqlite::Connection, U) -> anyhow::Result<(bool, Option<T>)>,
) -> anyhow::Result<Option<T>> {
    if cutex_session_id.is_empty() {
        anyhow::bail!("cutexSessionId must not be empty for session activity");
    }
    activity_repository(root)?.update_then(cutex_session_id, |deleted, row| {
        if deleted { return Ok(None); }
        let state = row.get_or_insert_with(Default::default);
        if state
            .runtime_generation
            .is_some_and(|current| runtime_generation < current)
        {
            return Ok(None);
        }
        let generation_changed = state.runtime_generation != Some(runtime_generation);
        if generation_changed {
            state.runtime_generation = Some(runtime_generation);
        }
        let (updated, carried) = update(state)?;
        let state_changed = updated || generation_changed;
        if state_changed {
            state.revision = state
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_SEQUENCE)
                .context("session activity revision exhausted")?;
            state.validate()?;

        }
        Ok(Some(carried))
    }, |sql, carried| match carried {
        Some(carried) => then(sql, carried).map(|(_, result)| result),
        None => Ok(None),
    })
}

/// Remove a deleted session's rebuildable projection, never its history.
#[cfg(test)]
pub(crate) fn write_activity_test_row(root: &Path, id: &str, state: SessionActivityState) -> anyhow::Result<()> {
    activity_repository(root)?.update(id, |deleted, row| {
        anyhow::ensure!(!deleted, "test fixture cannot replace a deleted activity row");
        *row = Some(state);
        Ok(())
    })
}

pub fn forget_deleted_session(cutex_session_id: &str) -> anyhow::Result<()> {
    forget_deleted_session_at(&runtime_dir()?.join("management-v2"), cutex_session_id)
}

fn forget_deleted_session_at(root: &Path, id: &str) -> anyhow::Result<()> {
    activity_repository(root)?.forget(id)
}

// Explicit complete selector listing. SQL transport is keyset/byte bounded;
// point API and scheduler call sites use session_activity instead.
#[cfg(test)]
fn load_session_activity_states_at(root: &Path) -> anyhow::Result<HashMap<String, SessionActivityState>> {
    let repo = activity_repository(root)?;
    let mut result = HashMap::new();
    let mut after = String::new();
    loop {
        let page = repo.page(&after, 200, 256 * 1024)?;
        if page.is_empty() { break; }
        for (id, row) in page {
            after = id.clone();
            if let Some(row) = row { result.insert(id, row); }
        }
    }
    Ok(result)
}

#[cfg(test)]
fn persisted_activity_json(root: &Path) -> Vec<u8> {
    let repo = activity_repository(root).unwrap();
    let mut store = ActivityStore::default();
    let mut after = String::new();
    loop {
        let page = repo.page(&after,200,256*1024).unwrap();
        if page.is_empty() {break;}
        for (id,row) in page {
            after=id.clone();
            if let Some(row)=row {store.sessions.insert(id,row);} else {store.deleted_sessions.insert(id);}
        }
    }
    serde_json::to_vec(&store).unwrap()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::management::v2::model::EventCorrelation;
    use crate::management::v2::model::NativeMessage;
    use crate::management::v2::model::CONTRACT_VERSION;

    fn test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cutex-activity-{label}-{}", Uuid::new_v4()))
    }

    fn envelope(received_at: &str, method: &str, params: serde_json::Value) -> EventEnvelope {
        envelope_at_generation(received_at, method, params, 1)
    }

    fn envelope_at_generation(
        received_at: &str,
        method: &str,
        params: serde_json::Value,
        runtime_generation: u64,
    ) -> EventEnvelope {
        EventEnvelope {
            contract_version: CONTRACT_VERSION,
            event_id: Uuid::new_v4().to_string(),
            cursor: "stream:1".to_string(),
            stream_id: "stream".to_string(),
            sequence: 1,
            received_at: received_at.to_string(),
            cutex_session_id: "cutex.session-a".to_string(),
            host_id: "test-host".to_string(),
            source: EventSource::AppServer,
            sensitivity: "owner".to_string(),
            schema: None,
            correlation: EventCorrelation {
                runtime_generation: Some(runtime_generation),
                ..EventCorrelation::default()
            },
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({ "method": method, "params": params }),
            }),
            cutex: None,
        }
    }

    #[test]
    fn live_output_checkpoints_once_per_interval_and_final_flushes_latest_signal() {
        let root = test_root("live-output");
        let recorder = ActivityRecorder::new(Duration::from_secs(1));
        let pending = super::super::model::PendingEvent {
            cutex_session_id: "cutex.session-a".into(), host_id: "test".into(), source: EventSource::AppServer,
            schema: None, correlation: EventCorrelation {runtime_generation:Some(1),..Default::default()},
            native: Some(NativeMessage {kind:NativeMessageKind::Notification,message:json!({"method":"item/agentMessage/delta","params":{"delta":"汉🙂"}})}),cutex:None,
        };
        recorder.record_live_at(&root, &pending, "2026-10-25T00:00:00.000Z").unwrap();
        let first = persisted_activity_json(&root);
        for millis in 1..1000 {
            recorder.record_live_at(&root, &pending, &format!("2026-10-25T00:00:00.{millis:03}Z")).unwrap();
        }
        assert_eq!(persisted_activity_json(&root), first,
            "999 live fragments must not cause 999 durable writes");
        recorder.record_at(&root, &envelope("2026-10-25T00:00:01.100Z", "item/completed",
            json!({"threadId":"thread-1","item":{"id":"m","type":"agentMessage","text":"full 汉🙂"}}))).unwrap();
        let state = load_session_activity_states_at(&root).unwrap().remove("cutex.session-a").unwrap();
        assert_eq!(state.last_output_at.as_deref(), Some("2026-10-25T00:00:00.999Z"));
        assert_eq!(state.last_output_completed_at.as_deref(), Some("2026-10-25T00:00:01.100Z"));
        assert!(state.last_output.is_some(), "only the durable completion creates a preview");
        let after_final = persisted_activity_json(&root);
        let mut reasoning = pending;
        reasoning.native.as_mut().unwrap().message["method"] = json!("item/reasoning/textDelta");
        recorder.record_live_at(&root, &reasoning, "2026-10-25T00:00:10Z").unwrap();
        assert_eq!(persisted_activity_json(&root), after_final);
        fs::remove_dir_all(root).unwrap();
    }

    fn correlated_envelope(
        received_at: &str,
        method: &str,
        params: serde_json::Value,
        runtime_generation: u64,
        stream_id: &str,
        sequence: u64,
        turn_id: &str,
    ) -> EventEnvelope {
        let mut envelope = envelope_at_generation(received_at, method, params, runtime_generation);
        envelope.stream_id = stream_id.to_string();
        envelope.sequence = sequence;
        envelope.correlation.thread_id = Some("thread-1".to_string());
        envelope.correlation.turn_id = Some(turn_id.to_string());
        envelope.correlation.item_id = Some(format!("item-{sequence}"));
        envelope
    }

    /// C-7 (r265): a history revert drops the previews of removed turns only.
    #[test]
    fn a_revert_clears_previews_of_removed_turns_and_keeps_the_rest() {
        let root = test_root("revert");
        let recorder = ActivityRecorder::new(Duration::from_secs(1));
        let item = |at: &str, seq: u64, turn: &str, item: serde_json::Value| {
            correlated_envelope(at, "item/completed", json!({"threadId": "thread-1", "turnId": turn, "item": item}), 1, "s", seq, turn)
        };
        recorder.record_at(&root, &item("2026-10-27T00:00:01Z", 1, "turn-1",
            json!({"id": "c1", "type": "commandExecution", "status": "completed"}))).unwrap();
        recorder.record_at(&root, &item("2026-10-27T00:00:02Z", 2, "turn-2",
            json!({"id": "m2", "type": "agentMessage", "text": "second answer"}))).unwrap();
        let state = |root: &Path| load_session_activity_states_at(root).unwrap().remove("cutex.session-a").unwrap();
        let before = state(&root);
        assert_eq!(before.last_output_turn_id.as_deref(), Some("turn-2"));
        assert_eq!(before.last_tool_call_turn_id.as_deref(), Some("turn-1"));
        // Removing another turn changes nothing (no write either).
        forget_reverted_turns_at(&root, "cutex.session-a", &["turn-9"]).unwrap();
        assert_eq!(state(&root), before);
        forget_reverted_turns_at(&root, "cutex.session-a", &[]).unwrap();
        assert_eq!(state(&root), before);
        // Reverting turn 2 clears the output preview; turn 1's tool call stays.
        forget_reverted_turns_at(&root, "cutex.session-a", &["turn-2"]).unwrap();
        let after = state(&root);
        assert!(after.last_output.is_none() && after.last_output_order.is_none());
        assert_eq!(after.last_tool_call, before.last_tool_call);
        assert_eq!(after.last_output_at, before.last_output_at, "activity times stay");
        assert_eq!(after.revision, before.revision + 1);
        // The next output after the revert is shown again (the order fence was reset).
        recorder.record_at(&root, &item("2026-10-27T00:00:03Z", 1, "turn-2b",
            json!({"id": "m3", "type": "agentMessage", "text": "new answer"}))).unwrap();
        assert_eq!(state(&root).last_output.unwrap().display_text, "new answer");
        // A preview of an unknown turn (older store) is cleared by any revert.
        activity_repository(&root).unwrap().update("cutex.session-a", |_, row| {row.as_mut().unwrap().last_tool_call_turn_id = None; Ok(())}).unwrap();
        forget_reverted_turns_at(&root, "cutex.session-a", &["turn-7"]).unwrap();
        let last = state(&root);
        assert!(last.last_tool_call.is_none());
        assert_eq!(last.last_output.unwrap().display_text, "new answer");
        // Unknown sessions are a no-op.
        forget_reverted_turns_at(&root, "cutex.other", &["turn-1"]).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleted_session_projection_cleanup_keeps_other_sessions() {
        let root = test_root("deleted-session");
        let mut store = ActivityStore::default();
        store.sessions.insert("gone".into(), SessionActivityState::default());
        store.sessions.insert("keep".into(), SessionActivityState::default());
        write_private_pretty_json_atomic(&root.join(ACTIVITY_STATE_FILE), &store, "test").unwrap();
        forget_deleted_session_at(&root, "gone").unwrap();
        let loaded = load_session_activity_states_at(&root).unwrap();
        assert!(!loaded.contains_key("gone"));
        assert!(loaded.contains_key("keep"));
        forget_deleted_session_at(&root, "gone").unwrap();
        update_activity_at(&root, "gone", 99, |_| panic!("late deleted-session activity reached mutation")).unwrap();
        assert!(!load_session_activity_states_at(&root).unwrap().contains_key("gone"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_store_loads_as_empty_without_creating_state_file() {
        let root = test_root("missing");

        let states = load_session_activity_states_at(&root).expect("load missing state");

        assert!(states.is_empty());
        assert!(!root.join(ACTIVITY_STATE_FILE).exists());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn conversation_activity_follows_turns_but_not_a_restart_interrupt() {
        let root = test_root("conversation");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        let state = |root: &Path| {
            load_session_activity_states_at(root)
                .expect("load activity")
                .remove("cutex.session-a")
                .expect("activity state")
        };
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-10-18T01:00:00Z",
                    "turn/started",
                    json!({ "threadId": "thread-1", "turn": { "id": "turn-1" } }),
                ),
            )
            .expect("record turn start");
        let started = state(&root);
        assert_eq!(started.last_turn_started_at.as_deref(), Some("2026-10-18T01:00:00Z"));
        assert_eq!(started.conversation_at().as_deref(), Some("2026-10-18T01:00:00Z"));

        // The runtime restarts (new generation) and interrupts the turn the
        // restart left in progress: not conversation activity.
        recorder
            .record_at(
                &root,
                &envelope_at_generation(
                    "2026-10-18T03:00:00Z",
                    "turn/completed",
                    json!({ "threadId": "thread-1", "turn": { "id": "turn-1", "status": "interrupted" } }),
                    2,
                ),
            )
            .expect("record restart interrupt");
        let restarted = state(&root);
        assert_eq!(restarted.last_turn_completed_at.as_deref(), Some("2026-10-18T03:00:00Z"));
        assert_eq!(restarted.last_turn_completed_generation, Some(2));
        assert_eq!(restarted.conversation_at().as_deref(), Some("2026-10-18T01:00:00Z"));
        // Lifecycle-only events are not recorded at all.
        recorder
            .record_at(
                &root,
                &envelope_at_generation("2026-10-18T03:00:01Z", "thread/started", json!({ "thread": { "id": "thread-1" } }), 2),
            )
            .expect("ignore thread start");
        assert_eq!(state(&root).conversation_at().as_deref(), Some("2026-10-18T01:00:00Z"));

        for (at, method, params) in [
            ("2026-10-18T04:00:00Z", "item/started", json!({ "item": { "type": "commandExecution" } })),
            ("2026-10-18T04:00:05Z", "turn/completed", json!({ "turn": { "id": "turn-2", "status": "completed" } })),
        ] {
            recorder
                .record_at(&root, &envelope_at_generation(at, method, params, 2))
                .expect("record conversation event");
        }
        let resource = state(&root);
        assert_eq!(resource.last_conversation_at.as_deref(), Some("2026-10-18T04:00:05Z"));
        let persisted = String::from_utf8(persisted_activity_json(&root)).unwrap();
        assert!(persisted.contains("lastConversationAt") && persisted.contains("lastTurnStartedAt"));
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn completion_clock_ignores_metadata_and_retains_its_own_generation() {
        let root = test_root("completion-clock");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        recorder.record_at(&root, &envelope_at_generation("2026-10-18T01:00:00Z", "turn/completed", json!({"threadId":"thread-1","turn":{"id":"t1","status":"completed"}}), 1)).unwrap();
        recorder.record_at(&root, &envelope_at_generation("2026-10-18T01:01:00Z", "thread/name/updated", json!({"threadId":"thread-1","name":"changed"}), 1)).unwrap();
        let state = load_session_activity_states_at(&root).unwrap().remove("cutex.session-a").unwrap();
        assert_eq!(state.last_turn_completed_at.as_deref(), Some("2026-10-18T01:00:00Z"));
        assert_eq!(state.last_turn_completed_generation, Some(1));
        recorder.record_at(&root, &envelope_at_generation("2026-10-18T01:02:00Z", "turn/started", json!({"threadId":"thread-1","turn":{"id":"t2"}}), 2)).unwrap();
        let restored = load_session_activity_states_at(&root).unwrap().remove("cutex.session-a").unwrap();
        assert_eq!(restored.runtime_generation, Some(2));
        assert_eq!(restored.last_turn_completed_generation, Some(1));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn older_stores_derive_conversation_activity_from_their_turn_and_item_fields() {
        let legacy: SessionActivityState = serde_json::from_value(json!({
            "revision": 4,
            "runtimeGeneration": 1,
            "lastOutputAt": "2026-10-18T01:00:00Z",
            "lastTurnCompletedAt": "2026-10-18T01:00:09.5+00:00",
            "lastFileChangeAt": "2026-10-18T00:59:00Z"
        }))
        .expect("decode a pre-conversation-activity state");
        assert!(legacy.last_conversation_at.is_none());
        assert_eq!(legacy.conversation_at().as_deref(), Some("2026-10-18T01:00:09.5+00:00"));
        assert_eq!(SessionActivityState::default().conversation_at(), None);
    }

    #[test]
    fn r218_task_turn_bindings_load_and_are_dropped_on_next_write() {
        let root = test_root("r218-task-bindings");
        fs::create_dir_all(&root).expect("create test root");
        let r218_store = json!({
            "version": 1,
            "sessions": {
                "cutex.session-a": {
                    "revision": 3,
                    "runtimeGeneration": 2,
                    "taskTurnBindings": [{
                        "runtimeGeneration": 2,
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "assignmentId": "assignment-1",
                        "order": {
                            "runtimeGeneration": 2,
                            "streamId": "turn-1",
                            "sequence": 3,
                            "receivedAt": "2026-08-13T02:30:00Z"
                        }
                    }]
                }
            }
        });
        fs::write(
            root.join(ACTIVITY_STATE_FILE),
            serde_json::to_vec_pretty(&r218_store).expect("encode r218 store"),
        )
        .expect("write r218 activity store");

        let states = load_session_activity_states_at(&root).expect("load r218 activity store");
        assert_eq!(states["cutex.session-a"].revision, 3);

        ActivityRecorder::new(Duration::ZERO)
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:31:00Z",
                    "turn/completed",
                    json!({ "threadId": "thread-1", "turn": { "id": "turn-2" } }),
                    2,
                    "stream-a",
                    1,
                    "turn-2",
                ),
            )
            .expect("record after loading an r218 store");
        let persisted =
            String::from_utf8(persisted_activity_json(&root)).unwrap();
        assert!(persisted.contains("lastTurnCompletedAt"));
        assert!(!persisted.contains("taskTurnBindings"));
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn output_checkpoints_are_bounded_and_terminal_events_flush_latest_delta() {
        let root = test_root("output");
        let recorder = ActivityRecorder::new(Duration::from_secs(1));
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T01:00:00.000Z",
                    "item/agentMessage/delta",
                    json!({ "delta": "one" }),
                ),
            )
            .expect("record first delta");
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T01:00:00.100Z",
                    "item/agentMessage/delta",
                    json!({ "delta": "two" }),
                ),
            )
            .expect("buffer second delta");

        let state = load_session_activity_states_at(&root)
            .expect("load checkpoint")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 1);
        assert_eq!(
            state.last_output_at.as_deref(),
            Some("2026-08-13T01:00:00.000Z")
        );

        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T01:00:00.200Z",
                    "item/completed",
                    json!({
                        "item": { "type": "agentMessage", "text": "onetwo" }
                    }),
                ),
            )
            .expect("flush completed output");
        let state = load_session_activity_states_at(&root)
            .expect("load flushed state")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 2);
        assert_eq!(
            state.last_output_at.as_deref(),
            Some("2026-08-13T01:00:00.100Z")
        );
        assert_eq!(
            state.last_output_completed_at.as_deref(),
            Some("2026-08-13T01:00:00.200Z")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn failed_output_persistence_is_retried_at_a_bounded_rate() {
        let root = test_root("retry");
        fs::create_dir_all(&root).expect("create test root");
        fs::write(
            root.join(ACTIVITY_STATE_FILE),
            br#"{"version":9,"sessions":{}}"#,
        )
        .expect("write unsupported activity state");
        let recorder = ActivityRecorder::new(Duration::from_secs(1));
        let first = recorder.record_at(
            &root,
            &envelope(
                "2026-08-13T01:30:00.000Z",
                "item/agentMessage/delta",
                json!({ "delta": "one" }),
            ),
        );
        assert!(first
            .expect_err("unsupported state must fail")
            .to_string()
            .contains("unsupported session store version"));

        fs::remove_file(root.join(ACTIVITY_STATE_FILE)).expect("remove invalid state");
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T01:30:00.100Z",
                    "item/agentMessage/delta",
                    json!({ "delta": "two" }),
                ),
            )
            .expect("rate-limit immediate retry");
        assert!(load_session_activity_states_at(&root)
            .expect("load empty state")
            .is_empty());

        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T01:30:01.100Z",
                    "item/agentMessage/delta",
                    json!({ "delta": "three" }),
                ),
            )
            .expect("retry after interval");
        let state = load_session_activity_states_at(&root)
            .expect("load recovered state")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 1);
        assert_eq!(
            state.last_output_at.as_deref(),
            Some("2026-08-13T01:30:01.100Z")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn terminal_activity_and_file_changes_are_persisted() {
        let root = test_root("terminal");
        let recorder = ActivityRecorder::new(Duration::from_secs(1));
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T02:00:00Z",
                    "turn/completed",
                    json!({ "turn": { "id": "turn-1", "status": "completed" } }),
                ),
            )
            .expect("record completed turn");
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T02:00:01Z",
                    "item/completed",
                    json!({
                        "item": {
                            "type": "fileChange",
                            "status": "completed",
                            "changes": [{ "path": "/workspace/a.rs", "kind": "update" }]
                        }
                    }),
                ),
            )
            .expect("record file change");

        let state = load_session_activity_states_at(&root)
            .expect("load terminal activity")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 2);
        assert_eq!(
            state.last_turn_completed_at.as_deref(),
            Some("2026-08-13T02:00:00Z")
        );
        assert_eq!(
            state.last_file_change_at.as_deref(),
            Some("2026-08-13T02:00:01Z")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn legacy_task_message_turn_stays_session_scoped_and_tool_lifecycle_is_sanitized() {
        let root = test_root("legacy-task-turn");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        recorder
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:30:00Z",
                    "item/started",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "task-message-1",
                            "type": "interAgentMessage",
                            "taskServicePresentation": {
                                "class": "assignment",
                                "taskName": "bounded task",
                                "assignmentId": "assignment-1",
                                "semanticPayload": "not inspected"
                            }
                        }
                    }),
                    2,
                    "stream-a",
                    1,
                    "turn-1",
                ),
            )
            .expect("ignore legacy Task message start");
        recorder
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:30:01Z",
                    "item/started",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "tool-1",
                            "type": "commandExecution",
                            "command": "printf secret-command-material",
                            "arguments": { "token": "must-not-persist" }
                        }
                    }),
                    2,
                    "stream-a",
                    2,
                    "turn-1",
                ),
            )
            .expect("record tool start");
        recorder
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:30:02Z",
                    "item/commandExecution/outputDelta",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "itemId": "tool-1",
                        "delta": "unfiltered-command-output-must-not-persist"
                    }),
                    2,
                    "stream-a",
                    3,
                    "turn-1",
                ),
            )
            .expect("record tool progress");
        recorder
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:30:03Z",
                    "item/completed",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "tool-1",
                            "type": "commandExecution",
                            "status": "failed",
                            "aggregatedOutput": "unfiltered-command-output-must-not-persist"
                        }
                    }),
                    2,
                    "stream-a",
                    4,
                    "turn-1",
                ),
            )
            .expect("record tool failure");
        recorder
            .record_at(
                &root,
                &correlated_envelope(
                    "2026-08-13T02:30:04Z",
                    "item/completed",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "id": "message-1",
                            "type": "agentMessage",
                            "phase": "final_answer",
                            "text": "final visible reply"
                        }
                    }),
                    2,
                    "stream-a",
                    5,
                    "turn-1",
                ),
            )
            .expect("record final output");

        let state = load_session_activity_states_at(&root)
            .expect("load legacy-task-turn activity")
            .remove("cutex.session-a")
            .expect("legacy-task-turn activity");
        let output = state.last_output.expect("final output");
        assert_eq!(output.class, SafeOutputClass::FinalVisible);
        assert_eq!(output.display_text, "final visible reply");
        assert_eq!(output.association, ObservationAssociation::session("cutex.session-a"));
        let tool = state.last_tool_call.expect("tool projection");
        assert_eq!(tool.class, SafeToolCallClass::Command);
        assert_eq!(tool.status, SafeToolCallStatus::Failed);
        assert_eq!(tool.display_text, "Command");
        assert_eq!(tool.association, ObservationAssociation::session("cutex.session-a"));

        let persisted = String::from_utf8(
            persisted_activity_json(&root),
        )
        .expect("utf8 projection");
        for forbidden in [
            "secret-command-material",
            "must-not-persist",
            "unfiltered-command-output",
            "semanticPayload",
        ] {
            assert!(
                !persisted.contains(forbidden),
                "persisted unsafe {forbidden}"
            );
        }
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn output_projection_is_bounded_absent_when_unavailable_and_order_fenced() {
        let root = test_root("projection-order");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        let first = correlated_envelope(
            "2026-08-13T02:40:00Z",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-unbound",
                "item": { "type": "agentMessage", "text": "x".repeat(900) }
            }),
            2,
            "stream-a",
            10,
            "turn-unbound",
        );
        recorder
            .record_at(&root, &first)
            .expect("record bounded output");
        let duplicate = correlated_envelope(
            "2026-08-13T02:40:00Z",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-unbound",
                "item": { "type": "agentMessage", "text": "duplicate-must-not-win" }
            }),
            2,
            "stream-a",
            10,
            "turn-unbound",
        );
        recorder
            .record_at(&root, &duplicate)
            .expect("ignore duplicate");
        let stale_generation = correlated_envelope(
            "2026-08-13T02:40:10Z",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-unbound",
                "item": { "type": "agentMessage", "text": "stale-generation" }
            }),
            1,
            "stream-a",
            11,
            "turn-unbound",
        );
        recorder
            .record_at(&root, &stale_generation)
            .expect("ignore stale generation");
        let rebound = correlated_envelope(
            "2026-08-13T02:40:11Z",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-unbound",
                "item": {
                    "type": "agentMessage",
                    "phase": "commentary",
                    "text": "rebound progress"
                }
            }),
            2,
            "stream-b",
            1,
            "turn-unbound",
        );
        recorder
            .record_at(&root, &rebound)
            .expect("accept stream rebind");

        let state = load_session_activity_states_at(&root)
            .expect("load ordered projection")
            .remove("cutex.session-a")
            .expect("ordered projection");
        let output = state.last_output.expect("last output");
        assert_eq!(output.display_text, "rebound progress");
        assert_eq!(output.class, SafeOutputClass::Progress);
        assert_eq!(output.association.assignment_id, None);
        assert_eq!(output.association.attempt_number, None);
        let initial_preview = sanitize_visible_output(&"x".repeat(900)).expect("bounded preview");
        assert_eq!(
            initial_preview.chars().count(),
            crate::observability::OBSERVABILITY_TEXT_LIMIT
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn stale_events_cannot_move_activity_backwards_or_bump_revision() {
        let root = test_root("monotonic");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T03:00:02Z",
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "text": "new" } }),
                ),
            )
            .expect("record current completion");
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T03:00:01Z",
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "text": "stale" } }),
                ),
            )
            .expect("ignore stale completion");

        let state = load_session_activity_states_at(&root)
            .expect("load monotonic state")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 1);
        assert_eq!(
            state.last_output_at.as_deref(),
            Some("2026-08-13T03:00:02Z")
        );
        assert_eq!(
            state.last_output_completed_at.as_deref(),
            Some("2026-08-13T03:00:02Z")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn older_runtime_generation_cannot_advance_or_clear_current_activity() {
        let root = test_root("generation");
        let current = ActivityRecorder::new(Duration::ZERO);
        current
            .record_at(
                &root,
                &envelope_at_generation(
                    "2026-08-13T03:10:00Z",
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "text": "current" } }),
                    2,
                ),
            )
            .expect("record current generation");

        let stale_process = ActivityRecorder::new(Duration::ZERO);
        stale_process
            .record_at(
                &root,
                &envelope_at_generation(
                    "2026-08-13T03:10:01Z",
                    "item/agentMessage/delta",
                    json!({ "delta": "stale" }),
                    1,
                ),
            )
            .expect("ignore stale generation delta");
        stale_process
            .record_at(
                &root,
                &envelope_at_generation(
                    "2026-08-13T03:10:02Z",
                    "item/completed",
                    json!({ "item": { "type": "agentMessage", "text": "stale" } }),
                    1,
                ),
            )
            .expect("ignore stale generation completion");

        let state = load_session_activity_states_at(&root)
            .expect("load generation-fenced state")
            .remove("cutex.session-a")
            .expect("activity state");
        assert_eq!(state.revision, 1);
        assert_eq!(state.runtime_generation, Some(2));
        assert_eq!(
            state.last_output_at.as_deref(),
            Some("2026-08-13T03:10:00Z")
        );
        assert_eq!(
            state.last_output_completed_at.as_deref(),
            Some("2026-08-13T03:10:00Z")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn unrelated_and_empty_events_do_not_create_activity_while_typed_tools_do() {
        let root = test_root("ignored");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        for event in [
            envelope(
                "2026-08-13T04:00:00Z",
                "item/agentMessage/delta",
                json!({ "delta": "" }),
            ),
            envelope(
                "2026-08-13T04:00:01Z",
                "item/completed",
                json!({
                    "item": { "type": "fileChange", "status": "completed", "changes": [] }
                }),
            ),
        ] {
            recorder.record_at(&root, &event).expect("ignore event");
        }

        assert!(load_session_activity_states_at(&root)
            .expect("load ignored activity")
            .is_empty());

        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T04:00:02Z",
                    "item/completed",
                    json!({ "item": { "type": "commandExecution", "status": "completed" } }),
                ),
            )
            .expect("project typed command completion");
        let state = load_session_activity_states_at(&root)
            .expect("load tool activity")
            .remove("cutex.session-a")
            .expect("tool activity");
        assert_eq!(
            state.last_tool_call.map(|tool| tool.status),
            Some(SafeToolCallStatus::Finished)
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn persisted_json_uses_stable_camel_case_fields_and_nulls() {
        let root = test_root("json");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        recorder
            .record_at(
                &root,
                &envelope(
                    "2026-08-13T05:00:00Z",
                    "turn/completed",
                    json!({ "turn": { "id": "turn-1", "status": "completed" } }),
                ),
            )
            .expect("record activity");

        let value: serde_json::Value = serde_json::from_slice(
            &persisted_activity_json(&root),
        )
        .expect("parse activity state");
        assert_eq!(
            value.pointer("/sessions/cutex.session-a"),
            Some(&json!({
                "revision": 1,
                "runtimeGeneration": 1,
                "lastOutputAt": null,
                "lastOutputCompletedAt": null,
                    "lastTurnCompletedAt": "2026-08-13T05:00:00Z",
                    "lastTurnCompletedGeneration": 1,
                "lastFileChangeAt": null,
                "lastConversationAt": "2026-08-13T05:00:00Z",
                "lastOutput": null,
                "lastToolCall": null
            }))
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    fn turn_end(at: &str, turn: &str, status: &str, error: serde_json::Value) -> EventEnvelope {
        envelope(at, "turn/completed", json!({"threadId": "t", "turn": {"id": turn, "status": status, "error": error}}))
    }

    const REUSED: &str = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";

    #[test]
    fn turn_error_codes_are_classified() {
        let none = serde_json::Value::Null;
        assert_eq!(classify_turn_error(REUSED, &json!("unauthorized"), &none), "refresh_token_reused");
        assert_eq!(classify_turn_error("REFRESH TOKEN WAS ALREADY USED", &none, &none), "refresh_token_reused");
        assert_eq!(classify_turn_error("x", &json!("other"), &json!({"code": "refresh_token_reused"})), "refresh_token_reused");
        assert_eq!(classify_turn_error("x", &json!({"refresh_token_reused": {}}), &none), "refresh_token_reused");
        assert_eq!(classify_turn_error("x", &json!("unauthorized"), &none), "unauthorized");
        assert_eq!(
            classify_turn_error("unexpected status 401 Unauthorized: …", &json!({"httpConnectionFailed": {"httpStatusCode": 401}}), &none),
            "unauthorized"
        );
        assert_eq!(classify_turn_error("x", &json!({"httpConnectionFailed": {"httpStatusCode": 502}}), &none), "httpConnectionFailed");
        assert_eq!(classify_turn_error("x", &json!("usageLimitExceeded"), &none), "usageLimitExceeded");
        assert_eq!(classify_turn_error("x", &json!({"a": 1, "b": 2}), &none), "other");
        assert_eq!(classify_turn_error("x", &none, &none), "other");
        assert_eq!(classify_turn_error("x", &json!(""), &none), "other");
        assert!(is_unauthorized_code("unauthorized") && is_unauthorized_code("refresh_token_reused"));
        assert!(!is_unauthorized_code("other"));
        // Message bounded; an oversized info keeps its code.
        let failure = TurnFailure::from_turn(&json!({"id": "t1", "status": "failed",
            "error": {"message": "m".repeat(5000), "codexErrorInfo": {"big": "x".repeat(5000)}}}));
        assert_eq!(failure.message.chars().count(), TURN_ERROR_MESSAGE_CHARS);
        assert_eq!((failure.code.as_str(), failure.codex_error_info.clone()), ("big", json!("big")));
    }

    #[test]
    fn last_turn_error_sets_counts_survives_interrupt_clears_on_completion_and_persists() {
        let root = test_root("turn-error");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        let load = |root: &Path| load_session_activity_states_at(root).unwrap().remove("cutex.session-a").unwrap();
        let reused = json!({"message": REUSED, "codexErrorInfo": "unauthorized", "additionalDetails": null});
        recorder.record_at_for(&root, &turn_end("2026-11-04T05:27:25.123Z", "turn-1", "failed", reused.clone()), Some("acct-1"), Some("work")).unwrap();
        let error = load(&root).last_turn_error.expect("failed turn recorded");
        assert_eq!(error.resource(), json!({"turnId": "turn-1", "message": REUSED, "code": "refresh_token_reused",
            "codexErrorInfo": "unauthorized", "at": "2026-11-04T05:27:25.123Z", "consecutive": 1}));
        assert_eq!((error.profile_id.as_deref(), error.profile_name.as_deref()), (Some("acct-1"), Some("work")));
        // The same turn reported again keeps its count.
        recorder.record_at(&root, &turn_end("2026-11-04T05:27:26Z", "turn-1", "failed", reused.clone())).unwrap();
        assert_eq!(load(&root).last_turn_error, Some(error), "a redelivery keeps count, time and profile");
        let http = json!({"message": "unexpected status 401 Unauthorized: …", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 401}}});
        recorder.record_at(&root, &turn_end("2026-11-04T05:30:00Z", "turn-2", "failed", http)).unwrap();
        let error = load(&root).last_turn_error.unwrap();
        assert_eq!((error.turn_id.as_deref(), error.code.as_str(), error.consecutive), (Some("turn-2"), "unauthorized", 2));
        assert_eq!(error.codex_error_info, json!({"httpConnectionFailed": {"httpStatusCode": 401}}));
        // Interrupted leaves it; a stale older end cannot move it.
        recorder.record_at(&root, &turn_end("2026-11-04T05:31:00Z", "turn-3", "interrupted", serde_json::Value::Null)).unwrap();
        recorder.record_at(&root, &turn_end("2026-11-04T05:00:00Z", "turn-0", "completed", serde_json::Value::Null)).unwrap();
        let kept = load(&root).last_turn_error.unwrap();
        assert_eq!((kept.turn_id.as_deref(), kept.consecutive), (Some("turn-2"), 2));
        // Persisted with the activity state: a fresh reader (restart) sees it.
        let persisted: serde_json::Value = serde_json::from_slice(&persisted_activity_json(&root)).unwrap();
        assert_eq!(persisted.pointer("/sessions/cutex.session-a/lastTurnError/code"), Some(&json!("unauthorized")));
        assert_eq!(persisted.pointer("/sessions/cutex.session-a/lastTurnError/consecutive"), Some(&json!(2)));
        let restarted = ActivityRecorder::new(Duration::ZERO);
        // Any other code counts on; a completed turn clears.
        restarted.record_at(&root, &turn_end("2026-11-04T05:32:00Z", "turn-4", "failed",
            json!({"message": "boom", "codexErrorInfo": "other"}))).unwrap();
        assert_eq!(load(&root).last_turn_error.unwrap().consecutive, 3);
        restarted.record_at(&root, &turn_end("2026-11-04T05:33:00Z", "turn-5", "completed", serde_json::Value::Null)).unwrap();
        assert!(load(&root).last_turn_error.is_none());
        let persisted = String::from_utf8(persisted_activity_json(&root)).unwrap();
        assert!(!persisted.contains("lastTurnError"), "cleared, not kept: {persisted}");
        restarted.record_at(&root, &turn_end("2026-11-04T05:34:00Z", "turn-6", "failed",
            json!({"message": "again", "codexErrorInfo": "serverOverloaded"}))).unwrap();
        assert_eq!(load(&root).last_turn_error.unwrap().consecutive, 1, "counting restarts after a completed turn");
        // A revert that removed the failed turn clears it.
        forget_reverted_turns_at(&root, "cutex.session-a", &["turn-6"]).unwrap();
        assert!(load(&root).last_turn_error.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unauthorized_notices_are_limited_per_profile_per_rolling_hour() {
        let root = test_root("turn-error-notice");
        let recorder = ActivityRecorder::new(Duration::ZERO);
        let unauthorized = json!({"message": "unexpected status 401", "codexErrorInfo": "unauthorized"});
        let fail = |session: &str, at: &str, turn: &str, error: &serde_json::Value, profile: &str| {
            let mut event = turn_end(at, turn, "failed", error.clone());
            event.cutex_session_id = session.to_string();
            recorder.record_at_for(&root, &event, Some(profile), Some(&format!("name-{profile}"))).unwrap()
        };
        let first = fail("cutex.a", "2026-11-04T05:00:00Z", "a1", &unauthorized, "p1").expect("first failure notifies");
        assert_eq!((first.cutex_session_id.as_str(), first.turn_id.as_deref(), first.affected_sessions), ("cutex.a", Some("a1"), 1));
        assert!(first.text().contains("配置「name-p1」") && first.text().contains("1 个会话"), "{}", first.text());
        assert!(first.text().contains("重新登录"));
        // Within the hour: a second failure on the profile (another session) is limited.
        assert!(fail("cutex.b", "2026-11-04T05:20:00Z", "b1", &json!({"message": REUSED}), "p1").is_none());
        // Other codes never notify; another profile has its own hour.
        assert!(fail("cutex.c", "2026-11-04T05:21:00Z", "c1", &json!({"message": "x", "codexErrorInfo": "other"}), "p2").is_none());
        let other = fail("cutex.d", "2026-11-04T05:22:00Z", "d1", &json!({"message": REUSED}), "p2").expect("per profile");
        assert_eq!(other.affected_sessions, 1);
        assert!(other.text().contains("并发刷新冲突") && other.text().contains("自动重试"), "{}", other.text());
        // After the hour: another notice, counting both sessions of p1.
        assert!(fail("cutex.a", "2026-11-04T05:59:59.999999999Z", "a2", &unauthorized, "p1").is_none());
        let later = fail("cutex.a", "2026-11-04T06:00:00Z", "a3", &unauthorized, "p1").expect("rolling hour passed");
        assert_eq!((later.affected_sessions, later.consecutive), (2, 3));
        // The limit is persisted: a restarted recorder still honours it.
        let restarted = ActivityRecorder::new(Duration::ZERO);
        let mut event = turn_end("2026-11-04T06:10:00Z", "b2", "failed", unauthorized.clone());
        event.cutex_session_id = "cutex.b".into();
        assert!(restarted.record_at_for(&root, &event, Some("p1"), None).unwrap().is_none());
        let db = rusqlite::Connection::open(root.join("session-activity.sqlite3")).unwrap();
        let count: i64 = db.query_row("SELECT count(*) FROM turn_error_notices", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_answer_phase_is_progress_not_final() {
        assert_eq!(output_class(Some("partial_answer")), SafeOutputClass::Progress);
        assert_eq!(output_class(Some("commentary")), SafeOutputClass::Progress);
        assert_eq!(output_class(Some("final_answer")), SafeOutputClass::FinalVisible);
        assert_eq!(output_class(Some("future_phase")), SafeOutputClass::Unclassified);
        assert_eq!(output_class(None), SafeOutputClass::Unclassified);
    }
}

#[cfg(test)]
mod agent_projection_tests {
    use super::*;
    use serde_json::{json,Value};
    #[test]
    fn reads_do_not_invent_activity_or_reuse_another_generations_busy_since(){
        let now=chrono::Utc::now();
        let unknown=agent_projection(None,Some(true),false,Some(2),now);
        assert_eq!(unknown["lastActivityAt"],Value::Null);
        assert_eq!(unknown["activityStatus"],"unknown");
        assert_eq!(unknown["idleThresholdSeconds"],7200);
        let activity=SessionActivityState{runtime_generation:Some(1),last_conversation_at:Some("2026-10-27T00:00:00Z".into()),last_turn_started_at:Some("2026-10-27T00:00:00Z".into()),..Default::default()};
        let before=activity.clone();
        let a=agent_projection(Some(&activity),Some(true),true,Some(2),now);
        let b=agent_projection(Some(&activity),Some(true),true,Some(2),now+chrono::Duration::hours(3));
        assert_eq!(a["lastActivityAt"],b["lastActivityAt"]);
        assert_eq!(a["busySince"],Value::Null);
        assert_eq!(a["activityStatus"],json!("busy"));
        assert_eq!(activity,before);
        assert_eq!(agent_projection(Some(&activity),Some(false),false,Some(1),now)["activityStatus"],"offline");
    }
}
