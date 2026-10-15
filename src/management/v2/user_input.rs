use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::management::v2::model::MAX_SAFE_SEQUENCE;
use crate::management::v2::native_requests::canonical_request_sha256;

const USER_INPUT_STATE_FILE: &str = "user-input-state.json";
const USER_INPUT_LOCK_FILE: &str = "user-input-state.lock";

static USER_INPUT_REPOSITORY: super::process_repository::ProcessRepository<UserInputRepository> =
    super::process_repository::ProcessRepository::new();

/// Shared by native submit/flush and Management queue mutations. Keep the
/// reservation, native send and queue completion in one session critical section.
pub fn session_input_lock(session: &str) -> anyhow::Result<std::sync::Arc<Mutex<()>>> {
    static LOCKS: std::sync::OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("app-server session input lock registry was poisoned"))?;
    Ok(locks
        .entry(session.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRevisionConflict {
    pub expected: u64,
    pub current: u64,
}

impl std::fmt::Display for QueueRevisionConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "queue revision conflict: expected {}, current {}",
            self.expected, self.current
        )
    }
}

impl std::error::Error for QueueRevisionConflict {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueMoveError {
    NotFound(String),
    AdmissionNotFound(String),
    DeliveryBlocked(String),
    PassingNotAllowed,
}

impl std::fmt::Display for QueueMoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PassingNotAllowed => write!(f, "moving ahead of a non-owner item is disabled"),
            Self::AdmissionNotFound(id) => write!(f, "admission item {id} does not exist"),
            Self::NotFound(id) => write!(f, "queue item {id} does not exist"),
            Self::DeliveryBlocked(id) => write!(f, "queue item {id} already has a native delivery"),
        }
    }
}

impl std::error::Error for QueueMoveError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserInputOriginKind {
    Android,
    Backend,
    Tui,
    AgentBus,
    Automation,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserInputOrigin {
    pub kind: UserInputOriginKind,
    pub client_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserInputStrategy {
    Auto,
    Queue,
    Interrupt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInputSubmitParams {
    pub client_user_message_id: String,
    pub origin: UserInputOrigin,
    pub strategy: UserInputStrategy,
    pub input: Vec<Value>,
    pub expected_turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInputSubmitCommand {
    pub management_request_id: String,
    pub cutex_session_id: String,
    pub thread_id: String,
    pub params: UserInputSubmitParams,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserInputDisposition {
    Started,
    Steered,
    Queued,
    Deduplicated,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInputSubmitExecution {
    pub disposition: UserInputDisposition,
    pub app_server_accepted: bool,
    pub native_request_id: Option<Value>,
    pub native_method: Option<String>,
    pub turn_id: Option<String>,
    pub queue: Option<UserInputQueueItem>,
    /// The item waits in a paused queue (`cutex/turn/stop`): it goes out
    /// only after `cutex/userInput/queue/resume` or an explicit flush.
    /// Result field `paused` (with `queue`).
    pub queue_paused: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInputExecutionError {
    pub stage: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
    pub outcome_unknown: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQueueItem {
    pub queue_id: String,
    pub client_user_message_id: String,
    pub origin: UserInputOrigin,
    pub input: Vec<Value>,
    pub position: u64,
    pub revision: u64,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    /// Sent even while the queue is paused: input that `startIfOffline`
    /// queued only because the runtime was offline (it is not a queued
    /// follow-up, so a stop must not hold it back).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass_pause: bool,
    /// Listing only (never stored): `sending` / `unknown` while an earlier
    /// native send of this item is unresolved. Such an item is not resent
    /// automatically and holds the queue until it resolves or is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
}

/// Why and when a session's queue stopped auto-flushing. While a pause is
/// recorded, Cutex never starts a queued item by itself (turn completion,
/// idle status and runtime connect all skip it, except `bypass_pause` items);
/// an explicit `cutex/userInput/queue/flush` still sends the front item, and
/// `cutex/userInput/queue/resume` clears the pause. An emptied queue drops
/// its pause. Persisted with the queue, so it survives daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQueuePause {
    pub paused_at: String,
    /// `interrupted` (`cutex/turn/stop`) or `manual` (`cutex/userInput/queue/pause`).
    pub reason: String,
    #[serde(default)]
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueuePauseDecision {
    Paused {
        pause: UserInputQueuePause,
        revision: u64,
    },
    AlreadyPaused {
        pause: UserInputQueuePause,
        revision: u64,
    },
    /// Nothing queued: there is nothing to hold back, so no pause is recorded.
    Empty { revision: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueEnqueueDecision {
    Queued(UserInputQueueItem),
    Deduplicated(UserInputQueueItem),
    ClientMessageConflict,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientIdentityDecision {
    New,
    Existing,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredQueueItem {
    management_request_id: String,
    thread_id: String,
    item: UserInputQueueItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientInputIdentity {
    canonical_sha256: String,
    first_seen_at: String,
    /// Native delivery of this input (C-010). Absent: never sent to native,
    /// or the last send was refused. Older cutex versions ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<InputDelivery>,
}

/// What cutex knows about delivering one clientUserMessageId to native.
///
/// The native history alone cannot deduplicate a retry: right after
/// `turn/start` / `turn/steer` is accepted, `thread/read` can list the turn
/// before it lists the userMessage carrying the clientId, so a retry in that
/// window was sent again (Kutex C-010). The delivery is recorded under the
/// user-input store lock: `sending` *before* the native request goes out
/// (the reservation), then `accepted` or `unknown`, or cleared on a definite
/// refusal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum InputDelivery {
    /// A native request is (or was, if cutex died) in flight.
    #[serde(rename_all = "camelCase")]
    Sending { thread_id: String, since: String },
    /// Native accepted it into `turn_id` (None: native returned no turn id).
    #[serde(rename_all = "camelCase")]
    Accepted {
        thread_id: String,
        turn_id: Option<String>,
        accepted_at: String,
    },
    /// The native request may or may not have been accepted (transport
    /// failure, timeout, or the acceptance could not be recorded).
    #[serde(rename_all = "camelCase")]
    Unknown { thread_id: String, since: String },
}

impl InputDelivery {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Sending { thread_id, .. }
            | Self::Accepted { thread_id, .. }
            | Self::Unknown { thread_id, .. } => thread_id,
        }
    }
}

/// A queued item whose input already went (or may have gone) to native
/// cannot be edited: `409 queue_input_already_submitted`.
#[derive(Debug)]
pub struct QueueInputAlreadySubmitted;

impl std::fmt::Display for QueueInputAlreadySubmitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the queued input was already sent to the native runtime (or its delivery is unresolved); it cannot be edited",
        )
    }
}

impl std::error::Error for QueueInputAlreadySubmitted {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionInputState {
    queue_revision: u64,
    #[serde(default)]
    admission: super::admission::StoredAdmission,
    #[serde(default)]
    items: Vec<StoredQueueItem>,
    #[serde(default)]
    identities: BTreeMap<String, ClientInputIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause: Option<UserInputQueuePause>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInputStateStore {
    version: u8,
    #[serde(default)]
    sessions: BTreeMap<String, SessionInputState>,
}

impl Default for UserInputStateStore {
    fn default() -> Self {
        Self {
            version: 1,
            sessions: BTreeMap::new(),
        }
    }
}

pub struct UserInputRepository {
    root: PathBuf,
    asks_dir: PathBuf,
    host_queue: bool,
    process_lock: Mutex<()>,
}

pub fn user_input_repository() -> anyhow::Result<&'static UserInputRepository> {
    USER_INPUT_REPOSITORY.get_or_open(
        || Ok(runtime_dir()?.join("management-v2")),
        |root| { let asks = runtime_dir()?.join("asks"); UserInputRepository::open_with_asks(root, asks, true) },
    )
}

pub fn parse_user_input_submit_params(value: &Value) -> Result<UserInputSubmitParams, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "cutex/userInput/submit params must be an object".to_string())?;
    let allowed = [
        "clientUserMessageId",
        "origin",
        "strategy",
        "input",
        "expectedTurnId",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || !["clientUserMessageId", "origin", "strategy", "input"]
            .iter()
            .all(|key| object.contains_key(*key))
    {
        return Err("cutex/userInput/submit params contain missing or unknown fields".to_string());
    }
    let client_user_message_id = object
        .get("clientUserMessageId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "clientUserMessageId must be a non-empty string".to_string())?
        .to_string();
    let origin: UserInputOrigin = serde_json::from_value(
        object
            .get("origin")
            .cloned()
            .ok_or_else(|| "origin is required".to_string())?,
    )
    .map_err(|error| format!("invalid origin: {error}"))?;
    let origin_object = object
        .get("origin")
        .and_then(Value::as_object)
        .ok_or_else(|| "origin must be an object".to_string())?;
    if origin_object.len() != 2
        || !origin_object.contains_key("kind")
        || !origin_object.contains_key("clientId")
        || origin.client_id.is_empty()
    {
        return Err("origin requires exactly non-empty kind and clientId".to_string());
    }
    let strategy: UserInputStrategy = serde_json::from_value(
        object
            .get("strategy")
            .cloned()
            .ok_or_else(|| "strategy is required".to_string())?,
    )
    .map_err(|error| format!("invalid strategy: {error}"))?;
    let input = object
        .get("input")
        .and_then(Value::as_array)
        .filter(|input| !input.is_empty())
        .ok_or_else(|| "input must be a non-empty array".to_string())?
        .clone();
    for item in &input {
        validate_user_input(item)?;
    }
    let expected_turn_id = match object.get("expectedTurnId") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(_) => {
            return Err("expectedTurnId must be a non-empty string or null".to_string());
        }
    };
    Ok(UserInputSubmitParams {
        client_user_message_id,
        origin,
        strategy,
        input,
        expected_turn_id,
    })
}

pub fn validate_user_input(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "each input item must be an object".to_string())?;
    let input_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "each input item requires a string type".to_string())?;
    let required_string = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(|_| ())
            .ok_or_else(|| format!("{input_type} input requires non-empty {key}"))
    };
    match input_type {
        "text" => {
            if object.get("text").and_then(Value::as_str).is_none() {
                return Err("text input requires string text".to_string());
            }
            if object
                .get("text_elements")
                .is_some_and(|elements| !elements.is_array())
            {
                return Err("text_elements must be an array when present".to_string());
            }
        }
        "image" => required_string("url")?,
        "localImage" => required_string("path")?,
        "skill" | "mention" => {
            required_string("name")?;
            required_string("path")?;
        }
        _ => return Err(format!("unsupported native UserInput type: {input_type}")),
    }
    if matches!(input_type, "image" | "localImage")
        && object
            .get("detail")
            .is_some_and(|detail| !detail.is_null() && !detail.is_string())
    {
        return Err("image detail must be a string or null".to_string());
    }
    Ok(())
}

impl UserInputRepository {
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        let asks = root.join("asks");
        Self::open_with_asks(root, asks, false)
    }

    pub(crate) fn open_with_asks(root: PathBuf, asks_dir: PathBuf, host_queue: bool) -> anyhow::Result<Self> {
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        Ok(Self {
            root,
            asks_dir,
            host_queue,
            process_lock: Mutex::new(()),
        })
    }

    pub fn register_identity(
        &self,
        cutex_session_id: &str,
        params: &UserInputSubmitParams,
    ) -> anyhow::Result<ClientIdentityDecision> {
        let identity_sha256 = input_identity_sha256(params)?;
        self.mutate(|store| {
            let state = store
                .sessions
                .entry(cutex_session_id.to_string())
                .or_default();
            match state.identities.get(&params.client_user_message_id) {
                Some(identity) if identity.canonical_sha256 == identity_sha256 => {
                    Ok(ClientIdentityDecision::Existing)
                }
                Some(_) => Ok(ClientIdentityDecision::Conflict),
                None => {
                    state.identities.insert(
                        params.client_user_message_id.clone(),
                        ClientInputIdentity {
                            canonical_sha256: identity_sha256,
                            first_seen_at: Utc::now().to_rfc3339(),
                            delivery: None,
                        },
                    );
                    Ok(ClientIdentityDecision::New)
                }
            }
        })
    }

    pub fn enqueue(
        &self,
        cutex_session_id: &str,
        thread_id: &str,
        management_request_id: &str,
        params: &UserInputSubmitParams,
    ) -> anyhow::Result<QueueEnqueueDecision> {
        let identity_sha256 = input_identity_sha256(params)?;
        self.ask_store().inspect(|asks| self.mutate(|store| {
            let state = store
                .sessions
                .entry(cutex_session_id.to_string())
                .or_default();
            self.sync_admission(cutex_session_id, state, asks)?;
            if let Some(identity) = state.identities.get(&params.client_user_message_id) {
                if identity.canonical_sha256 != identity_sha256 {
                    return Ok(QueueEnqueueDecision::ClientMessageConflict);
                }
            } else {
                state.identities.insert(
                    params.client_user_message_id.clone(),
                    ClientInputIdentity {
                        canonical_sha256: identity_sha256,
                        first_seen_at: Utc::now().to_rfc3339(),
                        delivery: None,
                    },
                );
            }
            if let Some((position, existing)) = state
                .items
                .iter()
                .enumerate()
                .find(|(_, stored)| stored.item.client_user_message_id == params.client_user_message_id)
            {
                let mut item = existing.item.clone();
                item.position = position as u64;
                return Ok(QueueEnqueueDecision::Deduplicated(item));
            }
            state.queue_revision = next_revision(state.queue_revision)?;
            let now = Utc::now().to_rfc3339();
            let item = UserInputQueueItem {
                queue_id: Uuid::new_v4().to_string(),
                client_user_message_id: params.client_user_message_id.clone(),
                origin: params.origin.clone(),
                input: params.input.clone(),
                position: state.items.len() as u64,
                revision: 1,
                state: "queued".to_string(),
                created_at: now.clone(),
                updated_at: now,
                bypass_pause: false,
                delivery: None,
            };
            state.items.push(StoredQueueItem {
                management_request_id: management_request_id.to_string(),
                thread_id: thread_id.to_string(),
                item: item.clone(),
            });
            self.sync_admission(cutex_session_id, state, asks)?;
            Ok(QueueEnqueueDecision::Queued(item))
        }))?
    }

    /// The recorded native delivery of `client_user_message_id`, if any.
    pub fn delivery(
        &self,
        cutex_session_id: &str,
        client_user_message_id: &str,
    ) -> anyhow::Result<Option<InputDelivery>> {
        self.read(|store| {
            Ok(store
                .sessions
                .get(cutex_session_id)
                .and_then(|state| state.identities.get(client_user_message_id))
                .and_then(|identity| identity.delivery.clone()))
        })
    }

    /// Compare-and-set of the native delivery in one locked read-modify-write:
    /// `Ok(false)` (nothing written) when the current delivery is not
    /// `expected`. Reserving a send is `expected: None, new: Some(Sending)`,
    /// so of two concurrent senders only one gets `true`. Errors when the
    /// identity is not registered (a delivery is only ever recorded for an
    /// input whose identity was registered first).
    pub fn set_delivery(
        &self,
        cutex_session_id: &str,
        client_user_message_id: &str,
        expected: Option<&InputDelivery>,
        new: Option<InputDelivery>,
    ) -> anyhow::Result<bool> {
        self.mutate(|store| {
            let identity = store
                .sessions
                .get_mut(cutex_session_id)
                .and_then(|state| state.identities.get_mut(client_user_message_id))
                .context("the user input has no registered clientUserMessageId")?;
            if identity.delivery.as_ref() != expected {
                return Ok(false);
            }
            identity.delivery = new;
            Ok(true)
        })
    }

    pub fn list(&self, cutex_session_id: &str) -> anyhow::Result<(u64, Vec<UserInputQueueItem>)> {
        self.read(|store| {
            let Some(state) = store.sessions.get(cutex_session_id) else {
                return Ok((0, Vec::new()));
            };
            Ok((state.queue_revision, positioned_items(state)))
        })
    }

    pub fn ask_store(&self) -> crate::asks::store::AskStore {
        crate::asks::store::AskStore::with_queue(self.asks_dir.clone(), self.root.clone(), self.host_queue)
    }

    /// Unobserved offline sessions need no background bus scan. Owner queues
    /// and online runtimes still require admission changes without a UI client.
    pub fn needs_admission_poll(&self) -> anyhow::Result<bool> {
        self.ask_store().inspect(|asks| {
            if asks.presence.values().any(|presence| presence.online) { return Ok(true); }
            self.read(|store| Ok(store.sessions.values().any(|session| !session.items.is_empty())))
        })?
    }

    pub fn owner_may_start(&self, session: &str, queue_id: Option<&str>) -> anyhow::Result<bool> {
        self.ask_store().inspect(|state| {
            let mut admission = self.admission_with_state(session, state)?;
            if super::admission::owner_priority(session) {
                admission.next_step.clear();
            } else {
                let waiting = !state.leases.contains_key(session)
                    && admission.next_step.iter().any(|item| item.eligible);
                if super::admission_wait::waiting(session, waiting) { admission.next_step.clear(); }
            }
            Ok(crate::asks::scheduler::owner_may_flush(state, session, &admission, queue_id))
        })?
    }

    pub fn list_admission(&self, session: &str) -> anyhow::Result<(u64, Vec<UserInputQueueItem>, Option<UserInputQueuePause>, super::admission::AdmissionSnapshot)> {
        self.ask_store().inspect(|asks| self.with_lock(|path| {
            let before = cached_store(path)?;
            let prior = before.sessions.get(session);
            // Admission synchronization never reads identities. In the usual
            // unchanged case, only copy this session's live queue/admission,
            // not every session's entire retained C-010 identity history.
            let mut projection = admission_projection(prior);
            let admission = self.sync_admission(session, &mut projection, asks)?;
            let changed = match prior {
                Some(prior) => serde_json::to_vec(&prior.admission)? != serde_json::to_vec(&projection.admission)?,
                None => true,
            };
            // Keep serialized comparison: next_step.eligible is deliberately
            // transient and must not cause a write or admission notification.
            if changed {
                let mut store = (*before).clone();
                store.sessions.entry(session.to_string()).or_default().admission = projection.admission;
                write_private_pretty_json_atomic(path, &store, "management v2 user-input state")?;
                super::admission::changed(session);
            }
            Ok((projection.queue_revision, prior.map(positioned_items).unwrap_or_default(), projection.pause, admission))
        }))?
    }

    pub fn admission(&self, session: &str) -> anyhow::Result<super::admission::AdmissionSnapshot> {
        self.ask_store().inspect(|asks| self.admission_with_state(session, asks))?
    }

    pub fn admission_with_state(&self, session: &str, asks: &crate::asks::store::AskState) -> anyhow::Result<super::admission::AdmissionSnapshot> {
        self.read(|store| {
            let mut state = admission_projection(store.sessions.get(session));
            self.sync_admission(session, &mut state, asks)
        })
    }

    /// Offline scheduling needs numbered order and holds, but cannot consume
    /// ridealongs. Avoid bus/config reads used only by the full presentation.
    pub(crate) fn numbered_admission_with_state(&self, session: &str, asks: &crate::asks::store::AskState) -> anyhow::Result<super::admission::AdmissionSnapshot> {
        self.read(|store| {
            let mut state = admission_projection(store.sessions.get(session));
            let mut admission = self.sync_numbered(session, &mut state, asks)?;
            admission.next_step.clear();
            admission.current_turn = None;
            admission.ridealongs_held = None;
            Ok(admission)
        })
    }

    /// AskStore calls this while holding its write lock. Every other path takes
    /// the ask lock before the owner lock, so registration and moves serialize.
    pub(crate) fn sync_asks(&self, before: &crate::asks::store::AskState, asks: &crate::asks::store::AskState) -> anyhow::Result<()> {
        let mut sessions = std::collections::BTreeSet::new();
        for (id, ask) in asks.asks.iter().chain(before.asks.iter()) {
            if before.asks.get(id) != asks.asks.get(id) { sessions.insert(ask.target_cutex_session_id.clone()); }
        }
        for (id, delivery) in asks.deliveries.iter().chain(before.deliveries.iter()) {
            if before.deliveries.get(id) != asks.deliveries.get(id) { sessions.insert(delivery.requester_cutex_session_id.clone()); }
        }
        for session in asks.pauses.keys().chain(before.pauses.keys()).chain(asks.leases.keys()).chain(before.leases.keys()) {
            if before.pauses.get(session) != asks.pauses.get(session) || before.leases.get(session) != asks.leases.get(session) { sessions.insert(session.clone()); }
        }
        if sessions.is_empty() { return Ok(()); }
        let approved: std::collections::BTreeSet<String> = asks.asks.iter().filter(|(id, ask)|
            ask.execution == crate::asks::model::ExecutionState::Queued
                && before.asks.get(*id).is_some_and(|prior| prior.execution == crate::asks::model::ExecutionState::PendingReview)
        ).map(|(id, _)| id.clone()).collect();
        self.with_lock(|path| {
            let mut store = (*cached_store(path)?).clone();
            let mut changed = false;
            for session in &sessions {
                // Only affected rows are inspected. The shared JSON file is
                // replaced once, without per-session scans or bus reads.
                let state = store.sessions.entry(session.clone()).or_default();
                let before = serde_json::to_vec(&state.admission)?;
                let items = self.numbered_items(session, state, asks);
                state.admission.priority_asks = asks.asks.values().filter(|ask| ask.target_cutex_session_id == *session && ask.resume_pending()).map(|ask| ask.ask_id.clone()).collect();
                state.admission.sync_approved(items, &approved)?;
                changed |= before != serde_json::to_vec(&state.admission)?;
            }
            if changed { write_private_pretty_json_atomic(path, &store, "management v2 user-input state")?; }
            Ok(())
        })?;
        for session in sessions { super::admission::changed(&session); }
        Ok(())
    }

    fn sync_numbered(&self, session: &str, state: &mut SessionInputState, asks: &crate::asks::store::AskState) -> anyhow::Result<super::admission::AdmissionSnapshot> {
        let items = self.numbered_items(session, state, asks);
        state.admission.priority_asks = asks.asks.values().filter(|ask| ask.target_cutex_session_id == session && ask.resume_pending()).map(|ask| ask.ask_id.clone()).collect();
        state.admission.sync(items)
    }

    fn numbered_items(&self, session: &str, state: &SessionInputState, asks: &crate::asks::store::AskState) -> Vec<super::admission::AdmissionItem> {
        let mut items = super::admission::ask_items(asks, session);
        for stored in &state.items {
            let item = &stored.item;
            items.push(super::admission::AdmissionItem {
                kind: "owner".into(), id: item.queue_id.clone(), position: 0, enqueued_at: item.created_at.clone(),
                state: if state.pause.is_some() && !item.bypass_pause { "paused" } else { "waiting" }.into(),
                hold: None, delivery_kind: None, label: None, included_notes: Vec::new(),
            });
        }
        for item in &mut items {
            if let Some(prior) = state.admission.snapshot.items.iter().find(|prior| prior.kind == item.kind && prior.id == item.id) {
                item.included_notes = prior.included_notes.clone();
            }
        }
        items
    }

    fn sync_admission(&self, session: &str, state: &mut SessionInputState, asks: &crate::asks::store::AskState) -> anyhow::Result<super::admission::AdmissionSnapshot> {
        self.sync_numbered(session, state, asks)?;
        if self.host_queue {
            let notes = super::agent_bus_state::agent_bus_message_repository()?.pending_admission_notes(session)?;
            let mut attached = false;
            let mut notes_changed = false;
            for item in &mut state.admission.snapshot.items {
                let expected = if !attached && item.kind == "ask" && (item.state == "waiting" || item.hold.as_deref() == Some("targetBusy")) {
                    attached = true;
                    notes.clone()
                } else { Vec::new() };
                if item.included_notes != expected { item.included_notes = expected; notes_changed = true; }
            }
            if notes_changed { state.admission.snapshot.revision = next_revision(state.admission.snapshot.revision)?; }
        }
        let mut next_step = super::admission::ask_next_step(asks, session);
        if self.host_queue {
            next_step.extend(super::agent_bus_state::agent_bus_message_repository()?.pending_admission_messages(session, asks)?);
        }
        next_step.sort_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at).then(a.id.cmp(&b.id)));
        let current_turn = if self.host_queue { super::admission::current_turn(session, asks) } else { None };
        let ridealongs_held = crate::asks::control::stop_held(asks, session)
            .then(|| crate::asks::control::HELD_OWNER_STOP.to_string());
        // The one-shot owner yield lives in this daemon's memory only.
        let owner_yielded = self.host_queue && super::admission::owner_yielded(session);
        let snapshot = &mut state.admission.snapshot;
        if serde_json::to_value(&snapshot.next_step)? != serde_json::to_value(&next_step)?
            || snapshot.current_turn != current_turn
            || snapshot.ridealongs_held != ridealongs_held
            || snapshot.owner_yielded != owner_yielded
        {
            snapshot.revision = next_revision(snapshot.revision)?;
        }
        snapshot.next_step = next_step;
        snapshot.current_turn = current_turn;
        snapshot.ridealongs_held = ridealongs_held;
        snapshot.owner_yielded = owner_yielded;
        Ok(snapshot.clone())
    }

    /// Only the moved owner's fractional key changes. The source queue vector
    /// follows that projection for existing flush/steer readers.
    pub fn move_admission_item(&self, session: &str, queue_id: &str, expected: u64,
        before: Option<&super::admission::AdmissionRef>, may_pass: bool,
    ) -> anyhow::Result<(super::admission::AdmissionSnapshot, u64, Vec<UserInputQueueItem>, bool)> {
        self.ask_store().inspect(|asks| self.mutate(|store| {
            let state = store.sessions.entry(session.to_string()).or_default();
            self.sync_admission(session, state, asks)?;
            if state.admission.snapshot.order_revision != expected {
                return Err(QueueRevisionConflict { expected, current: state.admission.snapshot.order_revision }.into());
            }
            let prior = state.admission.snapshot.clone();
            let changed = state.admission.move_owner(queue_id, before, may_pass)?;
            if changed {
                let moved = state.admission.snapshot.items.iter().position(|item| item.kind == "owner" && item.id == queue_id).expect("moved owner");
                let old = prior.items.iter().position(|item| item.kind == "owner" && item.id == queue_id).expect("original owner");
                for item in &state.items {
                    if state.identities.get(&item.item.client_user_message_id).is_some_and(|identity| identity.delivery.is_some()) {
                        let blocking = item.item.queue_id == queue_id || (moved < old && prior.items[moved..old].iter().any(|entry| entry.kind == "owner" && entry.id == item.item.queue_id));
                        if blocking { return Err(QueueMoveError::DeliveryBlocked(item.item.queue_id.clone()).into()); }
                    }
                }
                let owners: Vec<&str> = state.admission.snapshot.items.iter().filter(|item| item.kind == "owner").map(|item| item.id.as_str()).collect();
                state.items.sort_by_key(|stored| owners.iter().position(|id| *id == stored.item.queue_id).expect("registered owner"));
                state.queue_revision = next_revision(state.queue_revision)?;
            }
            Ok((state.admission.snapshot.clone(), state.queue_revision, positioned_items(state), changed))
        }))?
    }

    /// The queue's pause, if one is recorded.
    pub fn pause_state(&self, cutex_session_id: &str) -> anyhow::Result<Option<UserInputQueuePause>> {
        self.read(|store| {
            Ok(store
                .sessions
                .get(cutex_session_id)
                .and_then(|state| state.pause.clone()))
        })
    }

    /// Hold the queue back (no auto-flush) until `resume`. A queue with no
    /// items is not paused (`QueuePauseDecision::Empty`); an existing pause is
    /// kept as it is.
    pub fn pause(
        &self,
        cutex_session_id: &str,
        reason: &str,
        turn_id: Option<String>,
    ) -> anyhow::Result<QueuePauseDecision> {
        self.mutate(|store| {
            let Some(state) = store.sessions.get_mut(cutex_session_id) else {
                return Ok(QueuePauseDecision::Empty { revision: 0 });
            };
            if let Some(pause) = &state.pause {
                return Ok(QueuePauseDecision::AlreadyPaused {
                    pause: pause.clone(),
                    revision: state.queue_revision,
                });
            }
            if state.items.is_empty() {
                return Ok(QueuePauseDecision::Empty {
                    revision: state.queue_revision,
                });
            }
            let pause = UserInputQueuePause {
                paused_at: Utc::now().to_rfc3339(),
                reason: reason.to_string(),
                turn_id,
            };
            state.pause = Some(pause.clone());
            state.queue_revision = next_revision(state.queue_revision)?;
            Ok(QueuePauseDecision::Paused {
                pause,
                revision: state.queue_revision,
            })
        })
    }

    /// Clear the pause. Returns the cleared pause and the new queue revision,
    /// or `None` when the queue was not paused.
    pub fn resume(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<(UserInputQueuePause, u64)>> {
        self.mutate(|store| {
            let Some(state) = store.sessions.get_mut(cutex_session_id) else {
                return Ok(None);
            };
            let Some(pause) = state.pause.take() else {
                return Ok(None);
            };
            state.queue_revision = next_revision(state.queue_revision)?;
            Ok(Some((pause, state.queue_revision)))
        })
    }

    /// Let one queued item through a pause (`UserInputQueueItem::bypass_pause`).
    pub fn mark_bypass_pause(&self, cutex_session_id: &str, queue_id: &str) -> anyhow::Result<bool> {
        self.mutate(|store| {
            let Some(stored) = store.sessions.get_mut(cutex_session_id).and_then(|state| {
                state
                    .items
                    .iter_mut()
                    .find(|stored| stored.item.queue_id == queue_id)
            }) else {
                return Ok(false);
            };
            stored.item.bypass_pause = true;
            Ok(true)
        })
    }

    /// The next item a flush may send: the front item, or while paused the
    /// first item marked `bypass_pause` (if any). `ignore_pause` (an explicit
    /// flush) always takes the front item.
    pub fn next_flushable(
        &self,
        cutex_session_id: &str,
        ignore_pause: bool,
    ) -> anyhow::Result<Option<(String, String, UserInputQueueItem)>> {
        self.read(|store| {
            let Some(state) = store.sessions.get(cutex_session_id) else {
                return Ok(None);
            };
            let paused = state.pause.is_some() && !ignore_pause;
            Ok(state
                .items
                .iter()
                .find(|stored| !paused || stored.item.bypass_pause)
                .map(|stored| {
                    (
                        stored.management_request_id.clone(),
                        stored.thread_id.clone(),
                        stored.item.clone(),
                    )
                }))
        })
    }

    pub fn front(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Option<(String, String, UserInputQueueItem)>> {
        self.read(|store| {
            Ok(store
                .sessions
                .get(cutex_session_id)
                .and_then(|state| state.items.first())
                .map(|stored| {
                    (
                        stored.management_request_id.clone(),
                        stored.thread_id.clone(),
                        stored.item.clone(),
                    )
                }))
        })
    }

    pub fn update(
        &self,
        cutex_session_id: &str,
        queue_id: &str,
        expected_revision: u64,
        input: Vec<Value>,
    ) -> anyhow::Result<Option<UserInputQueueItem>> {
        for item in &input {
            validate_user_input(item).map_err(anyhow::Error::msg)?;
        }
        self.mutate(|store| {
            let Some(state) = store.sessions.get_mut(cutex_session_id) else {
                return Ok(None);
            };
            let Some((position, stored)) = state
                .items
                .iter_mut()
                .enumerate()
                .find(|(_, stored)| stored.item.queue_id == queue_id)
            else {
                return Ok(None);
            };
            if stored.item.revision != expected_revision {
                return Err(QueueRevisionConflict { expected: expected_revision, current: stored.item.revision }.into());
            }
            // Its input is (or may be) in native already: an edit would make
            // the queue item and the native history disagree.
            if state
                .identities
                .get(&stored.item.client_user_message_id)
                .is_some_and(|identity| identity.delivery.is_some())
            {
                return Err(QueueInputAlreadySubmitted.into());
            }
            stored.item.revision = next_revision(stored.item.revision)?;
            stored.item.input = input;
            stored.item.updated_at = Utc::now().to_rfc3339();
            let client_user_message_id = stored.item.client_user_message_id.clone();
            let identity_sha256 = canonical_request_sha256(&serde_json::json!({
                "origin": stored.item.origin,
                "input": stored.item.input,
            }))?;
            let mut item = stored.item.clone();
            item.position = position as u64;
            if let Some(identity) = state.identities.get_mut(&client_user_message_id) {
                identity.canonical_sha256 = identity_sha256;
            }
            state.queue_revision = next_revision(state.queue_revision)?;
            Ok(Some(item))
        })
    }

    pub fn remove(
        &self,
        cutex_session_id: &str,
        queue_id: &str,
        expected_revision: u64,
    ) -> anyhow::Result<Option<UserInputQueueItem>> {
        self.mutate(|store| {
            let Some(state) = store.sessions.get_mut(cutex_session_id) else {
                return Ok(None);
            };
            let Some(index) = state
                .items
                .iter()
                .position(|stored| stored.item.queue_id == queue_id)
            else {
                return Ok(None);
            };
            if state.items[index].item.revision != expected_revision {
                return Err(QueueRevisionConflict { expected: expected_revision, current: state.items[index].item.revision }.into());
            }
            let mut removed = state.items.remove(index).item;
            state.admission.remove_owner(queue_id)?;
            removed.revision = next_revision(removed.revision)?;
            removed.updated_at = Utc::now().to_rfc3339();
            if state.items.is_empty() {
                // Nothing left to hold back.
                state.pause = None;
            }
            state.queue_revision = next_revision(state.queue_revision)?;
            Ok(Some(removed))
        })
    }

    pub fn snapshot(&self, cutex_session_id: &str) -> anyhow::Result<Vec<Value>> {
        let (_, items) = self.list(cutex_session_id)?;
        items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn clear_session(&self, cutex_session_id: &str) -> anyhow::Result<()> {
        self.mutate(|store| {
            store.sessions.remove(cutex_session_id);
            Ok(())
        })
    }

    fn mutate<T>(
        &self,
        action: impl FnOnce(&mut UserInputStateStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_lock(|path| {
            let mut store = (*cached_store(path)?).clone();
            let before = serde_json::to_vec(&store)?;
            let previous: BTreeMap<_, _> = store.sessions.iter().map(|(id, state)| (id.clone(), serde_json::to_vec(state).unwrap_or_default())).collect();
            let result = action(&mut store)?;
            if serde_json::to_vec(&store)? != before {
                write_private_pretty_json_atomic(path, &store, "management v2 user-input state")?;
                {
                    for (session, state) in &store.sessions {
                        if previous.get(session) != Some(&serde_json::to_vec(state)?) { super::admission::changed(session); }
                    }
                    for session in previous.keys().filter(|id| !store.sessions.contains_key(*id)) { super::admission::changed(session); }
                }
            }
            Ok(result)
        })
    }

    fn read<T>(
        &self,
        action: impl FnOnce(&UserInputStateStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_lock(|path| action(cached_store(path)?.as_ref()))
    }

    fn with_lock<T>(&self, action: impl FnOnce(&Path) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 user-input lock was poisoned"))?;
        let lock_file = open_private_lock(&self.root.join(USER_INPUT_LOCK_FILE))?;
        lock_file.lock()?;
        let result = action(&self.root.join(USER_INPUT_STATE_FILE));
        let unlock = lock_file.unlock();
        if result.is_ok() {
            unlock?;
        }
        result
    }
}

fn cached_store(path: &Path) -> anyhow::Result<std::sync::Arc<UserInputStateStore>> {
    use crate::platform::file_stamp::FileStamp;
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (Option<FileStamp>, std::sync::Arc<UserInputStateStore>)>>> = std::sync::OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stamp = FileStamp::of(path);
    if let Some((prior, store)) = cache.get(path) { if prior == &stamp { return Ok(store.clone()); } }
    let store = std::sync::Arc::new(load_store(path)?);
    if cache.len() > 32 { cache.clear(); }
    cache.insert(path.into(), (stamp, store.clone()));
    Ok(store)
}

/// Scratch state for admission calculations. Neither sync_numbered nor
/// sync_admission reads identities; listings still resolve delivery status
/// against the original locked state via positioned_items.
fn admission_projection(state: Option<&SessionInputState>) -> SessionInputState {
    match state {
        Some(state) => SessionInputState {
            queue_revision: state.queue_revision,
            admission: state.admission.clone(),
            items: state.items.clone(),
            identities: BTreeMap::new(),
            pause: state.pause.clone(),
        },
        None => SessionInputState::default(),
    }
}

fn positioned_items(state: &SessionInputState) -> Vec<UserInputQueueItem> {
    state
        .items
        .iter()
        .enumerate()
        .map(|(position, stored)| {
            let mut item = stored.item.clone();
            item.position = position as u64;
            item.delivery = match state
                .identities
                .get(&item.client_user_message_id)
                .and_then(|identity| identity.delivery.as_ref())
            {
                Some(InputDelivery::Sending { .. }) => Some("sending".to_string()),
                Some(InputDelivery::Unknown { .. }) => Some("unknown".to_string()),
                _ => None,
            };
            item
        })
        .collect()
}

fn input_identity_sha256(params: &UserInputSubmitParams) -> anyhow::Result<String> {
    canonical_request_sha256(&serde_json::json!({
        "origin": params.origin,
        "input": params.input,
    }))
}

pub(crate) fn next_revision(current: u64) -> anyhow::Result<u64> {
    if current >= MAX_SAFE_SEQUENCE {
        anyhow::bail!("user-input revision exhausted the JSON-safe integer range");
    }
    Ok(current + 1)
}

fn load_store(path: &Path) -> anyhow::Result<UserInputStateStore> {
    match fs::read(path) {
        Ok(bytes) => {
            let store: UserInputStateStore = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "failed to parse management v2 user-input state: {}",
                    path.display()
                )
            })?;
            if store.version != 1 {
                anyhow::bail!("unsupported management v2 user-input state version");
            }
            Ok(store)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UserInputStateStore::default()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to read management v2 user-input state: {}",
                path.display()
            )
        }),
    }
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

    use super::*;

    fn params(text: &str) -> UserInputSubmitParams {
        parse_user_input_submit_params(&json!({
            "clientUserMessageId": "client-message-1",
            "origin": { "kind": "android", "clientId": "device-1" },
            "strategy": "queue",
            "input": [{ "type": "text", "text": text, "text_elements": [] }]
        }))
        .expect("valid params")
    }

    #[test]
    fn queue_list_noop_preserves_receipts_and_skipped_eligibility_without_rewriting() {
        let root = std::env::temp_dir().join(format!("cutex-v2-list-noop-{}", Uuid::new_v4()));
        let session = format!("list-noop-{}", Uuid::new_v4());
        let repository = UserInputRepository::open(&root).unwrap();
        let input = params("owner");
        repository.enqueue(&session, "thread", "request", &input).unwrap();
        let sending = InputDelivery::Sending { thread_id: "thread".into(), since: "t".into() };
        assert!(repository.set_delivery(&session, &input.client_user_message_id, None, Some(sending.clone())).unwrap());
        repository.ask_store().with_state(|state| {
            let mut new = crate::asks::store::tests::new_ask("returned", "result");
            new.requester = crate::asks::model::Requester::Agent { cutex_session_id: session.clone(), name: None };
            let ask = state.create_ask(new, Utc::now()).unwrap().0;
            crate::asks::delivery::record_answer_delivery(state, &ask, "answer", Utc::now());
        }).unwrap();
        let first = repository.list_admission(&session).unwrap();
        assert_eq!(first.1[0].delivery.as_deref(), Some("sending"));
        assert!(first.3.next_step[0].eligible);
        let path = root.join(USER_INPUT_STATE_FILE);
        let stamp = crate::platform::file_stamp::FileStamp::of(&path);
        let bytes = fs::read(&path).unwrap();
        let stored = load_store(&path).unwrap();
        assert!(!stored.sessions[&session].admission.snapshot.next_step[0].eligible, "eligibility is intentionally not persisted");
        let cache_competitor = crate::asks::store::AskStore::at(root.join("cache-competitor"));
        cache_competitor.with_state(|state| { state.wait_cancel.insert("fixture".into(), 1); }).unwrap();
        for _ in 0..4 {
            // Deterministically reproduce parallel tests using another ask
            // store. Its read evicts the single-slot AskStore cache, so the
            // next list may legitimately publish admission invalidations
            // while reloading asks. Those are not user-input state writes.
            cache_competitor.snapshot().unwrap();
            assert_eq!(repository.list_admission(&session).unwrap(), first);
        }
        assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let reopened = UserInputRepository::open(&root).unwrap();
        assert_eq!(reopened.register_identity(&session, &input).unwrap(), ClientIdentityDecision::Existing);
        assert_eq!(reopened.delivery(&session, &input.client_user_message_id).unwrap(), Some(sending));
        assert!(!reopened.set_delivery(&session, &input.client_user_message_id, None, None).unwrap(), "queue reads preserve the C-010 reservation");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_list_persists_changed_admission_and_preserves_other_sessions() {
        let root = std::env::temp_dir().join(format!("cutex-v2-list-change-{}", Uuid::new_v4()));
        let session = format!("list-change-{}", Uuid::new_v4());
        let repository = UserInputRepository::open(&root).unwrap();
        repository.enqueue(&session, "thread", "request", &params("owner")).unwrap();
        let initial = repository.list_admission(&session).unwrap();
        // A separately opened repository models another writer using the same
        // file lock. Its freshly stored receipt must survive admission sync.
        let other = UserInputRepository::open(&root).unwrap();
        other.register_identity("unrelated-session", &params("other")).unwrap();
        let path = root.join(USER_INPUT_STATE_FILE);
        let other_before = serde_json::to_vec(&load_store(&path).unwrap().sessions["unrelated-session"]).unwrap();
        repository.ask_store().with_state(|state| {
            crate::asks::control::owner_stop(state, &session, Utc::now());
        }).unwrap();
        let before = load_store(&path).unwrap().sessions[&session].admission.snapshot.clone();
        assert!(before.ridealongs_held.is_none(), "ask sync persists numbered order, not full presentation");
        let _ = super::super::admission::take_change(&session);
        let changed = repository.list_admission(&session).unwrap();
        assert_eq!(changed.0, initial.0);
        assert_eq!(changed.3.order_revision, initial.3.order_revision);
        assert!(changed.3.revision > before.revision);
        assert_eq!(changed.3.ridealongs_held.as_deref(), Some("ownerStop"));
        assert!(super::super::admission::take_change(&session));
        let persisted = load_store(&path).unwrap();
        assert_eq!(persisted.sessions[&session].admission.snapshot, changed.3);
        assert_eq!(serde_json::to_vec(&persisted.sessions["unrelated-session"]).unwrap(), other_before);
        let stamp = crate::platform::file_stamp::FileStamp::of(&path);
        assert_eq!(other.list_admission(&session).unwrap(), changed);
        // A cache reload may publish an advisory admission invalidation;
        // persistent state/revision must nevertheless remain unchanged.
        assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "synthetic queue/list performance comparison; no live state"]
    fn benchmark_queue_list_unchanged_large_identity_history() {
        let root = std::env::temp_dir().join(format!("cutex-v2-list-bench-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        repository.mutate(|store| {
            for session in 0..100 {
                let state = store.sessions.entry(format!("session-{session}")).or_default();
                for id in 0..100 {
                    state.identities.insert(format!("client-{id}"), ClientInputIdentity {
                        canonical_sha256: "a".repeat(64), first_seen_at: "2026-10-13T00:00:00Z".into(), delivery: None,
                    });
                }
            }
            Ok(())
        }).unwrap();
        repository.list_admission("session-0").unwrap();
        let bytes = fs::read(root.join(USER_INPUT_STATE_FILE)).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..20 {
            // Exact former implementation, retained only in this opt-in test.
            repository.ask_store().inspect(|asks| repository.mutate(|store| {
                let state = store.sessions.entry("session-0".into()).or_default();
                let admission = repository.sync_admission("session-0", state, asks)?;
                Ok((state.queue_revision, positioned_items(state), state.pause.clone(), admission))
            })).unwrap().unwrap();
        }
        let baseline = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..20 { repository.list_admission("session-0").unwrap(); }
        let optimized = start.elapsed();
        assert_eq!(fs::read(root.join(USER_INPUT_STATE_FILE)).unwrap(), bytes);
        eprintln!("queue_list_noop bytes={} identities=10000 iterations=20 baseline_ms={:.3} optimized_ms={:.3}",
            bytes.len(), baseline.as_secs_f64() * 1000.0, optimized.as_secs_f64() * 1000.0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_reads_leave_store_bytes_mtime_and_revision_unchanged() {
        let root = std::env::temp_dir().join(format!("cutex-v2-admission-read-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let session = "admission-read";
        repository.enqueue(session, "thread", "request", &params("owner")).unwrap();
        let asks = repository.ask_store();
        asks.with_state(|state| {
            let mut ask = crate::asks::store::tests::new_ask("ask", "pending ask");
            ask.target_cutex_session_id = session.into();
            state.create_ask(ask, Utc::now()).unwrap();
        }).unwrap();
        let initial = repository.list_admission(session).unwrap().3;
        assert_eq!(initial.items.len(), 2);
        let path = root.join(USER_INPUT_STATE_FILE);
        // Pin an old mtime so even a fast redundant atomic rewrite is visible.
        let modified = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
        let bytes = fs::read(&path).unwrap();
        let snapshot = asks.snapshot().unwrap();
        for _ in 0..4 {
            assert_eq!(repository.admission_with_state(session, &snapshot).unwrap(), initial);
            assert_eq!(repository.list_admission(session).unwrap().3, initial);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn numbered_admission_preserves_order_and_continuation_without_ridealongs() {
        use crate::asks::model::{ExecutionState, Requester, ResumeReason, ResumeRecord};
        let root = std::env::temp_dir().join(format!("cutex-v2-numbered-admission-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let session = "numbered-admission";
        repository.enqueue(session, "thread", "request", &params("owner")).unwrap();
        let asks = repository.ask_store();
        let resumed = asks.with_state(|state| {
            let now = Utc::now();
            let mut ordinary = crate::asks::store::tests::new_ask("ordinary", "ordinary ask");
            ordinary.target_cutex_session_id = session.into();
            state.create_ask(ordinary, now).unwrap();
            let mut continuation = crate::asks::store::tests::new_ask("continuation", "continued ask");
            continuation.target_cutex_session_id = session.into();
            let id = state.create_ask(continuation, now).unwrap().0.ask_id;
            let ask = state.asks.get_mut(&id).unwrap();
            ask.execution = ExecutionState::Running;
            ask.resume = Some(ResumeRecord {
                reason: ResumeReason::Resumed, since: now.to_rfc3339(), from_attempt: 0,
                jobs: Vec::new(), by: None, note: None,
            });
            let mut returned = crate::asks::store::tests::new_ask("returned", "returned result");
            returned.requester = Requester::Agent { cutex_session_id: session.into(), name: None };
            let returned = state.create_ask(returned, now).unwrap().0;
            crate::asks::delivery::record_answer_delivery(state, &returned, "answer", now).unwrap();
            id
        }).unwrap();
        let full = repository.list_admission(session).unwrap().3;
        assert_eq!(full.items[0].id, resumed);
        assert_eq!(full.items.len(), 3);
        assert_eq!(full.next_step.len(), 1);
        let path = root.join(USER_INPUT_STATE_FILE);
        let bytes = fs::read(&path).unwrap();
        let snapshot = asks.snapshot().unwrap();
        let numbered = repository.numbered_admission_with_state(session, &snapshot).unwrap();
        assert_eq!(numbered.items, full.items);
        assert_eq!(numbered.revision, full.revision);
        assert!(numbered.next_step.is_empty());
        assert!(numbered.current_turn.is_none());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leased_requester_results_follow_the_busy_wait() {
        let root = std::env::temp_dir().join(format!("cutex-v2-leased-results-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let session = format!("leased-results-{}", Uuid::new_v4());
        let created = Utc::now() - chrono::Duration::hours(1);
        repository.ask_store().with_state(|state| {
            let mut ask = crate::asks::store::tests::new_ask("leased", "leased result");
            ask.requester = crate::asks::model::Requester::Agent { cutex_session_id: session.clone(), name: None };
            let ask = state.create_ask(ask, created).unwrap().0;
            crate::asks::delivery::record_answer_delivery(state, &ask, "answer", created).unwrap();
        }).unwrap();
        let free = repository.list_admission(&session).unwrap().3;
        assert_eq!(free.next_step.len(), 1);
        let expected = super::super::admission::timing(&free.next_step[0].enqueued_at, false);
        assert_eq!((free.next_step[0].timing.clone(), free.next_step[0].next_step_at.clone()), expected);
        // A delegation event started the running turn: asks::returns sends
        // the result beside that lease at the next step after the same busy
        // wait (an owner requester agent, 2026-10-12), so the snapshot shows the same rows.
        repository.ask_store().with_state(|state| {
            state.leases.insert(session.clone(), crate::asks::model::AdmissionLease {
                cutex_session_id: session.clone(),
                holder: crate::asks::model::LeaseHolder::Event { event_id: "aev-test".into() },
                native_message_id: "cutex-ask-event-test".into(),
                runtime_generation: 1,
                turn_id: Some("turn-1".into()),
                since: Utc::now().to_rfc3339(),
            });
        }).unwrap();
        let leased = repository.list_admission(&session).unwrap().3;
        assert_eq!((leased.next_step[0].timing.clone(), leased.next_step[0].next_step_at.clone()), expected);
        // Sent at the next step: `nextStep` since it was sent.
        let sent = repository.ask_store().with_state(|state| {
            let id = crate::asks::delivery::coalesce_into_event(state, &session, &format!("{}-test", crate::asks::returns::NEXT_STEP_EVENT_PREFIX), Utc::now()).unwrap();
            state.events[&id].created_at.clone()
        }).unwrap();
        let sent_rows = repository.list_admission(&session).unwrap().3;
        assert_eq!((sent_rows.next_step[0].timing.as_str(), sent_rows.next_step[0].next_step_at.as_deref()), ("nextStep", Some(sent.as_str())));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_cas_ignores_ridealong_and_turn_snapshot_changes() {
        let root = std::env::temp_dir().join(format!("cutex-v2-order-cas-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let session = format!("order-cas-{}", Uuid::new_v4());
        let QueueEnqueueDecision::Queued(owner) = repository.enqueue(&session, "thread", "owner", &params("owner")).unwrap() else {
            panic!("queued owner");
        };
        let initial = repository.list_admission(&session).unwrap().3;
        repository.ask_store().with_state(|state| {
            let now = Utc::now();
            let mut returned = crate::asks::store::tests::new_ask("returned", "returned result");
            returned.requester = crate::asks::model::Requester::Agent { cutex_session_id: session.clone(), name: None };
            let returned = state.create_ask(returned, now).unwrap().0;
            crate::asks::delivery::record_answer_delivery(state, &returned, "answer", now).unwrap();
        }).unwrap();
        let returned = repository.list_admission(&session).unwrap().3;
        assert_eq!(returned.next_step.len(), 1);
        // The ride-along names its delegation (title), not the generic kind.
        assert_eq!(returned.next_step[0].label.as_deref(), Some("returned result"));
        assert!(returned.revision > initial.revision);
        assert_eq!(returned.order_revision, initial.order_revision);
        repository.mutate(|store| {
            let snapshot = &mut store.sessions.get_mut(&session).unwrap().admission.snapshot;
            snapshot.next_step[0].timing = if returned.next_step[0].timing == "afterTurn" { "nextStep" } else { "afterTurn" }.into();
            snapshot.current_turn = Some(super::super::admission::AdmissionCurrentTurn { kind: "owner".into(), ask_id: None });
            snapshot.revision = next_revision(snapshot.revision)?;
            Ok(())
        }).unwrap();
        let (moved, _, _, changed) = repository.move_admission_item(&session, &owner.queue_id,
            initial.order_revision, None, true).unwrap();
        assert!(!changed);
        assert!(moved.revision > returned.revision);
        assert_eq!(moved.order_revision, initial.order_revision);
        assert!(moved.current_turn.is_none());
        assert_eq!(moved.next_step[0].timing, returned.next_step[0].timing);
        let mut second = params("second");
        second.client_user_message_id = "second-owner".into();
        repository.enqueue(&session, "thread", "second-owner", &second).unwrap();
        let error = repository.move_admission_item(&session, &owner.queue_id,
            initial.order_revision, None, true).unwrap_err();
        assert!(error.downcast_ref::<QueueRevisionConflict>().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_poll_interest_tracks_owner_queue_and_online_presence() {
        let root = std::env::temp_dir().join(format!("cutex-admission-interest-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let session = format!("interest-{}", Uuid::new_v4());
        assert!(!repository.needs_admission_poll().unwrap());
        repository.enqueue(&session, "thread", "owner", &params("owner")).unwrap();
        let owner = repository.list(&session).unwrap().1.remove(0);
        assert!(repository.needs_admission_poll().unwrap());
        repository.remove(&session, &owner.queue_id, owner.revision).unwrap();
        assert!(!repository.needs_admission_poll().unwrap());
        let asks = repository.ask_store();
        asks.with_state(|state| { state.presence.insert(session.clone(), crate::asks::store::Presence { online: true, ..Default::default() }); }).unwrap();
        assert!(repository.needs_admission_poll().unwrap());
        asks.with_state(|state| { state.presence.get_mut(&session).unwrap().online = false; }).unwrap();
        assert!(!repository.needs_admission_poll().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_terminal_keys_are_removed_without_touching_other_sessions() {
        const CHILD: &str = "CUTEX_ADMISSION_TERMINAL_KEYS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let parent = PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home"));
            let home = parent.join(format!("admission-terminal-keys-{}", Uuid::new_v4()));
            fs::create_dir_all(&home).unwrap();
            fs::write(home.join(".cutex-test-private-home"), b"admission terminal keys test\n").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::user_input::tests::admission_terminal_keys_are_removed_without_touching_other_sessions", "--nocapture"])
                .env("HOME", &home).env("CUTEX_TEST_PRIVATE_HOME", &home).env(CHILD, "1")
                .status().unwrap();
            fs::remove_dir_all(home).unwrap();
            assert!(status.success(), "isolated admission terminal keys test failed");
            return;
        }

        use crate::asks::model::{CloseOutcome, ExecutionState, Hold, PausedBy};
        let root = std::env::temp_dir().join(format!("cutex-v2-admission-cleanup-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).unwrap();
        let active_id = format!("admission-changing-{}", Uuid::new_v4());
        let untouched_id = format!("admission-unchanged-{}", Uuid::new_v4());
        let active = active_id.as_str();
        let untouched = untouched_id.as_str();
        let QueueEnqueueDecision::Queued(owner) = repository.enqueue(active, "thread", "owner", &params("owner")).unwrap() else {
            panic!("queued owner");
        };
        repository.enqueue(untouched, "other-thread", "other-owner", &params("other owner")).unwrap();
        let asks = repository.ask_store();
        let ask_id = asks.with_state(|state| {
            let mut ask = crate::asks::store::tests::new_ask("changing", "changing ask");
            ask.target_cutex_session_id = active.into();
            let id = state.create_ask(ask, Utc::now()).unwrap().0.ask_id;
            let mut other = crate::asks::store::tests::new_ask("untouched", "untouched ask");
            other.target_cutex_session_id = untouched.into();
            state.create_ask(other, Utc::now()).unwrap();
            id
        }).unwrap();
        let before = repository.list_admission(active).unwrap().3;
        let stable = repository.list_admission(untouched).unwrap().3;
        let path = root.join(USER_INPUT_STATE_FILE);
        let read_store = || serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap();
        let untouched_before = read_store()["sessions"][untouched].clone();
        assert_eq!(read_store()["sessions"][active]["admission"]["keys"].as_object().unwrap().len(), 2);
        crate::management::v2::admission::take_change(untouched);
        asks.with_state(|state| {
            state.update_ask(&ask_id, Utc::now(), |ask| ask.hold = Some(Hold::Paused { by: PausedBy::Owner }));
        }).unwrap();
        assert!(repository.admission(active).unwrap().revision > before.revision);
        assert_eq!(read_store()["sessions"][untouched], untouched_before);
        assert_eq!(repository.admission(untouched).unwrap(), stable);
        assert!(!crate::management::v2::admission::take_change(untouched));
        asks.with_state(|state| {
            state.update_ask(&ask_id, Utc::now(), |ask| {
                ask.execution = ExecutionState::Cancelled;
                ask.closed = Some(CloseOutcome::Cancelled);
            });
        }).unwrap();
        assert_eq!(read_store()["sessions"][active]["admission"]["keys"].as_object().unwrap().len(), 1);
        repository.remove(active, &owner.queue_id, owner.revision).unwrap();
        assert_eq!(read_store()["sessions"][active]["admission"]["keys"].as_object().unwrap().len(), 0);
        assert_eq!(read_store()["sessions"][untouched], untouched_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parser_preserves_native_inputs_and_rejects_unknown_types() {
        let parsed = params("hello");
        assert_eq!(parsed.origin.kind, UserInputOriginKind::Android);
        assert_eq!(parsed.input[0]["text_elements"], json!([]));
        assert!(parse_user_input_submit_params(&json!({
            "clientUserMessageId": "message-1",
            "origin": { "kind": "android", "clientId": "device-1" },
            "strategy": "auto",
            "input": [{ "type": "future" }]
        }))
        .is_err());
    }

    #[test]
    fn queue_revisions_and_client_identity_are_durable() {
        let root = std::env::temp_dir().join(format!("cutex-v2-user-input-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).expect("open repository");
        let first = repository
            .enqueue("session-1", "thread-1", "request-1", &params("hello"))
            .expect("enqueue");
        let QueueEnqueueDecision::Queued(first) = first else {
            panic!("expected queued item");
        };
        assert_eq!(first.revision, 1);
        assert_eq!(repository.list("session-1").expect("list").0, 1);
        assert!(matches!(
            repository
                .enqueue("session-1", "thread-1", "request-2", &params("hello"))
                .expect("dedupe"),
            QueueEnqueueDecision::Deduplicated(_)
        ));
        assert_eq!(
            repository
                .enqueue("session-1", "thread-1", "request-3", &params("changed"))
                .expect("conflict"),
            QueueEnqueueDecision::ClientMessageConflict
        );
        let updated = repository
            .update(
                "session-1",
                &first.queue_id,
                1,
                vec![json!({ "type": "text", "text": "updated", "text_elements": [] })],
            )
            .expect("update")
            .expect("queue item");
        assert_eq!(updated.revision, 2);
        let removed = repository
            .remove("session-1", &first.queue_id, 2)
            .expect("remove")
            .expect("removed item");
        assert_eq!(removed.revision, 3);
        assert_eq!(repository.list("session-1").expect("empty list").0, 3);
        fs::remove_dir_all(root).expect("remove repository");
    }

    fn params_with_id(id: &str, text: &str) -> UserInputSubmitParams {
        let mut parsed = params(text);
        parsed.client_user_message_id = id.to_string();
        parsed
    }

    #[test]
    fn queue_pause_is_durable_holds_auto_flush_and_clears_when_emptied() {
        let root = std::env::temp_dir().join(format!("cutex-v2-queue-pause-{}", Uuid::new_v4()));
        let repository = UserInputRepository::open(&root).expect("open repository");
        // An empty queue has nothing to hold back.
        assert_eq!(
            repository.pause("session-1", "interrupted", None).expect("pause empty"),
            QueuePauseDecision::Empty { revision: 0 }
        );
        let QueueEnqueueDecision::Queued(first) = repository
            .enqueue("session-1", "thread-1", "request-1", &params_with_id("m-1", "one"))
            .expect("enqueue first")
        else {
            panic!("expected queued item");
        };
        let QueueEnqueueDecision::Queued(second) = repository
            .enqueue("session-1", "thread-1", "request-2", &params_with_id("m-2", "two"))
            .expect("enqueue second")
        else {
            panic!("expected queued item");
        };
        let QueuePauseDecision::Paused { pause, revision } = repository
            .pause("session-1", "interrupted", Some("turn-1".into()))
            .expect("pause")
        else {
            panic!("expected a new pause");
        };
        assert_eq!(pause.reason, "interrupted");
        assert_eq!(pause.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(revision, 3);
        assert!(matches!(
            repository.pause("session-1", "manual", None).expect("pause again"),
            QueuePauseDecision::AlreadyPaused { revision: 3, .. }
        ));

        // Durable: a fresh repository on the same root (daemon restart) still sees it.
        let reopened = UserInputRepository::open(&root).expect("reopen repository");
        assert_eq!(reopened.pause_state("session-1").expect("pause state"), Some(pause.clone()));
        // Auto flush is held; an explicit flush still takes the front item.
        assert!(reopened.next_flushable("session-1", false).expect("auto").is_none());
        assert_eq!(
            reopened.next_flushable("session-1", true).expect("explicit").map(|(_, _, item)| item.queue_id),
            Some(first.queue_id.clone())
        );
        // An offline submit marked bypass_pause goes through the pause.
        assert!(reopened.mark_bypass_pause("session-1", &second.queue_id).expect("mark"));
        let (_, _, next) = reopened
            .next_flushable("session-1", false)
            .expect("auto with bypass")
            .expect("bypass item");
        assert_eq!(next.queue_id, second.queue_id);
        assert!(next.bypass_pause);
        assert_eq!(serde_json::to_value(&next).expect("item json")["bypassPause"], json!(true));
        assert!(serde_json::to_value(&first).expect("item json").get("bypassPause").is_none());

        // Resume clears it once.
        let (cleared, revision) = reopened.resume("session-1").expect("resume").expect("was paused");
        assert_eq!(cleared, pause);
        assert_eq!(revision, 4);
        assert!(reopened.resume("session-1").expect("resume again").is_none());
        assert_eq!(
            reopened.next_flushable("session-1", false).expect("auto after resume").map(|(_, _, item)| item.queue_id),
            Some(first.queue_id.clone())
        );

        // Emptying a paused queue drops the pause.
        assert!(matches!(
            reopened.pause("session-1", "manual", None).expect("pause"),
            QueuePauseDecision::Paused { .. }
        ));
        reopened.remove("session-1", &first.queue_id, 1).expect("remove first");
        assert!(reopened.pause_state("session-1").expect("still paused").is_some());
        reopened.remove("session-1", &second.queue_id, 1).expect("remove second");
        assert!(reopened.pause_state("session-1").expect("emptied").is_none());
        fs::remove_dir_all(root).expect("remove repository");
    }
}
