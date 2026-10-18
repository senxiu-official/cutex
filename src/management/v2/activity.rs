use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use serde::Deserialize;
use serde::Serialize;

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
const ACTIVITY_LOCK_FILE: &str = "session-activity-state.lock";
const OUTPUT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

static ACTIVITY_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ACTIVITY_RECORDER: OnceLock<ActivityRecorder> = OnceLock::new();

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
struct ActivityStore {
    #[serde(default = "activity_store_version")]
    version: u8,
    #[serde(default)]
    sessions: HashMap<String, SessionActivityState>,
}

impl Default for ActivityStore {
    fn default() -> Self {
        Self {
            version: activity_store_version(),
            sessions: HashMap::new(),
        }
    }
}

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

    fn record_at(&self, root: &Path, envelope: &EventEnvelope) -> anyhow::Result<()> {
        let Some(event) = activity_event(envelope) else {
            return Ok(());
        };
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
                let checkpoint = output.entry(envelope.cutex_session_id.clone()).or_default();
                match checkpoint.runtime_generation {
                    Some(current) if runtime_generation < current => return Ok(()),
                    Some(current) if runtime_generation > current => {
                        *checkpoint = OutputCheckpoint {
                            runtime_generation: Some(runtime_generation),
                            ..OutputCheckpoint::default()
                        };
                    }
                    None => checkpoint.runtime_generation = Some(runtime_generation),
                    Some(_) => {}
                }
                merge_timestamp(&mut checkpoint.latest_at, &envelope.received_at)?;
                let latest_at = checkpoint
                    .latest_at
                    .as_deref()
                    .context("output checkpoint is missing its latest timestamp")?;
                if !checkpoint_due(
                    checkpoint.last_attempt_at.as_deref(),
                    latest_at,
                    self.output_checkpoint_interval,
                )? {
                    return Ok(());
                }
                checkpoint.last_attempt_at = Some(latest_at.to_string());
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let changed = merge_timestamp(&mut state.last_output_at, latest_at)?;
                        Ok(merge_timestamp(&mut state.last_conversation_at, latest_at)? | changed)
                    },
                )?;
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
            ActivityEvent::TurnCompleted { interrupted } => {
                let pending_at = output
                    .get(&envelope.cutex_session_id)
                    .filter(|checkpoint| checkpoint.runtime_generation == Some(runtime_generation))
                    .and_then(|checkpoint| checkpoint.latest_at.clone());
                update_activity_at(
                    root,
                    &envelope.cutex_session_id,
                    runtime_generation,
                    |state| {
                        let mut changed = merge_optional_timestamp(
                            &mut state.last_output_at,
                            pending_at.as_deref(),
                        )?;
                        changed |= merge_timestamp(
                            &mut state.last_turn_completed_at,
                            &envelope.received_at,
                        )?;
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
                        Ok(changed)
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
        Ok(())
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
    },
    FileChangeCompleted,
    ToolLifecycle {
        class: SafeToolCallClass,
        status: SafeToolCallStatus,
    },
}

pub fn record_activity_event(envelope: &EventEnvelope) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    ACTIVITY_RECORDER
        .get_or_init(|| ActivityRecorder::new(OUTPUT_CHECKPOINT_INTERVAL))
        .record_at(&root, envelope)
}

pub fn load_session_activity_states() -> anyhow::Result<HashMap<String, SessionActivityState>> {
    let root = runtime_dir()?.join("management-v2");
    load_session_activity_states_at(&root)
}

/// UI refreshes keep their previous snapshot while another process writes.
pub fn try_load_session_activity_states() -> anyhow::Result<Option<HashMap<String, SessionActivityState>>> {
    try_load_session_activity_states_at(&runtime_dir()?.join("management-v2"))
}

fn try_load_session_activity_states_at(root: &Path) -> anyhow::Result<Option<HashMap<String, SessionActivityState>>> {
    let _guard = match ACTIVITY_STORE_LOCK.get_or_init(|| Mutex::new(())).try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        Err(error) => anyhow::bail!("management v2 session activity lock: {error}"),
    };
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let lock = open_private_lock(&root.join(ACTIVITY_LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => {},
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    let result = load_activity_store(&root.join(ACTIVITY_STATE_FILE)).map(|store| Some(store.sessions));
    let unlock = lock.unlock();
    if result.is_ok() { unlock?; }
    result
}

#[cfg(test)]
#[test]
fn ui_activity_refresh_skips_a_locked_store_and_recovers() {
    let root = std::env::temp_dir().join(format!("cutex-ui-activity-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let lock = open_private_lock(&root.join(ACTIVITY_LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    let start = std::time::Instant::now();
    assert!(try_load_session_activity_states_at(&root).unwrap().is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
    lock.unlock().unwrap();
    // Sibling tests share the process-wide ACTIVITY_STORE_LOCK, so a refresh may
    // still be skipped briefly under the parallel test runner; recovery must
    // happen once the in-process holder is gone.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if try_load_session_activity_states_at(&root).unwrap().is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "activity refresh never recovered");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(lock);
    fs::remove_dir_all(root).unwrap();
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
        "turn/completed" => Some(ActivityEvent::TurnCompleted {
            interrupted: params.pointer("/turn/status").and_then(serde_json::Value::as_str)
                == Some("interrupted"),
        }),
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
    Ok(true)
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
    if cutex_session_id.is_empty() {
        anyhow::bail!("cutexSessionId must not be empty for session activity");
    }
    with_activity_store_lock(root, |path| {
        let mut store = load_activity_store(path)?;
        let state = store
            .sessions
            .entry(cutex_session_id.to_string())
            .or_default();
        if state
            .runtime_generation
            .is_some_and(|current| runtime_generation < current)
        {
            return Ok(());
        }
        let generation_changed = state.runtime_generation != Some(runtime_generation);
        if generation_changed {
            state.runtime_generation = Some(runtime_generation);
        }
        if update(state)? || generation_changed {
            state.revision = state
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_SEQUENCE)
                .context("session activity revision exhausted")?;
            state.validate()?;
            write_private_pretty_json_atomic(path, &store, "management v2 session activity")?;
        }
        Ok(())
    })
}

fn load_session_activity_states_at(
    root: &Path,
) -> anyhow::Result<HashMap<String, SessionActivityState>> {
    with_activity_store_lock(root, |path| Ok(load_activity_store(path)?.sessions))
}

fn with_activity_store_lock<T>(
    root: &Path,
    action: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _process_guard = ACTIVITY_STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 session activity lock was poisoned"))?;
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let lock_file = open_private_lock(&root.join(ACTIVITY_LOCK_FILE))?;
    lock_file.lock()?;
    let result = action(&root.join(ACTIVITY_STATE_FILE));
    let unlock = lock_file.unlock();
    if result.is_ok() {
        unlock?;
    }
    result
}

fn load_activity_store(path: &Path) -> anyhow::Result<ActivityStore> {
    let store = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<ActivityStore>(&bytes).with_context(|| {
            format!(
                "Failed to parse management v2 session activity: {}",
                path.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ActivityStore::default(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read management v2 session activity: {}",
                    path.display()
                )
            });
        }
    };
    if store.version != activity_store_version() {
        anyhow::bail!(
            "unsupported management v2 session activity version: {}",
            store.version
        );
    }
    for (cutex_session_id, state) in &store.sessions {
        if cutex_session_id.is_empty() {
            anyhow::bail!("session activity contains an empty cutexSessionId");
        }
        state
            .validate()
            .with_context(|| format!("invalid session activity for {cutex_session_id}"))?;
    }
    Ok(store)
}

fn open_private_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
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
        let persisted = fs::read_to_string(root.join(ACTIVITY_STATE_FILE)).expect("read store");
        assert!(persisted.contains("lastConversationAt") && persisted.contains("lastTurnStartedAt"));
        fs::remove_dir_all(root).expect("remove test root");
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
            fs::read_to_string(root.join(ACTIVITY_STATE_FILE)).expect("read rewritten store");
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
            .contains("unsupported management v2 session activity version"));

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
            fs::read(root.join(ACTIVITY_STATE_FILE)).expect("read activity projection"),
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
            &fs::read(root.join(ACTIVITY_STATE_FILE)).expect("read activity state"),
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
                "lastFileChangeAt": null,
                "lastConversationAt": "2026-08-13T05:00:00Z",
                "lastOutput": null,
                "lastToolCall": null
            }))
        );
        fs::remove_dir_all(root).expect("remove test root");
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
