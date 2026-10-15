//! Owner-initiated Director rotation (capability `owner-rotation`;
//! cutex-desktop docs/research/owner-rotation.md). Replaces the
//! agent-initiated `director_rotate` that r219 removed.
//!
//! The owner starts a rotation for a Project that has a Director. Cutex asks
//! the Director (an owner-origin `cutex/userInput/submit`, `auto` +
//! `startIfOffline`) to write a handoff note to
//! `<workspace_root or Director cwd>/handoff/<UTC timestamp>.md` and end its
//! turn, waits for the file plus the turn end (or a timeout), creates the
//! successor (not started) in the Project, moves the Director seat with the
//! fenced `rotate_director` project mutation, disposes the predecessor
//! (archive / member / Agent Operator) and finally starts the successor with
//! a first message that points at the handoff note.
//!
//! ```text
//! requested → handoff_requested → handoff_written ─┐
//!                  │                                ├→ successor_created → authority_rebound
//!                  └─(deadline)→ timeout ─(continue)┘     → predecessor_disposed → done
//! waiting states →(cancel) cancelled; unrecoverable errors → failed
//! ```
//!
//! Records live in `<cutex home>/runtime/owner-rotation/rotations.json`
//! (private, rewritten atomically, newest 50 finished records kept). A daemon
//! thread advances them (see `owner_rotation_api`); every step can be redone
//! after a restart (fixed `clientUserMessageId`s and `action_id`s, archive
//! checks the lifecycle first). The one step whose outcome cannot be proven is
//! an interrupted successor creation: on resume the agent is looked up by its
//! reserved name; when it is not found the rotation fails with
//! `successor_creation_interrupted` (the Director is unchanged).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const UPDATED_EVENT: &str = "cutex/ownerRotation/updated";
pub const DEFAULT_HANDOFF_TIMEOUT_SECS: u64 = 600;
pub const MIN_HANDOFF_TIMEOUT_SECS: u64 = 60;
pub const MAX_HANDOFF_TIMEOUT_SECS: u64 = 86_400;
/// Retryable failures of one step before the rotation fails.
pub const MAX_STEP_ATTEMPTS: u32 = 5;
/// Consecutive polls that must see the handoff file and no running turn.
pub const STABLE_POLLS: u32 = 2;
const KEEP_FINISHED: usize = 50;
const MAX_NAME_CHARS: usize = 128;
const MAX_FIELD_CHARS: usize = 200;
/// `origin.clientId` of the rotation's owner messages.
pub const CLIENT_ID: &str = "cutex-owner-rotation";

// ---------------------------------------------------------------------------
// Shapes

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationState {
    Requested,
    HandoffRequested,
    HandoffWritten,
    Timeout,
    SuccessorCreated,
    AuthorityRebound,
    PredecessorDisposed,
    Done,
    Cancelled,
    Failed,
}

impl RotationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Failed)
    }

    /// Waiting for the Director (cancel / continue apply).
    pub fn is_waiting(self) -> bool {
        matches!(self, Self::HandoffRequested | Self::Timeout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredecessorDisposition {
    Archive,
    Member,
    Operator,
}

impl PredecessorDisposition {
    fn sentence(self) -> &'static str {
        match self {
            Self::Archive => "be archived",
            Self::Member => "stay in the Project as a member",
            Self::Operator => "stay in the Project as an Agent Operator",
        }
    }
}

/// Successor fields the owner may set; omitted ones are inherited from the
/// predecessor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SuccessorInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `POST /v2/director-rotations`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub request_id: String,
    pub project_id: String,
    pub expected_director_cutex_session_id: String,
    pub expected_authority_epoch: u64,
    pub predecessor: PredecessorDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_timeout_secs: Option<u64>,
    #[serde(default)]
    pub successor: SuccessorInput,
}

/// The successor as it will be created (defaults resolved at start).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuccessorSpec {
    pub name: String,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub cwd: String,
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRef {
    pub cutex_session_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub path: String,
    pub timeout_secs: u64,
    pub client_user_message_id: String,
    pub requested_at: Option<String>,
    pub deadline: Option<String>,
    pub written_at: Option<String>,
    /// The owner chose to continue without waiting for the note.
    pub skipped: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationCounts {
    /// Open delegations the predecessor requested (they stay its own).
    pub requested: u32,
    /// Open delegations the predecessor handles.
    pub handling: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub state: RotationState,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RotationError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rotation {
    pub rotation_id: String,
    pub request_id: String,
    pub request_sha256: String,
    pub project_id: String,
    pub project_name: String,
    pub state: RotationState,
    pub predecessor: AgentRef,
    pub expected_authority_epoch: u64,
    pub disposition: PredecessorDisposition,
    pub successor_spec: SuccessorSpec,
    pub handoff: Handoff,
    pub successor_cutex_session_id: Option<String>,
    /// Set (durably) right before the successor is created.
    pub successor_create_started_at: Option<String>,
    /// Authority epoch after the rebind.
    pub authority_epoch: Option<u64>,
    pub predecessor_delegations: DelegationCounts,
    /// Retryable failures of the current step.
    pub attempts: u32,
    pub error: Option<RotationError>,
    pub warnings: Vec<String>,
    pub history: Vec<HistoryEntry>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub revision: u64,
}

impl Rotation {
    /// Cancel applies until the successor creation begins.
    pub fn cancellable(&self) -> bool {
        matches!(
            self.state,
            RotationState::Requested | RotationState::HandoffRequested | RotationState::Timeout
        ) && self.successor_create_started_at.is_none()
    }

    fn transition(&mut self, state: RotationState, now: &str, note: Option<String>) {
        self.state = state;
        self.attempts = 0;
        self.history.push(HistoryEntry {
            state,
            at: now.to_string(),
            note,
        });
        if state.is_terminal() {
            self.finished_at = Some(now.to_string());
        }
    }

    fn fail(&mut self, code: &str, message: String, now: &str) {
        self.error = Some(RotationError {
            code: code.to_string(),
            message: message.clone(),
        });
        self.transition(RotationState::Failed, now, Some(message));
    }

    /// Count a retryable failure, or fail the rotation.
    fn retry_or_fail(&mut self, code: &str, error: &EffectError, now: &str) {
        if error.retryable && self.attempts + 1 < MAX_STEP_ATTEMPTS {
            self.attempts += 1;
            return;
        }
        self.fail(code, format!("{}: {}", error.code, error.message), now);
    }
}

/// Owner-facing error of a route (`write_v2_error`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            message: message.into(),
            retryable: false,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    fn unavailable(error: impl std::fmt::Display) -> Self {
        Self {
            status: 503,
            code: "event_persistence_unavailable".to_string(),
            message: format!("owner rotation state is unavailable: {error}"),
            retryable: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Effects (daemon implementation in owner_rotation_api.rs, fakes in tests)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl EffectError {
    pub fn new(code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFacts {
    pub project_id: String,
    pub display_name: String,
    pub active: bool,
    pub director: Option<String>,
    pub authority_epoch: u64,
    pub workspace_root: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    pub cutex_session_id: String,
    pub name: String,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub cwd: String,
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
    pub retired: bool,
    /// Current Project membership (Agent Management).
    pub project_id: Option<String>,
}

pub trait RotationEffects: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn project(&self, project_id: &str) -> Result<Option<ProjectFacts>, EffectError>;
    fn agent(&self, cutex_session_id: &str) -> Result<Option<AgentFacts>, EffectError>;
    fn delegations(&self, cutex_session_id: &str) -> DelegationCounts;
    /// An owner-origin message (`auto`, `startIfOffline`). Re-sending the same
    /// `client_user_message_id` is deduplicated by the user-input store.
    fn send_message(
        &self,
        cutex_session_id: &str,
        client_user_message_id: &str,
        text: &str,
    ) -> Result<(), EffectError>;
    /// Whether the session runs a turn (offline: false).
    fn turn_active(&self, cutex_session_id: &str) -> Result<bool, EffectError>;
    /// The handoff file exists and is not empty.
    fn handoff_ready(&self, path: &Path) -> bool;
    /// An active agent named `name` created at or after `since`.
    fn find_agent_by_name(&self, name: &str, since: &str) -> Result<Option<String>, EffectError>;
    /// Create the successor in the Project without starting it.
    fn create_successor(&self, rotation: &Rotation) -> Result<String, EffectError>;
    /// `rotate_director` with the epoch fence; returns the new epoch.
    fn rebind(
        &self,
        project_id: &str,
        action_id: &str,
        expected_authority_epoch: u64,
        successor: &str,
    ) -> Result<u64, EffectError>;
    /// Archive (retire) an agent; an archived agent is left as is.
    fn archive(&self, cutex_session_id: &str, request_id: &str, reason: &str)
        -> Result<(), EffectError>;
    /// Make the agent a member of the Project (no-op when it is one).
    fn ensure_member(&self, project_id: &str, cutex_session_id: &str, action_id: &str)
        -> Result<(), EffectError>;
    /// Grant Agent Operator (no-op when granted).
    fn grant_operator(&self, project_id: &str, cutex_session_id: &str, action_id: &str)
        -> Result<(), EffectError>;
}

// ---------------------------------------------------------------------------
// Store

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    rotations: Vec<Rotation>,
}

pub struct RotationStore {
    path: PathBuf,
    lock: Mutex<()>,
    notify: fn(&Rotation),
}

impl RotationStore {
    pub fn at(path: PathBuf, notify: fn(&Rotation)) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
            notify,
        }
    }

    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(crate::config::paths::runtime_dir()?
            .join("owner-rotation")
            .join("rotations.json"))
    }

    fn load(&self) -> anyhow::Result<StoreFile> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, file: &mut StoreFile) -> anyhow::Result<()> {
        file.version = 1;
        // Newest finished records only; active ones are always kept.
        let mut finished: Vec<(String, String)> = file
            .rotations
            .iter()
            .filter(|rotation| rotation.state.is_terminal())
            .map(|rotation| (rotation.updated_at.clone(), rotation.rotation_id.clone()))
            .collect();
        if finished.len() > KEEP_FINISHED {
            finished.sort();
            let drop: Vec<String> = finished[..finished.len() - KEEP_FINISHED]
                .iter()
                .map(|(_, id)| id.clone())
                .collect();
            file.rotations.retain(|rotation| !drop.contains(&rotation.rotation_id));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::config::atomic::write_private_pretty_json_atomic(&self.path, file, "owner rotations")
    }

    pub fn get(&self, rotation_id: &str) -> anyhow::Result<Option<Rotation>> {
        let _guard = self.lock.lock().map_err(|_| anyhow::anyhow!("owner rotation lock poisoned"))?;
        Ok(self
            .load()?
            .rotations
            .into_iter()
            .find(|rotation| rotation.rotation_id == rotation_id))
    }

    /// Newest first.
    pub fn list(&self, project_id: Option<&str>, limit: usize) -> anyhow::Result<Vec<Rotation>> {
        let _guard = self.lock.lock().map_err(|_| anyhow::anyhow!("owner rotation lock poisoned"))?;
        let mut rotations: Vec<Rotation> = self
            .load()?
            .rotations
            .into_iter()
            .filter(|rotation| project_id.is_none_or(|id| rotation.project_id == id))
            .collect();
        rotations.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        rotations.truncate(limit);
        Ok(rotations)
    }

    pub fn active_ids(&self) -> anyhow::Result<Vec<String>> {
        let _guard = self.lock.lock().map_err(|_| anyhow::anyhow!("owner rotation lock poisoned"))?;
        Ok(self
            .load()?
            .rotations
            .into_iter()
            .filter(|rotation| !rotation.state.is_terminal())
            .map(|rotation| rotation.rotation_id)
            .collect())
    }

    /// Run `change` on one record under the lock; persists (bumping
    /// `revision` / `updatedAt` and publishing the update) only when the
    /// record changed.
    pub fn modify<T>(
        &self,
        rotation_id: &str,
        now: &str,
        change: impl FnOnce(&mut Rotation) -> T,
    ) -> anyhow::Result<Option<(Rotation, T)>> {
        let _guard = self.lock.lock().map_err(|_| anyhow::anyhow!("owner rotation lock poisoned"))?;
        let mut file = self.load()?;
        let Some(index) = file
            .rotations
            .iter()
            .position(|rotation| rotation.rotation_id == rotation_id)
        else {
            return Ok(None);
        };
        let before = file.rotations[index].clone();
        let value = change(&mut file.rotations[index]);
        if file.rotations[index] != before {
            let rotation = &mut file.rotations[index];
            rotation.revision += 1;
            rotation.updated_at = now.to_string();
            let updated = rotation.clone();
            self.save(&mut file)?;
            (self.notify)(&updated);
            return Ok(Some((updated, value)));
        }
        Ok(Some((before, value)))
    }

    fn insert(
        &self,
        decide: impl FnOnce(&[Rotation]) -> Result<Result<Rotation, Rotation>, ApiError>,
    ) -> Result<(Rotation, bool), ApiError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ApiError::unavailable("lock poisoned"))?;
        let mut file = self.load().map_err(ApiError::unavailable)?;
        match decide(&file.rotations)? {
            Err(existing) => Ok((existing, false)),
            Ok(rotation) => {
                file.rotations.push(rotation.clone());
                self.save(&mut file).map_err(ApiError::unavailable)?;
                (self.notify)(&rotation);
                Ok((rotation, true))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers

pub fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// `eva-r2` → `eva-r3`; anything else gets `-r2`.
pub fn next_successor_name(name: &str) -> String {
    if let Some((base, number)) = name.rsplit_once("-r") {
        if !base.is_empty()
            && (1..=6).contains(&number.len())
            && number.chars().all(|ch| ch.is_ascii_digit())
        {
            if let Ok(number) = number.parse::<u32>() {
                return format!("{base}-r{}", number + 1);
            }
        }
    }
    format!("{name}-r2")
}

/// `<base>/handoff/<UTC timestamp>.md`.
pub fn handoff_path(base: &str, now: DateTime<Utc>) -> String {
    Path::new(base)
        .join("handoff")
        .join(format!("{}.md", now.format("%Y%m%dT%H%M%SZ")))
        .to_string_lossy()
        .into_owned()
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.trim() == value
        && value.chars().count() <= max
        && !value.chars().any(char::is_control)
}

fn optional_text(value: &Option<String>, field: &str) -> Result<(), ApiError> {
    match value {
        Some(value) if !valid_text(value, MAX_FIELD_CHARS) => Err(ApiError::invalid(format!(
            "successor.{field} must be a trimmed, non-empty string"
        ))),
        _ => Ok(()),
    }
}

impl StartRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if !valid_text(&self.request_id, 256) {
            return Err(ApiError::invalid("requestId must be a non-empty string"));
        }
        if crate::agent_management::ProjectId::new(self.project_id.clone()).is_err() {
            return Err(ApiError::invalid("projectId is not a valid project ID"));
        }
        if !valid_text(&self.expected_director_cutex_session_id, 256) {
            return Err(ApiError::invalid("expectedDirectorCutexSessionId is required"));
        }
        if self.expected_authority_epoch == 0 {
            return Err(ApiError::invalid("expectedAuthorityEpoch must be at least 1"));
        }
        if let Some(timeout) = self.handoff_timeout_secs {
            if !(MIN_HANDOFF_TIMEOUT_SECS..=MAX_HANDOFF_TIMEOUT_SECS).contains(&timeout) {
                return Err(ApiError::invalid(format!(
                    "handoffTimeoutSecs must be between {MIN_HANDOFF_TIMEOUT_SECS} and {MAX_HANDOFF_TIMEOUT_SECS}"
                )));
            }
        }
        if let Some(name) = &self.successor.name {
            if !valid_text(name, MAX_NAME_CHARS) {
                return Err(ApiError::invalid(
                    "successor.name must be a trimmed, non-empty name of at most 128 characters",
                ));
            }
        }
        optional_text(&self.successor.profile, "profile")?;
        optional_text(&self.successor.model, "model")?;
        optional_text(&self.successor.reasoning_effort, "reasoningEffort")?;
        if let Some(cwd) = &self.successor.cwd {
            if cwd.is_empty() || cwd.chars().any(char::is_control) || !Path::new(cwd).is_absolute() {
                return Err(ApiError::invalid("successor.cwd must be an absolute path"));
            }
        }
        Ok(())
    }
}

/// The message that asks the Director for the handoff note.
pub fn handoff_request_text(rotation: &Rotation) -> String {
    format!(
        "[owner · Director rotation {id}]\n\
         The owner is replacing you as the Director of Project {project} ({project_id}). \
         Your successor will be {successor}.\n\n\
         1. Do not start new work, delegations or child agents.\n\
         2. Write a handoff note for your successor to:\n   {path}\n   \
         (create the directory if it does not exist). Cover: the current goals and their status; \
         open delegations you requested and who handles them; running child agents and jobs; \
         decisions made and open questions; where the important files and notes are.\n\
         3. When the file is complete, end your turn with a one-line reply.\n\n\
         After the rotation you will {after}.",
        id = rotation.rotation_id,
        project = rotation.project_name,
        project_id = rotation.project_id,
        successor = rotation.successor_spec.name,
        path = rotation.handoff.path,
        after = rotation.disposition.sentence(),
    )
}

/// The successor's first message.
pub fn successor_text(rotation: &Rotation, handoff_exists: bool) -> String {
    let predecessor = &rotation.predecessor;
    let after = match rotation.disposition {
        PredecessorDisposition::Archive => "was archived".to_string(),
        PredecessorDisposition::Member => "stays in the Project as a member".to_string(),
        PredecessorDisposition::Operator => {
            "stays in the Project as an Agent Operator".to_string()
        }
    };
    let note = if rotation.handoff.written_at.is_some() {
        format!(
            "Read the handoff note first: {}\nThen continue the work it describes.",
            rotation.handoff.path
        )
    } else if handoff_exists {
        format!(
            "The owner continued before the handoff was confirmed; the note may be incomplete: {}\n\
             Read it, then check the project memory for anything missing.",
            rotation.handoff.path
        )
    } else {
        format!(
            "The owner continued without a handoff note (none was written at {}). \
             Check the project memory and ask the owner{} for context.",
            rotation.handoff.path,
            if rotation.disposition == PredecessorDisposition::Archive {
                String::new()
            } else {
                format!(" or {}", predecessor.name)
            }
        )
    };
    format!(
        "[owner · Director rotation {id}]\n\
         You are now the Director of Project {project} ({project_id}), taking over from {name} \
         ({predecessor_id}), who {after}.\n{note}",
        id = rotation.rotation_id,
        project = rotation.project_name,
        project_id = rotation.project_id,
        name = predecessor.name,
        predecessor_id = predecessor.cutex_session_id,
    )
}

fn effect_text(error: &EffectError) -> String {
    format!("{}: {}", error.code, error.message)
}

// ---------------------------------------------------------------------------
// Driver (state machine)

pub struct Driver<E: RotationEffects> {
    pub store: RotationStore,
    pub effects: E,
    /// Consecutive polls that saw the handoff file and no turn (in memory:
    /// a restart only costs one more poll).
    ready_polls: Mutex<HashMap<String, u32>>,
}

impl<E: RotationEffects> Driver<E> {
    pub fn new(store: RotationStore, effects: E) -> Self {
        Self {
            store,
            effects,
            ready_polls: Mutex::new(HashMap::new()),
        }
    }

    fn now(&self) -> (DateTime<Utc>, String) {
        let now = self.effects.now();
        (now, rfc3339(now))
    }

    /// Validate and record a new rotation (or replay an identical request).
    pub fn start(&self, request: &StartRequest) -> Result<(Rotation, bool), ApiError> {
        request.validate()?;
        let digest = super::native_requests::canonical_request_sha256(
            &serde_json::to_value(request).map_err(|error| ApiError::invalid(error.to_string()))?,
        )
        .map_err(|error| ApiError::invalid(error.to_string()))?;
        if let Some(existing) = self
            .store
            .list(None, usize::MAX)
            .map_err(ApiError::unavailable)?
            .into_iter()
            .find(|rotation| rotation.request_id == request.request_id)
        {
            return if existing.request_sha256 == digest {
                Ok((existing, false))
            } else {
                Err(ApiError::new(
                    409,
                    "idempotency_conflict",
                    "requestId was already used with a different request body",
                ))
            };
        }
        let effect_error = |error: EffectError| ApiError {
            status: if error.retryable { 503 } else { 500 },
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        };
        let project = self
            .effects
            .project(&request.project_id)
            .map_err(effect_error)?
            .ok_or_else(|| ApiError::new(404, "project_not_found", "the Project does not exist on this host"))?;
        if !project.active {
            return Err(ApiError::new(409, "project_not_active", "the Project is archived"));
        }
        let director = project.director.clone().ok_or_else(|| {
            ApiError::new(409, "project_has_no_director", "the Project has no Director to rotate")
        })?;
        if director != request.expected_director_cutex_session_id
            || project.authority_epoch != request.expected_authority_epoch
        {
            return Err(ApiError::new(
                409,
                "stale_project_authority",
                format!(
                    "the Project's Director is {director} at epoch {}; reload and try again",
                    project.authority_epoch
                ),
            ));
        }
        let predecessor = self
            .effects
            .agent(&director)
            .map_err(effect_error)?
            .ok_or_else(|| ApiError::new(409, "director_not_found", "the Director's session record is missing"))?;
        let input = &request.successor;
        let cwd = input.cwd.clone().unwrap_or_else(|| predecessor.cwd.clone());
        if !Path::new(&cwd).is_absolute() {
            return Err(ApiError::invalid("successor.cwd must be an absolute path"));
        }
        let name = input
            .name
            .clone()
            .unwrap_or_else(|| next_successor_name(&predecessor.name));
        if name == predecessor.name {
            return Err(ApiError::invalid("the successor needs a name of its own"));
        }
        // Model / effort are inherited only while the profile is the predecessor's.
        let inherit = input.profile.is_none() || input.profile == predecessor.profile;
        let spec = SuccessorSpec {
            name,
            profile: input.profile.clone().or_else(|| predecessor.profile.clone()),
            model: input
                .model
                .clone()
                .or_else(|| inherit.then(|| predecessor.model.clone()).flatten()),
            reasoning_effort: input
                .reasoning_effort
                .clone()
                .or_else(|| inherit.then(|| predecessor.reasoning_effort.clone()).flatten()),
            cwd,
            sandbox: predecessor.sandbox.clone(),
            approval_policy: predecessor.approval_policy.clone(),
        };
        let (now, now_text) = self.now();
        let base = project
            .workspace_root
            .clone()
            .unwrap_or_else(|| predecessor.cwd.clone());
        let rotation_id = format!("rot-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let timeout_secs = request
            .handoff_timeout_secs
            .unwrap_or(DEFAULT_HANDOFF_TIMEOUT_SECS);
        let rotation = Rotation {
            rotation_id: rotation_id.clone(),
            request_id: request.request_id.clone(),
            request_sha256: digest.clone(),
            project_id: project.project_id.clone(),
            project_name: project.display_name.clone(),
            state: RotationState::Requested,
            predecessor: AgentRef {
                cutex_session_id: director.clone(),
                name: predecessor.name.clone(),
            },
            expected_authority_epoch: request.expected_authority_epoch,
            disposition: request.predecessor,
            successor_spec: spec,
            handoff: Handoff {
                path: handoff_path(&base, now),
                timeout_secs,
                client_user_message_id: format!("owner-rotation-{rotation_id}-handoff"),
                requested_at: None,
                deadline: None,
                written_at: None,
                skipped: false,
            },
            successor_cutex_session_id: None,
            successor_create_started_at: None,
            authority_epoch: None,
            predecessor_delegations: self.effects.delegations(&director),
            attempts: 0,
            error: None,
            warnings: Vec::new(),
            history: vec![HistoryEntry {
                state: RotationState::Requested,
                at: now_text.clone(),
                note: None,
            }],
            created_at: now_text.clone(),
            updated_at: now_text,
            finished_at: None,
            revision: 1,
        };
        self.store.insert(|existing| {
            if let Some(replay) = existing
                .iter()
                .find(|other| other.request_id == request.request_id)
            {
                return if replay.request_sha256 == digest {
                    Ok(Err(replay.clone()))
                } else {
                    Err(ApiError::new(409, "idempotency_conflict", "requestId was already used with a different request body"))
                };
            }
            let busy = existing.iter().find(|other| {
                !other.state.is_terminal()
                    && (other.project_id == rotation.project_id
                        || other.predecessor.cutex_session_id == director
                        || other.successor_cutex_session_id.as_deref() == Some(director.as_str()))
            });
            if let Some(busy) = busy {
                return Err(ApiError::new(
                    409,
                    "rotation_in_progress",
                    format!("rotation {} is still running for this Project or Director", busy.rotation_id),
                ));
            }
            Ok(Ok(rotation))
        })
    }

    pub fn cancel(&self, rotation_id: &str) -> Result<Rotation, ApiError> {
        let (_, now) = self.now();
        let outcome = self
            .store
            .modify(rotation_id, &now, |rotation| {
                if rotation.state == RotationState::Cancelled {
                    return Ok(());
                }
                if !rotation.cancellable() {
                    return Err(ApiError::new(
                        409,
                        "rotation_not_cancellable",
                        format!("the rotation is {:?}; it can be cancelled only before the successor is created", rotation.state),
                    ));
                }
                rotation.transition(RotationState::Cancelled, &now, Some("cancelled by the owner".into()));
                Ok(())
            })
            .map_err(ApiError::unavailable)?
            .ok_or_else(|| ApiError::new(404, "rotation_not_found", "no such rotation"))?;
        outcome.1.map(|()| outcome.0)
    }

    pub fn continue_without_handoff(&self, rotation_id: &str) -> Result<Rotation, ApiError> {
        let (_, now) = self.now();
        let outcome = self
            .store
            .modify(rotation_id, &now, |rotation| {
                if rotation.handoff.skipped {
                    return Ok(());
                }
                if !rotation.state.is_waiting() {
                    return Err(ApiError::new(
                        409,
                        "rotation_not_waiting",
                        "the rotation is not waiting for the handoff",
                    ));
                }
                rotation.handoff.skipped = true;
                let state = rotation.state;
                rotation.history.push(HistoryEntry {
                    state,
                    at: now.clone(),
                    note: Some("continue without handoff".into()),
                });
                Ok(())
            })
            .map_err(ApiError::unavailable)?
            .ok_or_else(|| ApiError::new(404, "rotation_not_found", "no such rotation"))?;
        outcome.1.map(|()| outcome.0)
    }

    /// Advance every active rotation as far as it goes now.
    pub fn tick(&self) {
        let ids = match self.store.active_ids() {
            Ok(ids) => ids,
            Err(error) => {
                eprintln!("owner rotation: state unavailable: {error:#}");
                return;
            }
        };
        for id in ids {
            for _ in 0..16 {
                match self.step(&id) {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(error) => {
                        eprintln!("owner rotation {id}: {error:#}");
                        break;
                    }
                }
            }
        }
    }

    /// One step; `true` when the next step can follow immediately.
    pub fn step(&self, rotation_id: &str) -> anyhow::Result<bool> {
        let Some(rotation) = self.store.get(rotation_id)? else {
            return Ok(false);
        };
        match rotation.state {
            RotationState::Requested => self.request_handoff(&rotation),
            RotationState::HandoffRequested | RotationState::Timeout => {
                if rotation.handoff.skipped {
                    self.create_successor(&rotation)
                } else {
                    self.watch_handoff(&rotation)
                }
            }
            RotationState::HandoffWritten => self.create_successor(&rotation),
            RotationState::SuccessorCreated => self.rebind(&rotation),
            RotationState::AuthorityRebound => self.dispose(&rotation),
            RotationState::PredecessorDisposed => self.start_successor(&rotation),
            RotationState::Done | RotationState::Cancelled | RotationState::Failed => Ok(false),
        }
    }

    fn request_handoff(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let result = self.effects.send_message(
            &rotation.predecessor.cutex_session_id,
            &rotation.handoff.client_user_message_id,
            &handoff_request_text(rotation),
        );
        let (now, now_text) = self.now();
        let deadline = rfc3339(now + ChronoDuration::seconds(rotation.handoff.timeout_secs as i64));
        let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.state != RotationState::Requested {
                return false;
            }
            match &result {
                Ok(()) => {
                    current.handoff.requested_at = Some(now_text.clone());
                    current.handoff.deadline = Some(deadline);
                    current.transition(RotationState::HandoffRequested, &now_text, None);
                    false
                }
                Err(error) => {
                    current.retry_or_fail("handoff_request_failed", error, &now_text);
                    false
                }
            }
        })?;
        Ok(changed.is_some_and(|(_, next)| next))
    }

    fn watch_handoff(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let ready = self.effects.handoff_ready(Path::new(&rotation.handoff.path))
            && matches!(
                self.effects.turn_active(&rotation.predecessor.cutex_session_id),
                Ok(false)
            );
        let polls = {
            let mut map = self
                .ready_polls
                .lock()
                .map_err(|_| anyhow::anyhow!("ready poll map poisoned"))?;
            let entry = map.entry(rotation.rotation_id.clone()).or_insert(0);
            *entry = if ready { *entry + 1 } else { 0 };
            *entry
        };
        let (now, now_text) = self.now();
        let written = polls >= STABLE_POLLS;
        let expired = rotation
            .handoff
            .deadline
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|deadline| now >= deadline);
        let state = rotation.state;
        let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.state != state || current.handoff.skipped {
                return false;
            }
            if written {
                current.handoff.written_at = Some(now_text.clone());
                current.transition(RotationState::HandoffWritten, &now_text, None);
                return true;
            }
            if current.state == RotationState::HandoffRequested && expired && !ready {
                current.transition(
                    RotationState::Timeout,
                    &now_text,
                    Some("no handoff before the deadline".into()),
                );
            }
            false
        })?;
        if written {
            if let Ok(mut map) = self.ready_polls.lock() {
                map.remove(&rotation.rotation_id);
            }
        }
        Ok(changed.is_some_and(|(_, next)| next))
    }

    fn create_successor(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let (_, now_text) = self.now();
        if let Some(started) = rotation.successor_create_started_at.clone() {
            // A creation that a previous process started and did not record.
            let found = self
                .effects
                .find_agent_by_name(&rotation.successor_spec.name, &started);
            let state = rotation.state;
            let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
                if current.state != state || current.successor_cutex_session_id.is_some() {
                    return false;
                }
                match &found {
                    Ok(Some(id)) => {
                        current.successor_cutex_session_id = Some(id.clone());
                        current.transition(
                            RotationState::SuccessorCreated,
                            &now_text,
                            Some("successor found after an interrupted creation".into()),
                        );
                        true
                    }
                    Ok(None) => {
                        current.fail(
                            "successor_creation_interrupted",
                            format!(
                                "the successor creation was interrupted and no agent named {} exists; \
                                 the Director is unchanged, start a new rotation",
                                current.successor_spec.name
                            ),
                            &now_text,
                        );
                        false
                    }
                    Err(error) => {
                        current.retry_or_fail("successor_lookup_failed", error, &now_text);
                        false
                    }
                }
            })?;
            return Ok(changed.is_some_and(|(_, next)| next));
        }
        // Reserve durably first: cancel is refused from here on.
        let state = rotation.state;
        let reserved = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            let eligible = current.state == state
                && current.successor_create_started_at.is_none()
                && (current.state == RotationState::HandoffWritten || current.handoff.skipped);
            if eligible {
                current.successor_create_started_at = Some(now_text.clone());
            }
            eligible
        })?;
        let Some((reserved, true)) = reserved else {
            return Ok(false);
        };
        let result = self.effects.create_successor(&reserved);
        let (_, now_text) = self.now();
        let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.successor_cutex_session_id.is_some() || current.state.is_terminal() {
                return false;
            }
            match &result {
                Ok(id) => {
                    current.successor_cutex_session_id = Some(id.clone());
                    current.transition(RotationState::SuccessorCreated, &now_text, None);
                    true
                }
                Err(error) => {
                    current.fail(
                        "successor_create_failed",
                        format!("{}; the Director is unchanged", effect_text(error)),
                        &now_text,
                    );
                    false
                }
            }
        })?;
        Ok(changed.is_some_and(|(_, next)| next))
    }

    fn rebind(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let Some(successor) = rotation.successor_cutex_session_id.clone() else {
            let (_, now_text) = self.now();
            self.store.modify(&rotation.rotation_id, &now_text, |current| {
                current.fail("invalid_rotation_state", "successor_created without a successor".into(), &now_text)
            })?;
            return Ok(false);
        };
        let result = self.effects.rebind(
            &rotation.project_id,
            &format!("owner-rotation:{}:rebind", rotation.rotation_id),
            rotation.expected_authority_epoch,
            &successor,
        );
        let (_, now_text) = self.now();
        let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.state != RotationState::SuccessorCreated {
                return false;
            }
            match &result {
                Ok(epoch) => {
                    current.authority_epoch = Some(*epoch);
                    current.transition(RotationState::AuthorityRebound, &now_text, None);
                    true
                }
                Err(error) if error.retryable => {
                    current.retry_or_fail("rebind_failed", error, &now_text);
                    false
                }
                Err(error) => {
                    let name = current.successor_spec.name.clone();
                    current.fail(
                        "rebind_failed",
                        format!(
                            "{}; the successor {name} ({successor}) was created and stays a member of the Project",
                            effect_text(error)
                        ),
                        &now_text,
                    );
                    false
                }
            }
        })?;
        Ok(changed.is_some_and(|(_, next)| next))
    }

    fn dispose(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let predecessor = &rotation.predecessor.cutex_session_id;
        let base = format!("owner-rotation:{}", rotation.rotation_id);
        let result = match rotation.disposition {
            PredecessorDisposition::Archive => self.effects.archive(
                predecessor,
                &format!("owner-rotation-{}-archive", rotation.rotation_id),
                &format!("Director rotation {}", rotation.rotation_id),
            ),
            PredecessorDisposition::Member => {
                self.effects
                    .ensure_member(&rotation.project_id, predecessor, &format!("{base}:member"))
            }
            PredecessorDisposition::Operator => self
                .effects
                .ensure_member(&rotation.project_id, predecessor, &format!("{base}:member"))
                .and_then(|()| {
                    self.effects.grant_operator(
                        &rotation.project_id,
                        predecessor,
                        &format!("{base}:operator"),
                    )
                }),
        };
        let (_, now_text) = self.now();
        let changed = self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.state != RotationState::AuthorityRebound {
                return false;
            }
            match &result {
                Ok(()) => {
                    current.transition(RotationState::PredecessorDisposed, &now_text, None);
                    true
                }
                Err(error) if error.retryable && current.attempts + 1 < MAX_STEP_ATTEMPTS => {
                    current.attempts += 1;
                    false
                }
                // The new Director is in place: a failed disposal is a
                // warning, never a reason to keep the successor stopped.
                Err(error) => {
                    let warning = format!("predecessor_dispose_failed: {}", effect_text(error));
                    current.warnings.push(warning.clone());
                    current.transition(RotationState::PredecessorDisposed, &now_text, Some(warning));
                    true
                }
            }
        })?;
        Ok(changed.is_some_and(|(_, next)| next))
    }

    fn start_successor(&self, rotation: &Rotation) -> anyhow::Result<bool> {
        let Some(successor) = rotation.successor_cutex_session_id.clone() else {
            return Ok(false);
        };
        let exists = self.effects.handoff_ready(Path::new(&rotation.handoff.path));
        let result = self.effects.send_message(
            &successor,
            &format!("owner-rotation-{}-successor", rotation.rotation_id),
            &successor_text(rotation, exists),
        );
        let (_, now_text) = self.now();
        self.store.modify(&rotation.rotation_id, &now_text, |current| {
            if current.state != RotationState::PredecessorDisposed {
                return;
            }
            match &result {
                Ok(()) => current.transition(RotationState::Done, &now_text, None),
                Err(error) if error.retryable && current.attempts + 1 < MAX_STEP_ATTEMPTS => {
                    current.attempts += 1;
                }
                Err(error) => {
                    let warning = format!(
                        "successor_start_failed: {}; send the successor its first message by hand",
                        effect_text(error)
                    );
                    current.warnings.push(warning.clone());
                    current.transition(RotationState::Done, &now_text, Some(warning));
                }
            }
        })?;
        Ok(false)
    }
}

/// `{rotation}` event / response body.
pub fn rotation_body(rotation: &Rotation) -> Value {
    serde_json::json!({ "rotation": rotation })
}

#[cfg(test)]
#[path = "owner_rotation_tests.rs"]
mod tests;
