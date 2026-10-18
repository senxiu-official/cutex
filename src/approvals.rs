//! Owner approvals for agent actions (owner decision 2026-10-07).
//!
//! Agents act without human approval inside their project and inside their
//! own permission envelope. An action needs the owner's approval when it
//! exceeds the caller's envelope (today: a managed service, which cutex runs
//! unsandboxed, requested by an agent that could not run that command
//! unsandboxed without asking) or when it uses a profile the owner marked as
//! needing approval.
//!
//! The gate is server-side, so it holds whatever the agent's own approval
//! policy is (`never` agents cannot be prompted by cute-codex at all) and
//! whether or not a call went through the MCP adapter. Flow:
//!
//! 1. The bus calls [`ApprovalStore::gate`] with a digest of the exact
//!    request. The first call records a pending approval and the tool answers
//!    `approval_required` with its `approval_id`.
//! 2. The owner approves or denies it in a client (`GET /v2/approvals`,
//!    `POST /v2/approvals/{id}/approve|deny`, host event
//!    `cutex/approvals/changed`).
//! 3. The agent retries the same call; an approved record is consumed once
//!    and the action proceeds. Pending and approved records expire after 24 h;
//!    a denial is answered for 24 h, after which a retry asks again.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Pending and approved-but-unused approvals lapse after this long.
const APPROVAL_TTL_HOURS: i64 = 24;
/// Decided records older than this are pruned.
const RETENTION_DAYS: i64 = 7;
const MAX_RECORDS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// `service_register|update` with a managed (cutex-run) command.
    ManagedService,
    /// Keep a service beyond its ephemeral lifetime (optionally a managed launch).
    ServiceLifetime,
    /// An agent launch (create / replace) with a profile
    /// the owner marked as needing approval.
    AgentProfile,
    /// An agent's ask to a target whose capabilities are not a subset of
    /// the requester's (asks §4.5, row A). Bound to the ask snapshot digest.
    AskDelivery,
    /// Owner explicitly selects an owner-pool browser tab; generic approval
    /// alone never selects a tab or issues a browser lease.
    BrowserClaim,
    /// Optional mechanical approval of one exact browser-open intent.
    BrowserOpen,
    /// An agent-started workflow run plan (cutex `workflows`): name, phases,
    /// step profiles, limits and the script digest. Approving starts the run;
    /// owner only, never the project reviewer (it multiplies spend).
    WorkflowRun,    /// A static service root outside the agent's cwd / project workspace
    /// (bound to the canonical root). Combined into `ServiceLifetime` when the
    /// same request also asks for a persistent lifetime.
    StaticRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    /// Approved and used by the retried action.
    Consumed,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRecord {
    pub approval_id: String,
    pub kind: ApprovalKind,
    /// Why approval is needed (`outside_caller_envelope`,
    /// `profile_requires_approval`).
    pub reason: String,
    pub requester_cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// One line for the owner, e.g. `Run managed service web: npm run dev`.
    pub summary: String,
    /// Structured request facts for the approval card (no secret values).
    #[serde(default)]
    pub details: Value,
    pub request_sha256: String,
    pub state: ApprovalState,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Who decided (`approval-decided-by`, 2026-10-18); absent on records
    /// decided before it and on undecided ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<ApprovalDecider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    /// The requesting tool call is holding for the owner's decision
    /// (approval-wait). Kept after the hold ended (`end`), so clients can
    /// tell a call that already gave up from one still waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<ApprovalWait>,
}

/// Who made an approval decision (`approval-decided-by`), so an unexpected
/// decision can be traced to a client (r243 acceptance: a denial nobody
/// remembered making).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecider {
    /// `owner` (a client holding the Management root credential); a decision
    /// cutex makes itself would be `auto:<reason>` (none today).
    pub by: String,
    /// `loopback` (this host, SSH tunnels included) or `remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// The remote peer address (`remote` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// What the client said about itself (`app`, `version`, `device`,
    /// `surface`, `input`); unverified, for diagnosis only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub client: BTreeMap<String, String>,
}

/// One tool call holding for the owner's decision (approval-wait,
/// [`ApprovalStore::begin_wait`] / [`ApprovalStore::finish_wait`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalWait {
    /// Distinguishes holds of the same record (a later identical call takes
    /// the slot over; the earlier one then ends without touching it).
    pub id: String,
    pub started_at: String,
    /// When the call gives up and answers `approval_required`.
    pub until: String,
    /// The caller's native thread and turn (when known): Management ends the
    /// hold when that turn ends, so an approval never runs into a turn the
    /// agent no longer sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Why the hold ended: `approved` (the call proceeded), `denied` (the
    /// call answered denied), `timeout`, `disconnected` (the caller went
    /// away), `turn_ended` (Management saw the caller's turn end),
    /// `runtime_changed` (the caller's runtime was replaced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

impl ApprovalWait {
    /// Still holding at `now`.
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.end.is_none() && timestamp(&self.until).is_some_and(|until| until > now)
    }
}

/// How long an agent's tool call holds for the owner's decision before it
/// answers `approval_required` (owner decision 2026-10-17, approval-wait).
pub const APPROVAL_WAIT_SECS: u64 = 120;
/// Façade → Agent Bus: the call may hold up to this many seconds.
pub const APPROVAL_WAIT_HEADER: &str = "x-cutex-approval-wait";
/// Façade → Agent Bus: the caller's native turn (when known).
pub const TURN_HEADER: &str = "x-cutex-mcp-turn-id";
/// cutex → façade: opt in to holding (set by cutex for Codex façades and for
/// Claude adapters declaring `features.approvalWait`; absent = answer at once).
pub const APPROVAL_WAIT_ENV: &str = "CUTEX_MCP_APPROVAL_WAIT_SECS";
/// Claude adapter → façade (one process per call): the calling turn.
pub const TURN_ENV: &str = "CUTEX_MCP_TURN_ID";

/// The hold a façade asks for in seconds: `1..=APPROVAL_WAIT_SECS`, else none.
pub fn parse_wait_secs(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(|secs| secs.min(APPROVAL_WAIT_SECS))
}

/// A tool call that may hold for the owner's decision (Agent Bus side).
pub struct Waiter {
    pub duration: std::time::Duration,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    /// Cheap, checked every poll: the caller still waits for the answer
    /// (its connection is open).
    pub connected: Box<dyn Fn() -> bool + Send + Sync>,
    /// Checked once, right before an approval is used: the caller's runtime
    /// is still the one that asked (same generation and thread).
    pub current: Box<dyn Fn() -> bool + Send + Sync>,
    pub poll: std::time::Duration,
}

impl Waiter {
    /// A waiter for the exact caller; `connected` / `current` as above.
    pub fn new(
        secs: u64,
        thread_id: Option<String>,
        turn_id: Option<String>,
        connected: impl Fn() -> bool + Send + Sync + 'static,
        current: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            duration: std::time::Duration::from_secs(secs.min(APPROVAL_WAIT_SECS)),
            thread_id,
            turn_id,
            connected: Box::new(connected),
            current: Box::new(current),
            poll: std::time::Duration::from_millis(400),
        }
    }
}

/// How a hold ended ([`ApprovalStore::finish_wait`]).
#[derive(Clone, Debug, PartialEq)]
pub enum WaitFinish {
    /// Approved while holding and used by this call.
    Approved(ApprovalRecord),
    /// Denied while holding; this call answers denied.
    Denied(ApprovalRecord),
    /// Still pending (timeout, caller gone, turn ended).
    Pending(ApprovalRecord),
    /// Decided while holding, but this call could not deliver it (the caller
    /// went away or its turn ended): the requester needs the decision notice.
    Undelivered(ApprovalRecord),
    /// The record is gone or no longer live (expired, consumed elsewhere).
    Gone,
}

/// A new approval request.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub kind: ApprovalKind,
    pub reason: String,
    pub requester_cutex_session_id: String,
    pub requester_name: Option<String>,
    pub project_id: Option<String>,
    pub summary: String,
    pub details: Value,
    pub request_sha256: String,
}

/// What the gate decided for one call.
#[derive(Clone, Debug, PartialEq)]
pub enum GateOutcome {
    /// An approval for exactly this request was granted; it is now consumed.
    Approved(ApprovalRecord),
    /// Waiting for the owner (`created` is true for the first request).
    Pending {
        record: ApprovalRecord,
        created: bool,
    },
    /// The owner denied this request while this call waited for the decision.
    Denied(ApprovalRecord),
    /// An earlier identical request (same digest) was already denied and that
    /// denial is still live: answered at once, so a retry loop stops, with a
    /// detail that says it is the earlier decision.
    DeniedEarlier(ApprovalRecord),
}

impl GateOutcome {
    /// The model-facing tool result when the action may not proceed yet.
    pub fn refusal(&self) -> Option<Value> {
        match self {
            Self::Approved(_) => None,
            Self::Pending { record, .. } => Some(json!({
                "ok": false,
                "status": "approval_required",
                "code": "approval_required",
                "approval_id": record.approval_id,
                "detail": format!(
                    "Nothing was done yet: {}this needs the owner's approval ({}). The owner sees the request in the cutex desktop. {}",
                    if record.wait.as_ref().is_some_and(|wait| wait.end.as_deref() == Some("timeout")) {
                        "the owner did not decide while this call waited; "
                    } else {
                        ""
                    },
                    record.summary,
                    if decision_needs_retry(record.kind) {
                        "When the owner decides you get a message from cutex; after an approval retry the same call (until then it answers approval_required). Do not poll in a loop: end your turn or continue with other work."
                    } else {
                        "Retry the same call after they approve; until then it answers approval_required. Do not poll in a loop: end your turn or set an alarm."
                    }
                ),
            })),
            Self::Denied(record) => Some(json!({
                "ok": false,
                "status": "no_write",
                "code": "approval_denied",
                "approval_id": record.approval_id,
                "detail": match record.note.as_deref() {
                    Some(note) => format!("The owner denied this request: {note}"),
                    None => "The owner denied this request.".to_string(),
                },
            })),
            Self::DeniedEarlier(record) => {
                let decided_at = record.decided_at.as_deref().unwrap_or(&record.created_at);
                let until = denial_expiry(record);
                let mut refusal = json!({
                    "ok": false,
                    "status": "no_write",
                    "code": "approval_denied",
                    "approval_id": record.approval_id,
                    "detail": format!(
                        "Nothing was done: the owner already denied this exact request at {decided_at}{}. This is that earlier decision, not a new one; the same request stays denied until {}. Change the request (for example another slug, command or target) or ask the owner; do not retry it unchanged.",
                        record.note.as_deref().map(|note| format!(" ({note})")).unwrap_or_default(),
                        until.as_deref().unwrap_or("the denial expires"),
                    ),
                    "denied_at": decided_at,
                    "reused_denial": true,
                });
                if let Some(until) = until {
                    refusal["denied_until"] = Value::String(until);
                }
                Some(refusal)
            }
        }
    }
}

impl GateOutcome {
    /// For a call that may proceed: the note its successful result carries
    /// when the owner approved this request while this same call held for
    /// the decision (approval-wait), see [`ApprovalRecord::approved_after_wait`].
    pub fn approved_after_wait(&self) -> Option<Value> {
        match self {
            Self::Approved(record) => record.approved_after_wait(),
            _ => None,
        }
    }
}

impl ApprovalRecord {
    /// `{decision: "approved", waitedSecs, approvalId, detail}` when this
    /// record was approved while a tool call held for the owner's decision
    /// and that call used it (its hold ended `approved`), so the agent's
    /// successful result says the owner reviewed it and it must not be
    /// retried. `None` for an approval a later retry used, and for anything
    /// not approved.
    pub fn approved_after_wait(&self) -> Option<Value> {
        let wait = self
            .wait
            .as_ref()
            .filter(|wait| wait.end.as_deref() == Some("approved"))?;
        let waited_secs = match (
            timestamp(&wait.started_at),
            wait.ended_at.as_deref().and_then(timestamp),
        ) {
            (Some(started), Some(ended)) => {
                ((ended - started).num_milliseconds().max(0) + 500) / 1000
            }
            _ => 0,
        };
        Some(json!({
            "decision": "approved",
            "waitedSecs": waited_secs,
            "approvalId": self.approval_id,
            "detail": approved_after_wait_detail(waited_secs),
        }))
    }
}

/// The sentence a successful result carries after an approval-wait.
pub fn approved_after_wait_detail(waited_secs: i64) -> String {
    format!("The owner approved this request after {waited_secs} s; it has been carried out.")
}

/// Put an approved-after-wait note ([`ApprovalRecord::approved_after_wait`])
/// on a successful tool result: `approval` is the note, and its sentence
/// leads the result's `detail` (or becomes it).
pub fn attach_approval_note(result: &mut Value, note: Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let sentence = note["detail"].as_str().unwrap_or_default().to_string();
    let detail = match object.get("detail").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{sentence} {existing}"),
        _ => sentence,
    };
    object.insert("approval".into(), note);
    object.insert("detail".into(), Value::String(detail));
}

/// `X-Cutex-Facade` part a façade sends when its management result parser
/// accepts the approved-after-wait note (`approval`, `detail`) on a
/// complete receipt; older façades refuse unknown top-level fields.
pub const FACADE_APPROVAL_NOTE: &str = "approval-note=1";

/// Whether an `X-Cutex-Facade` header value accepts the note.
pub fn facade_takes_approval_note(header: Option<&str>) -> bool {
    header
        .into_iter()
        .flat_map(|value| value.split(','))
        .filter_map(|part| part.trim().strip_prefix("approval-note="))
        .any(|level| level.trim().parse::<u32>().is_ok_and(|level| level >= 1))
}

/// Why a decision was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum DecideError {
    NotFound,
    NotPending(ApprovalState),
    Store(String),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("approval not found"),
            Self::NotPending(state) => {
                write!(formatter, "approval is no longer pending ({state:?})")
            }
            Self::Store(error) => write!(formatter, "approval store unavailable: {error}"),
        }
    }
}

/// SHA-256 of a JSON value with object keys sorted at every level.
pub fn request_digest(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), canonical(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    format!(
        "{:x}",
        Sha256::digest(canonical(value).to_string().as_bytes())
    )
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    approvals: Vec<ApprovalRecord>,
}

pub struct ApprovalStore {
    path: PathBuf,
}

pub(crate) fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// The default store file (`runtime/approvals/approvals.json`). Approvals are
/// requested by the Agent Bus and decided by Management, so Management
/// watches this file for `cutex/approvals/changed` (management::v2::approvals).
pub fn store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("approvals/approvals.json"))
}

/// Every record in the store file at `path` without locking or expiring
/// (writes are atomic renames, so a read sees one whole version). A missing
/// file has none.
pub fn read_all_at(path: &Path) -> anyhow::Result<Vec<ApprovalRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<StoreFile>(&bytes)
            .context("approval store is corrupt")?
            .approvals),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Whether the requesting agent has to retry its own call after the owner's
/// decision (the gate answered `approval_required` and nothing else acts on
/// the decision). Ask deliveries are re-checked by the asks scheduler,
/// workflow runs start from the engine, browser claims are consumed by the
/// owner's tab selection.
pub fn decision_needs_retry(kind: ApprovalKind) -> bool {
    matches!(
        kind,
        ApprovalKind::ManagedService
            | ApprovalKind::ServiceLifetime
            | ApprovalKind::StaticRoot
            | ApprovalKind::AgentProfile
            | ApprovalKind::BrowserOpen
    )
}

impl ApprovalStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(store_path()?))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn with_store<T>(&self, mutate: impl FnOnce(&mut StoreFile) -> T) -> anyhow::Result<T> {
        let parent = self
            .path
            .parent()
            .context("approval store has no directory")?;
        std::fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(&self.path))?;
        lock.lock_exclusive().context("approval store lock")?;
        let mut store: StoreFile = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("approval store is corrupt")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(error) => return Err(error.into()),
        };
        let before = serde_json::to_vec(&store.approvals)?;
        let result = mutate(&mut store);
        if serde_json::to_vec(&store.approvals)? != before {
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.path,
                &store,
                "owner approvals",
            )?;
        }
        Ok(result)
    }

    /// Decide whether the exact request may proceed now.
    pub fn gate(
        &self,
        request: ApprovalRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<GateOutcome> {
        self.gate_inner(request, now, true)
    }
    /// Browser enqueue consumes only after successful queue admission.
    pub fn gate_browser_open(
        &self,
        request: ApprovalRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<GateOutcome> {
        anyhow::ensure!(
            request.kind == ApprovalKind::BrowserOpen,
            "browser open required"
        );
        self.gate_inner(request, now, false)
    }
    fn gate_inner(
        &self,
        request: ApprovalRequest,
        now: DateTime<Utc>,
        consume: bool,
    ) -> anyhow::Result<GateOutcome> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let latest = store.approvals.iter_mut().rev().find(|record| {
                record.requester_cutex_session_id == request.requester_cutex_session_id
                    && record.request_sha256 == request.request_sha256
                    && matches!(
                        record.state,
                        ApprovalState::Pending | ApprovalState::Approved | ApprovalState::Denied
                    )
            });
            if let Some(record) = latest {
                match record.state {
                    ApprovalState::Approved => {
                        if consume {
                            record.state = ApprovalState::Consumed;
                            record.consumed_at = Some(now.to_rfc3339());
                        }
                        return Ok(GateOutcome::Approved(record.clone()));
                    }
                    ApprovalState::Pending => {
                        // An earlier call's hold is not this call's.
                        let mut record = record.clone();
                        record.wait = None;
                        return Ok(GateOutcome::Pending {
                            record,
                            created: false,
                        });
                    }
                    ApprovalState::Denied => {
                        return Ok(GateOutcome::DeniedEarlier(record.clone()))
                    }
                    _ => {}
                }
            }
            if matches!(
                request.kind,
                ApprovalKind::BrowserClaim | ApprovalKind::BrowserOpen
            ) {
                anyhow::ensure!(
                    make_browser_room(&mut store.approvals),
                    "browser approval capacity"
                );
            }
            let record = ApprovalRecord {
                approval_id: format!("apr-{}", uuid::Uuid::new_v4().simple()),
                kind: request.kind,
                reason: request.reason,
                requester_cutex_session_id: request.requester_cutex_session_id,
                requester_name: request.requester_name,
                project_id: request.project_id,
                summary: request.summary,
                details: request.details,
                request_sha256: request.request_sha256,
                state: ApprovalState::Pending,
                created_at: now.to_rfc3339(),
                decided_at: None,
                note: None,
                decided_by: None,
                consumed_at: None,
                wait: None,
            };
            store.approvals.push(record.clone());
            Ok(GateOutcome::Pending {
                record,
                created: true,
            })
        })?
    }

    /// Claim requests never consume approval through an agent retry. Only
    /// consume_browser_claim below, reached by the authenticated owner selecting
    /// a concrete tab, can consume this kind. The digest excludes requestId.
    pub fn request_browser_claim(
        &self,
        request: ApprovalRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ApprovalRecord> {
        self.request_browser_claim_with_change(request, now).map(|(record, _)| record)
    }

    /// Changed means a new owner-visible request or a binding/state change,
    /// never just another polling requestId. Persistent store shape is unchanged.
    pub fn request_browser_claim_with_change(
        &self,
        request: ApprovalRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<(ApprovalRecord, bool)> {
        anyhow::ensure!(
            request.kind == ApprovalKind::BrowserClaim,
            "browser claim kind required"
        );
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            if let Some(record) = store.approvals.iter_mut().rev().find(|r| {
                r.kind == ApprovalKind::BrowserClaim
                    && r.requester_cutex_session_id == request.requester_cutex_session_id
                    && r.request_sha256 == request.request_sha256
                    && matches!(
                        r.state,
                        ApprovalState::Pending | ApprovalState::Approved | ApprovalState::Denied
                    )
            }) {
                // Only a freshly authenticated live caller reaches this API.
                // A pending request follows the live runtime. A prior approval
                // cannot transfer to its replacement: ask the owner again.
                // Denials stay sticky for this session/device generation.
                let binding_changed = ["threadId", "runtimeGeneration"].iter()
                    .any(|key| record.details[*key] != request.details[*key]);
                let changed = binding_changed && record.state != ApprovalState::Denied;
                if changed {
                    if record.state == ApprovalState::Approved {
                        record.state = ApprovalState::Pending;
                        record.decided_at = None;
                        record.decided_by = None;
                        record.consumed_at = None;
                        record.note = None;
                    }
                    for key in ["threadId", "runtimeGeneration"] {
                        record.details[key] = request.details[key].clone();
                    }
                }
                return Ok((record.clone(), changed));
            }
            anyhow::ensure!(
                make_browser_room(&mut store.approvals),
                "browser approval capacity"
            );
            let record = ApprovalRecord {
                approval_id: format!("apr-{}", uuid::Uuid::new_v4().simple()),
                kind: request.kind,
                reason: request.reason,
                requester_cutex_session_id: request.requester_cutex_session_id,
                requester_name: request.requester_name,
                project_id: request.project_id,
                summary: request.summary,
                details: request.details,
                request_sha256: request.request_sha256,
                state: ApprovalState::Pending,
                created_at: now.to_rfc3339(),
                decided_at: None,
                note: None,
                decided_by: None,
                consumed_at: None,
                wait: None,
            };
            store.approvals.push(record.clone());
            Ok((record, true))
        })?
    }

    /// Serialize a one-use open approval with queue admission, not execution.
    /// Failed admission leaves the approval intact; the old requestId remains
    /// settled by the separate idempotency store and is never replayed.
    pub fn try_consume_browser_open<T, E>(
        &self,
        id: &str,
        session: &str,
        now: DateTime<Utc>,
        admit: impl FnOnce() -> Result<T, E>,
    ) -> anyhow::Result<Option<Result<T, E>>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let Some(index) = store.approvals.iter().position(|r| {
                r.approval_id == id
                    && r.kind == ApprovalKind::BrowserOpen
                    && r.requester_cutex_session_id == session
                    && r.state == ApprovalState::Approved
            }) else {
                return Ok(None);
            };
            let previous = store.approvals[index].clone();
            store.approvals[index].state = ApprovalState::Consumed;
            store.approvals[index].consumed_at = Some(now.to_rfc3339());
            // Durable one-use proof must precede making a frame dispatchable.
            // Keep the store lock across admission; on deterministic failure
            // restore Approved before allowing another requester to inspect it.
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.path,
                &store,
                "browser open admission",
            )?;
            let result = admit();
            if result.is_err() {
                store.approvals[index] = previous;
                crate::config::atomic::write_private_pretty_json_atomic(
                    &self.path,
                    &store,
                    "browser open admission rollback",
                )?;
            }
            Ok(Some(result))
        })?
    }

    /// The owner selecting a concrete tab is the approval gesture. A preceding
    /// generic approve is allowed but is not sufficient to grant by itself.
    /// All identity/device/runtime bindings and the one-use transition are
    /// checked under the same approval store lock.
    #[allow(clippy::too_many_arguments)]
    pub fn consume_browser_claim(
        &self,
        approval_id: &str,
        session: &str,
        host: &str,
        device: &str,
        connection_generation: &str,
        thread: &str,
        runtime_generation: u64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ApprovalRecord>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let record = store.approvals.iter_mut().find(|r| {
                r.approval_id == approval_id
                    && r.kind == ApprovalKind::BrowserClaim
                    && r.requester_cutex_session_id == session
                    && r.details["threadId"] == thread
                    && r.details["runtimeGeneration"] == runtime_generation
                    && matches!(r.state, ApprovalState::Pending | ApprovalState::Approved)
            })?;
            // The authenticated owner selects the actual device and tab; the
            // request's initially preferred desktop is only a routing hint.
            record.details["hostId"] = json!(host);
            record.details["deviceId"] = json!(device);
            record.details["connectionGeneration"] = json!(connection_generation);
            record.state = ApprovalState::Consumed;
            record.decided_at.get_or_insert_with(|| now.to_rfc3339());
            record.consumed_at = Some(now.to_rfc3339());
            Some(record.clone())
        })
    }

    /// One record by id (after expiry), if it still exists.
    pub fn get(
        &self,
        approval_id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ApprovalRecord>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            store
                .approvals
                .iter()
                .find(|record| record.approval_id == approval_id)
                .cloned()
        })
    }

    /// The newest live (pending / approved / denied) record of a requester
    /// for exactly this request digest, without consuming it.
    pub fn peek(
        &self,
        requester: &str,
        request_sha256: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ApprovalRecord>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            store
                .approvals
                .iter()
                .rev()
                .find(|record| {
                    record.requester_cutex_session_id == requester
                        && record.request_sha256 == request_sha256
                        && matches!(
                            record.state,
                            ApprovalState::Pending
                                | ApprovalState::Approved
                                | ApprovalState::Denied
                        )
                })
                .cloned()
        })
    }

    /// Records newest first; pending only unless `include_decided`.
    pub fn list(
        &self,
        include_decided: bool,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<ApprovalRecord>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            store
                .approvals
                .iter()
                .rev()
                .filter(|record| include_decided || record.state == ApprovalState::Pending)
                .cloned()
                .collect()
        })
    }

    /// Approve or deny a pending approval.
    pub fn decide(
        &self,
        approval_id: &str,
        approve: bool,
        note: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<ApprovalRecord, DecideError> {
        self.decide_as(approval_id, approve, note, None, now)
    }

    /// [`Self::decide`], recording who decided (`decidedBy`).
    pub fn decide_as(
        &self,
        approval_id: &str,
        approve: bool,
        note: Option<String>,
        decided_by: Option<ApprovalDecider>,
        now: DateTime<Utc>,
    ) -> Result<ApprovalRecord, DecideError> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let record = store
                .approvals
                .iter_mut()
                .find(|record| record.approval_id == approval_id)
                .ok_or(DecideError::NotFound)?;
            if record.state != ApprovalState::Pending {
                return Err(DecideError::NotPending(record.state));
            }
            record.state = if approve {
                ApprovalState::Approved
            } else {
                ApprovalState::Denied
            };
            record.decided_at = Some(now.to_rfc3339());
            record.note = note;
            record.decided_by = decided_by;
            Ok(record.clone())
        })
        .map_err(|error| DecideError::Store(format!("{error:#}")))?
    }
}

impl ApprovalStore {
    /// Start a hold on a pending record (approval-wait). `None` when the
    /// record is no longer pending (decided in the meantime: the caller
    /// re-checks at once).
    pub fn begin_wait(
        &self,
        approval_id: &str,
        wait: ApprovalWait,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ApprovalRecord>> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let record = store
                .approvals
                .iter_mut()
                .find(|record| record.approval_id == approval_id)?;
            if record.state != ApprovalState::Pending {
                return None;
            }
            record.wait = Some(wait);
            Some(record.clone())
        })
    }

    /// End this call's hold (`wait_id`) atomically with what it answers. An
    /// approval is used only when `deliverable` (the caller is connected and
    /// still the runtime that asked) and the hold was not ended by someone
    /// else (Management: the caller's turn ended). `end` is the reason
    /// recorded when the record is still pending.
    pub fn finish_wait(
        &self,
        approval_id: &str,
        wait_id: &str,
        end: &str,
        deliverable: bool,
        now: DateTime<Utc>,
    ) -> anyhow::Result<WaitFinish> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let Some(record) = store
                .approvals
                .iter_mut()
                .find(|record| record.approval_id == approval_id)
            else {
                return WaitFinish::Gone;
            };
            let ended_by_other = record
                .wait
                .as_ref()
                .filter(|wait| wait.id == wait_id)
                .is_none_or(|wait| wait.end.is_some());
            let deliver = deliverable && !ended_by_other;
            let close = |record: &mut ApprovalRecord, reason: &str| {
                if let Some(wait) = record.wait.as_mut().filter(|wait| wait.id == wait_id) {
                    if wait.end.is_none() {
                        wait.end = Some(reason.to_string());
                        wait.ended_at = Some(now.to_rfc3339());
                    }
                }
            };
            match record.state {
                ApprovalState::Approved if deliver => {
                    record.state = ApprovalState::Consumed;
                    record.consumed_at = Some(now.to_rfc3339());
                    close(record, "approved");
                    WaitFinish::Approved(record.clone())
                }
                ApprovalState::Denied if deliver => {
                    close(record, "denied");
                    WaitFinish::Denied(record.clone())
                }
                ApprovalState::Approved | ApprovalState::Denied => {
                    close(record, if ended_by_other { "turn_ended" } else { end });
                    WaitFinish::Undelivered(record.clone())
                }
                ApprovalState::Pending => {
                    close(record, end);
                    WaitFinish::Pending(record.clone())
                }
                ApprovalState::Consumed | ApprovalState::Expired => WaitFinish::Gone,
            }
        })
    }

    /// Management: the caller's turn ended (`turn/completed` on `thread`), so
    /// its holds stop (`end = turn_ended`) and never use an approval. A hold
    /// whose turn is known ends only for that turn. Cheap when nothing holds
    /// (one lockless read). Returns the records whose hold ended.
    pub fn end_waits_for_turn(
        &self,
        requester: &str,
        thread: Option<&str>,
        turn: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<ApprovalRecord>> {
        let matches = |record: &ApprovalRecord| {
            record.requester_cutex_session_id == requester
                && record.wait.as_ref().is_some_and(|wait| {
                    wait.end.is_none()
                        && (thread.is_none()
                            || wait.thread_id.is_none()
                            || wait.thread_id.as_deref() == thread)
                        && (turn.is_none() || wait.turn_id.is_none() || wait.turn_id.as_deref() == turn)
                })
        };
        if !read_all_at(&self.path)?.iter().any(matches) {
            return Ok(Vec::new());
        }
        self.with_store(|store| {
            let mut ended = Vec::new();
            for record in store.approvals.iter_mut().filter(|record| matches(record)) {
                if let Some(wait) = record.wait.as_mut() {
                    wait.end = Some("turn_ended".into());
                    wait.ended_at = Some(now.to_rfc3339());
                }
                ended.push(record.clone());
            }
            ended
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove every record of `kind` (rollback to a release that cannot
    /// parse it, `upgrade::older_stores`); returns the removed records.
    pub fn remove_kind(&self, kind: ApprovalKind) -> anyhow::Result<Vec<ApprovalRecord>> {
        self.with_store(|store| {
            let (removed, kept) = std::mem::take(&mut store.approvals).into_iter().partition(|record| record.kind == kind);
            store.approvals = kept;
            removed
        })
    }
}

/// [`ApprovalStore::gate`], holding a pending request up to the waiter's
/// duration for the owner's decision (approval-wait): an approval within the
/// window lets the call proceed (consumed once, like a retry), a denial
/// answers denied, otherwise the call answers `approval_required` as before.
/// Without a waiter it is exactly `gate`. `on_requested` runs for a newly
/// created request (publish it). A decision this call could not deliver
/// (caller gone, turn ended) is handed to `on_undelivered` (the decision
/// notice).
pub fn gate_with_wait(
    store: &ApprovalStore,
    request: ApprovalRequest,
    waiter: Option<&Waiter>,
    on_requested: impl FnOnce(&ApprovalRecord),
    on_undelivered: impl FnOnce(&ApprovalRecord),
) -> anyhow::Result<GateOutcome> {
    let now = Utc::now();
    let outcome = store.gate(request, now)?;
    let GateOutcome::Pending { record, created } = &outcome else {
        return Ok(outcome);
    };
    if *created {
        on_requested(record);
    }
    let Some(waiter) = waiter.filter(|waiter| !waiter.duration.is_zero()) else {
        return Ok(outcome);
    };
    let started = std::time::Instant::now();
    let wait_id = uuid::Uuid::new_v4().simple().to_string();
    let until = now + Duration::from_std(waiter.duration).unwrap_or_else(|_| Duration::seconds(0));
    let begun = store.begin_wait(
        &record.approval_id,
        ApprovalWait {
            id: wait_id.clone(),
            started_at: now.to_rfc3339(),
            until: until.to_rfc3339(),
            thread_id: waiter.thread_id.clone(),
            turn_id: waiter.turn_id.clone(),
            ended_at: None,
            end: None,
        },
        now,
    )?;
    if begun.is_none() {
        // Decided between the gate and the hold: answer like a retry would
        // (an approval used here still counts as decided during this call).
        return store.gate_existing(&record.approval_id, &wait_id, now);
    }
    let path = store.path().to_path_buf();
    let mut stamp = None;
    let mut polls = 0_u32;
    let end = loop {
        if !(waiter.connected)() {
            break "disconnected";
        }
        if started.elapsed() >= waiter.duration {
            break "timeout";
        }
        // Lockless poll; re-read when the file changed (and every few polls,
        // in case two writes share a modification time and length).
        let now_stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()));
        polls = polls.wrapping_add(1);
        if now_stamp != stamp || polls.is_multiple_of(5) {
            stamp = now_stamp;
            let row = read_all_at(&path)?
                .into_iter()
                .find(|row| row.approval_id == record.approval_id);
            let settled = match &row {
                None => true,
                Some(row) => {
                    row.state != ApprovalState::Pending
                        || row.wait.as_ref().is_none_or(|wait| wait.id != wait_id || wait.end.is_some())
                }
            };
            if settled {
                // Decided, taken over or ended by Management: finish_wait
                // decides under the lock (the reason applies only if still
                // pending, which then means another call holds it).
                break "superseded";
            }
        }
        std::thread::sleep(waiter.poll.min(waiter.duration.saturating_sub(started.elapsed())).max(std::time::Duration::from_millis(10)));
    };
    let deliverable = end != "disconnected" && (waiter.connected)() && (waiter.current)();
    let end = if end != "disconnected" && !deliverable {
        if (waiter.connected)() { "runtime_changed" } else { "disconnected" }
    } else {
        end
    };
    match store.finish_wait(&record.approval_id, &wait_id, end, deliverable, Utc::now())? {
        WaitFinish::Approved(record) => Ok(GateOutcome::Approved(record)),
        WaitFinish::Denied(record) => Ok(GateOutcome::Denied(record)),
        WaitFinish::Pending(record) => Ok(GateOutcome::Pending { record, created: false }),
        WaitFinish::Undelivered(row) => {
            on_undelivered(&row);
            Ok(GateOutcome::Pending { record: row, created: false })
        }
        WaitFinish::Gone => Ok(GateOutcome::Pending { record: record.clone(), created: false }),
    }
}

impl ApprovalStore {
    /// What a retry of an existing record's request would answer now
    /// (consumes an approval).
    fn gate_existing(
        &self,
        approval_id: &str,
        wait_id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<GateOutcome> {
        self.with_store(|store| {
            expire_and_prune(&mut store.approvals, now);
            let record = store
                .approvals
                .iter_mut()
                .find(|record| record.approval_id == approval_id)
                .context("approval vanished")?;
            Ok(match record.state {
                ApprovalState::Approved => {
                    record.state = ApprovalState::Consumed;
                    record.consumed_at = Some(now.to_rfc3339());
                    // Approved while this call was in flight (it had not
                    // begun holding yet): a zero-length hold that ended
                    // approved, so the result says the owner decided.
                    record.wait = Some(ApprovalWait {
                        id: wait_id.to_string(),
                        started_at: now.to_rfc3339(),
                        until: now.to_rfc3339(),
                        thread_id: None,
                        turn_id: None,
                        ended_at: Some(now.to_rfc3339()),
                        end: Some("approved".into()),
                    });
                    GateOutcome::Approved(record.clone())
                }
                ApprovalState::Denied => GateOutcome::Denied(record.clone()),
                _ => GateOutcome::Pending { record: record.clone(), created: false },
            })
        })?
    }
}

/// When a denial stops answering identical requests ([`expire_and_prune`]).
fn denial_expiry(record: &ApprovalRecord) -> Option<String> {
    let since = record
        .decided_at
        .as_deref()
        .and_then(timestamp)
        .or_else(|| timestamp(&record.created_at))?;
    Some((since + Duration::hours(APPROVAL_TTL_HOURS)).to_rfc3339())
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn expire_and_prune(records: &mut Vec<ApprovalRecord>, now: DateTime<Utc>) {
    let ttl = Duration::hours(APPROVAL_TTL_HOURS);
    for record in records.iter_mut() {
        let since = match record.state {
            ApprovalState::Pending => timestamp(&record.created_at),
            ApprovalState::Approved | ApprovalState::Denied => record
                .decided_at
                .as_deref()
                .and_then(timestamp)
                .or_else(|| timestamp(&record.created_at)),
            _ => None,
        };
        if since.is_some_and(|since| now - since > ttl) {
            record.state = ApprovalState::Expired;
        }
    }
    let retention = Duration::days(RETENTION_DAYS);
    records.retain(|record| {
        record.state == ApprovalState::Pending
            || timestamp(&record.created_at).is_none_or(|created| now - created <= retention)
    });
    if records.len() > MAX_RECORDS {
        let mut excess = records.len() - MAX_RECORDS;
        records.retain(|record| {
            if excess > 0
                && matches!(
                    record.state,
                    ApprovalState::Consumed | ApprovalState::Expired
                )
            {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}
fn make_browser_room(records: &mut Vec<ApprovalRecord>) -> bool {
    if records.len() < MAX_RECORDS {
        return true;
    }
    // Never erase pending/approved authority or a live denial under pressure.
    if let Some(index) = records
        .iter()
        .position(|r| matches!(r.state, ApprovalState::Consumed | ApprovalState::Expired))
    {
        records.remove(index);
    }
    records.len() < MAX_RECORDS
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "cutex.00000000-0000-0000-0000-000000000001";

    fn store(label: &str) -> (ApprovalStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "cutex-approvals-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        (ApprovalStore::at(dir.join("approvals.json")), dir)
    }

    fn request(digest: &str) -> ApprovalRequest {
        ApprovalRequest {
            kind: ApprovalKind::ManagedService,
            reason: "outside_caller_envelope".into(),
            requester_cutex_session_id: AGENT.into(),
            requester_name: Some("Builder".into()),
            project_id: Some("alpha".into()),
            summary: "Run managed service web: npm run dev".into(),
            details: json!({"slug": "web"}),
            request_sha256: digest.into(),
        }
    }

    #[test]
    fn pending_then_approved_is_consumed_once() {
        let (store, dir) = store("flow");
        let now = Utc::now();
        let GateOutcome::Pending {
            record,
            created: true,
        } = store.gate(request("d1"), now).unwrap()
        else {
            panic!("first request must be pending");
        };
        assert_eq!(
            store.gate(request("d1"), now).unwrap().refusal().unwrap()["code"],
            "approval_required"
        );
        assert!(matches!(
            store.gate(request("d1"), now).unwrap(),
            GateOutcome::Pending { created: false, .. }
        ));
        assert_eq!(store.list(false, now).unwrap().len(), 1);
        let approved = store.decide(&record.approval_id, true, None, now).unwrap();
        assert_eq!(approved.state, ApprovalState::Approved);
        assert_eq!(
            store.decide(&record.approval_id, false, None, now),
            Err(DecideError::NotPending(ApprovalState::Approved))
        );
        assert!(matches!(
            store.gate(request("d1"), now).unwrap(),
            GateOutcome::Approved(_)
        ));
        // Consumed: the next identical request asks again.
        assert!(matches!(
            store.gate(request("d1"), now).unwrap(),
            GateOutcome::Pending { created: true, .. }
        ));
        // A different request never matches another approval.
        assert!(matches!(
            store.gate(request("d2"), now).unwrap(),
            GateOutcome::Pending { created: true, .. }
        ));
        assert_eq!(
            store.decide("apr-missing", true, None, now),
            Err(DecideError::NotFound)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denial_is_answered_then_expires() {
        let (store, dir) = store("deny");
        let now = Utc::now();
        let GateOutcome::Pending { record, .. } = store.gate(request("d"), now).unwrap() else {
            panic!("pending");
        };
        store
            .decide(
                &record.approval_id,
                false,
                Some("use an external service".into()),
                now,
            )
            .unwrap();
        let denied = store.gate(request("d"), now).unwrap();
        assert!(matches!(denied, GateOutcome::DeniedEarlier(_)));
        let refusal = denied.refusal().unwrap();
        assert_eq!(refusal["code"], "approval_denied");
        assert_eq!(refusal["approval_id"], record.approval_id.as_str());
        let detail = refusal["detail"].as_str().unwrap();
        let until = (now + Duration::hours(APPROVAL_TTL_HOURS)).to_rfc3339();
        assert!(detail.contains("external service"), "{detail}");
        assert!(detail.contains("already denied this exact request at"), "{detail}");
        assert!(detail.contains(&now.to_rfc3339()), "{detail}");
        assert!(detail.contains(&format!("stays denied until {until}")), "{detail}");
        assert!(detail.contains("Change the request"), "{detail}");
        // The desktop classifies a tool result by these words: never both.
        assert!(!refusal.to_string().contains("approval_required"));
        assert_eq!(refusal["denied_until"], until.as_str());
        assert_eq!(refusal["reused_denial"], true);
        let later = now + Duration::hours(APPROVAL_TTL_HOURS + 1);
        assert!(matches!(
            store.gate(request("d"), later).unwrap(),
            GateOutcome::Pending { created: true, .. }
        ));
        let all = store.list(true, later).unwrap();
        assert!(all
            .iter()
            .any(|record| record.state == ApprovalState::Expired));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn digest_ignores_key_order() {
        assert_eq!(
            request_digest(&json!({"a": 1, "b": {"y": 2, "x": [1, {"q": 1, "p": 2}]}})),
            request_digest(&json!({"b": {"x": [1, {"p": 2, "q": 1}], "y": 2}, "a": 1}))
        );
        assert_ne!(
            request_digest(&json!({"a": 1})),
            request_digest(&json!({"a": 2}))
        );
    }

    fn browser_claim(digest: &str) -> ApprovalRequest {
        let mut request = request(digest);
        request.kind = ApprovalKind::BrowserClaim;
        request.details = json!({"hostId":"local","deviceId":"d","connectionGeneration":"g","threadId":"t","runtimeGeneration":1});
        request
    }
    #[test]
    fn browser_claim_only_owner_selection_consumes_exact_binding_once() {
        let (store, dir) = store("browser-claim");
        let now = Utc::now();
        let record = store
            .request_browser_claim(browser_claim("claim1"), now)
            .unwrap();
        // Agent retries do not consume even after a generic approval.
        store.decide(&record.approval_id, true, None, now).unwrap();
        assert_eq!(
            store
                .request_browser_claim(browser_claim("claim1"), now)
                .unwrap()
                .state,
            ApprovalState::Approved
        );
        for (session, host, device, generation, thread, runtime) in [
            ("other", "local", "d", "g", "t", 1),
            (AGENT, "local", "d", "g", "old", 1),
            (AGENT, "local", "d", "g", "t", 2),
        ] {
            assert!(store
                .consume_browser_claim(
                    &record.approval_id,
                    session,
                    host,
                    device,
                    generation,
                    thread,
                    runtime,
                    now
                )
                .unwrap()
                .is_none());
        }
        let consumed = store
            .consume_browser_claim(
                &record.approval_id,
                AGENT,
                "ssh-alias",
                "actual-owner-device",
                "new-generation",
                "t",
                1,
                now,
            )
            .unwrap()
            .unwrap();
        assert_eq!(consumed.state, ApprovalState::Consumed);
        assert_eq!(consumed.details["deviceId"], "actual-owner-device");
        assert_eq!(consumed.details["hostId"], "ssh-alias");
        assert_eq!(consumed.details["connectionGeneration"], "new-generation");
        assert!(store
            .consume_browser_claim(&record.approval_id, AGENT, "local", "d", "g", "t", 1, now)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .request_browser_claim(browser_claim("claim2"), now)
                .unwrap()
                .state,
            ApprovalState::Pending
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_claim_denied_expired_and_wrong_kind_cannot_be_granted() {
        let (store, dir) = store("browser-refusal");
        let now = Utc::now();
        let denied = store
            .request_browser_claim(browser_claim("denied"), now)
            .unwrap();
        store.decide(&denied.approval_id, false, None, now).unwrap();
        assert!(store
            .consume_browser_claim(&denied.approval_id, AGENT, "local", "d", "g", "t", 1, now)
            .unwrap()
            .is_none());
        let expired = store
            .request_browser_claim(browser_claim("expired"), now)
            .unwrap();
        assert!(store
            .consume_browser_claim(
                &expired.approval_id,
                AGENT,
                "local",
                "d",
                "g",
                "t",
                1,
                now + Duration::hours(25)
            )
            .unwrap()
            .is_none());
        assert!(store
            .request_browser_claim(request("wrong-kind"), now)
            .is_err());
        let pending = store
            .request_browser_claim(browser_claim("selected"), now)
            .unwrap();
        assert_eq!(
            store
                .consume_browser_claim(&pending.approval_id, AGENT, "local", "d", "g", "t", 1, now)
                .unwrap()
                .unwrap()
                .state,
            ApprovalState::Consumed
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_claim_pending_follows_runtime_denial_sticks_and_approval_requires_reconfirmation() {
        let (store, dir) = store("browser-sticky");
        let now = Utc::now();
        let first = store
            .request_browser_claim(browser_claim("stable"), now)
            .unwrap();
        let mut newer = browser_claim("stable");
        newer.details["runtimeGeneration"] = json!(2);
        newer.details["threadId"] = json!("t2");
        let updated = store.request_browser_claim(newer.clone(), now).unwrap();
        assert_eq!(updated.approval_id, first.approval_id);
        assert_eq!(updated.details["runtimeGeneration"], 2);
        assert!(store
            .consume_browser_claim(&first.approval_id, AGENT, "local", "d", "g", "t", 1, now)
            .unwrap()
            .is_none());
        store.decide(&first.approval_id, false, None, now).unwrap();
        newer.details["runtimeGeneration"] = json!(3);
        let denied = store.request_browser_claim(newer.clone(), now).unwrap();
        assert_eq!(denied.state, ApprovalState::Denied);
        assert_eq!(denied.approval_id, first.approval_id);
        assert_eq!(denied.details["runtimeGeneration"], 2);
        let after = store
            .request_browser_claim(newer, now + Duration::hours(25))
            .unwrap();
        assert_ne!(after.approval_id, first.approval_id);
        store
            .decide(&after.approval_id, true, None, now + Duration::hours(25))
            .unwrap();
        let old = store
            .request_browser_claim(browser_claim("stable"), now + Duration::hours(25))
            .unwrap();
        assert_eq!(old.details["runtimeGeneration"], 1);
        assert_eq!(old.state, ApprovalState::Pending);
        assert_eq!(old.approval_id, after.approval_id);
        assert!(old.decided_at.is_none() && old.consumed_at.is_none() && old.note.is_none());
        assert!(store.consume_browser_claim(&old.approval_id, AGENT, "local", "d", "g", "t2", 3, now + Duration::hours(25)).unwrap().is_none());
        // A fresh explicit owner selection is the confirmation; no agent poll
        // consumes it, and the replaced runtime cannot use this pending record.
        assert!(store.consume_browser_claim(&old.approval_id, AGENT, "local", "d", "g", "t", 1, now + Duration::hours(25)).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_claim_reports_changed_only_for_creation_and_owner_visible_binding_change() {
        let (store, dir) = store("browser-change-event");
        let now = Utc::now();
        let (first, changed) = store.request_browser_claim_with_change(browser_claim("stable"), now).unwrap();
        assert!(changed);
        for n in 0..8 {
            let mut request = browser_claim("stable");
            request.details["requestId"] = json!(format!("poll-{n}"));
            let (same, changed) = store.request_browser_claim_with_change(request, now).unwrap();
            assert!(!changed);
            assert_eq!(same.approval_id, first.approval_id);
        }
        let mut replacement = browser_claim("stable");
        replacement.details["threadId"] = json!("t2");
        replacement.details["runtimeGeneration"] = json!(2);
        assert!(store.request_browser_claim_with_change(replacement.clone(), now).unwrap().1);
        assert!(!store.request_browser_claim_with_change(replacement.clone(), now).unwrap().1);
        store.decide(&first.approval_id, true, None, now).unwrap();
        assert!(!store.request_browser_claim_with_change(replacement, now).unwrap().1);
        let (reset, changed) = store.request_browser_claim_with_change(browser_claim("stable"), now).unwrap();
        assert!(changed);
        assert_eq!(reset.state, ApprovalState::Pending);
        store.decide(&first.approval_id, false, None, now).unwrap();
        let mut replacement = browser_claim("stable");
        replacement.details["runtimeGeneration"] = json!(3);
        assert!(!store.request_browser_claim_with_change(replacement, now).unwrap().1);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_approval_capacity_never_evicts_other_pending_or_live_denied() {
        let (store, dir) = store("browser-capacity");
        let now = Utc::now();
        let GateOutcome::Pending { record, .. } =
            store.gate(request("other-pending"), now).unwrap()
        else {
            panic!()
        };
        store
            .with_store(|s| {
                let template = s.approvals[0].clone();
                for n in 1..MAX_RECORDS {
                    let mut copy = template.clone();
                    copy.approval_id = format!("apr-{n}");
                    copy.request_sha256 = n.to_string();
                    s.approvals.push(copy);
                }
            })
            .unwrap();
        assert!(store
            .request_browser_claim(browser_claim("new"), now)
            .is_err());
        let mut open = request("open");
        open.kind = ApprovalKind::BrowserOpen;
        assert!(store.gate_browser_open(open, now).is_err());
        let records = store.list(true, now).unwrap();
        assert_eq!(records.len(), MAX_RECORDS);
        assert!(records.iter().any(|r| r.approval_id == record.approval_id));
        assert!(records.iter().all(|r| r.state == ApprovalState::Pending));
        let _ = std::fs::remove_dir_all(dir);
    }

    // approval-wait ------------------------------------------------------

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn waiter(millis: u64, connected: Arc<AtomicBool>, current: Arc<AtomicBool>) -> Waiter {
        let mut waiter = Waiter::new(
            1,
            Some("thread-1".into()),
            Some("turn-1".into()),
            move || connected.load(Ordering::SeqCst),
            move || current.load(Ordering::SeqCst),
        );
        waiter.duration = std::time::Duration::from_millis(millis);
        waiter.poll = std::time::Duration::from_millis(10);
        waiter
    }

    fn flags() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)))
    }

    /// Run `act` on the pending record once the call holds for it.
    fn when_holding(path: PathBuf, act: impl FnOnce(&ApprovalStore, &ApprovalRecord) + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let store = ApprovalStore::at(&path);
            for _ in 0..500 {
                if let Some(record) = read_all_at(&path).unwrap_or_default().into_iter().find(|r| r.wait.as_ref().is_some_and(|w| w.end.is_none())) {
                    act(&store, &record);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the call never held");
        })
    }

    fn notices() -> (Arc<AtomicUsize>, impl Fn(&ApprovalRecord)) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        (count, move |_: &ApprovalRecord| {
            seen.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_holding_call_proceeds_once_approved_in_the_window() {
        let (store, dir) = store("wait-approve");
        let (connected, current) = flags();
        let decider = when_holding(store.path().to_path_buf(), |store, record| {
            let wait = record.wait.clone().unwrap();
            assert_eq!((wait.thread_id.as_deref(), wait.turn_id.as_deref()), (Some("thread-1"), Some("turn-1")));
            // Management sees the hold: no decision notice is due.
            assert!(wait.active(Utc::now()));
            store.decide(&record.approval_id, true, None, Utc::now()).unwrap();
        });
        let requested = AtomicUsize::new(0);
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w1"), Some(&waiter(5_000, connected, current)), |_| { requested.fetch_add(1, Ordering::SeqCst); }, notice).unwrap();
        decider.join().unwrap();
        // Q21: the successful result says the owner approved it during the hold.
        let note = outcome.approved_after_wait().expect("approved-after-wait note");
        let GateOutcome::Approved(record) = outcome else { panic!("{outcome:?}") };
        assert_eq!(note["decision"], "approved");
        assert_eq!(note["approvalId"], record.approval_id.as_str());
        assert!(note["waitedSecs"].as_i64().is_some_and(|secs| (0..=5).contains(&secs)), "{note}");
        assert_eq!(note["detail"].as_str().unwrap(), approved_after_wait_detail(note["waitedSecs"].as_i64().unwrap()));
        assert!(note["detail"].as_str().unwrap().starts_with("The owner approved this request after "));
        assert_eq!(record.state, ApprovalState::Consumed);
        assert_eq!(record.wait.unwrap().end.as_deref(), Some("approved"));
        assert_eq!(requested.load(Ordering::SeqCst), 1);
        assert_eq!(undelivered.load(Ordering::SeqCst), 0);
        // Consumed once: the same request asks again.
        assert!(matches!(store.gate(request("w1"), Utc::now()).unwrap(), GateOutcome::Pending { created: true, .. }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_holding_call_answers_denied_when_denied_in_the_window() {
        let (store, dir) = store("wait-deny");
        let (connected, current) = flags();
        let decider = when_holding(store.path().to_path_buf(), |store, record| {
            store.decide(&record.approval_id, false, Some("no".into()), Utc::now()).unwrap();
        });
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w2"), Some(&waiter(5_000, connected, current)), |_| {}, notice).unwrap();
        decider.join().unwrap();
        assert_eq!(outcome.refusal().unwrap()["code"], "approval_denied");
        // Decided during this call's hold: a fresh denial, not the earlier-decision text.
        assert!(matches!(outcome, GateOutcome::Denied(_)));
        assert_eq!(outcome.refusal().unwrap()["detail"], "The owner denied this request: no");
        assert_eq!(undelivered.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_holding_call_times_out_with_approval_required_and_stays_pending() {
        let (store, dir) = store("wait-timeout");
        let (connected, current) = flags();
        let started = std::time::Instant::now();
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w3"), Some(&waiter(150, connected, current)), |_| {}, notice).unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(150));
        let refusal = outcome.refusal().unwrap();
        assert_eq!(refusal["code"], "approval_required");
        assert!(refusal["detail"].as_str().unwrap().contains("did not decide while this call waited"), "{refusal}");
        let pending = store.list(false, Utc::now()).unwrap();
        assert_eq!(pending.len(), 1);
        let wait = pending[0].wait.clone().unwrap();
        assert_eq!(wait.end.as_deref(), Some("timeout"));
        assert!(!wait.active(Utc::now()));
        assert_eq!(undelivered.load(Ordering::SeqCst), 0);
        // A later plain retry does not claim it waited.
        let retry = store.gate(request("w3"), Utc::now()).unwrap().refusal().unwrap();
        assert!(!retry["detail"].as_str().unwrap().contains("waited"));
        // Approved afterwards: a retry proceeds as before, without the
        // approved-after-wait note (this call did not hold for it).
        store.decide(&pending[0].approval_id, true, None, Utc::now()).unwrap();
        let retried = gate_with_wait(&store, request("w3"), None, |_| {}, |_| {}).unwrap();
        assert!(matches!(retried, GateOutcome::Approved(_)));
        assert_eq!(retried.approved_after_wait(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_disconnected_caller_stops_holding_and_leaves_the_approval_unused() {
        let (store, dir) = store("wait-disconnect");
        let (connected, current) = flags();
        let gone = connected.clone();
        let decider = when_holding(store.path().to_path_buf(), move |store, record| {
            gone.store(false, Ordering::SeqCst);
            store.decide(&record.approval_id, true, None, Utc::now()).unwrap();
        });
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w4"), Some(&waiter(5_000, connected, current)), |_| {}, notice).unwrap();
        decider.join().unwrap();
        assert!(matches!(outcome, GateOutcome::Pending { .. }), "{outcome:?}");
        let record = store.peek(AGENT, "w4", Utc::now()).unwrap().unwrap();
        // Either the hold saw the disconnect first (pending, then approved
        // with no hold: Management sends the notice), or the approval first
        // (unused: the Bus sends the notice). Never consumed.
        assert_eq!(record.state, ApprovalState::Approved);
        assert_eq!(record.wait.unwrap().end.as_deref(), Some("disconnected"));
        assert!(undelivered.load(Ordering::SeqCst) <= 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_ended_turn_stops_the_hold_without_using_a_later_approval() {
        let (store, dir) = store("wait-turn");
        let (connected, current) = flags();
        let decider = when_holding(store.path().to_path_buf(), |store, record| {
            // Another turn of the same thread ending is not this call's.
            assert!(store.end_waits_for_turn(AGENT, Some("thread-1"), Some("turn-0"), Utc::now()).unwrap().is_empty());
            assert!(store.end_waits_for_turn(AGENT, Some("thread-2"), None, Utc::now()).unwrap().is_empty());
            assert!(store.end_waits_for_turn("cutex.other", None, None, Utc::now()).unwrap().is_empty());
            let ended = store.end_waits_for_turn(AGENT, Some("thread-1"), Some("turn-1"), Utc::now()).unwrap();
            assert_eq!(ended.len(), 1);
            store.decide(&record.approval_id, true, None, Utc::now()).unwrap();
        });
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w5"), Some(&waiter(5_000, connected, current)), |_| {}, notice).unwrap();
        decider.join().unwrap();
        assert!(matches!(outcome, GateOutcome::Pending { .. }), "{outcome:?}");
        let record = store.peek(AGENT, "w5", Utc::now()).unwrap().unwrap();
        assert_eq!(record.state, ApprovalState::Approved, "left for a retry");
        assert_eq!(record.wait.unwrap().end.as_deref(), Some("turn_ended"));
        assert!(undelivered.load(Ordering::SeqCst) <= 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replaced_runtime_never_uses_the_approval() {
        let (store, dir) = store("wait-runtime");
        let (connected, current) = flags();
        let stale = current.clone();
        let decider = when_holding(store.path().to_path_buf(), move |store, record| {
            stale.store(false, Ordering::SeqCst);
            store.decide(&record.approval_id, true, None, Utc::now()).unwrap();
        });
        let (undelivered, notice) = notices();
        let outcome = gate_with_wait(&store, request("w6"), Some(&waiter(5_000, connected, current)), |_| {}, notice).unwrap();
        decider.join().unwrap();
        assert!(matches!(outcome, GateOutcome::Pending { .. }), "{outcome:?}");
        let record = store.peek(AGENT, "w6", Utc::now()).unwrap().unwrap();
        assert_eq!(record.state, ApprovalState::Approved);
        assert_eq!(record.wait.unwrap().end.as_deref(), Some("runtime_changed"));
        assert_eq!(undelivered.load(Ordering::SeqCst), 1, "the requester gets the notice instead");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn approval_note_attaches_to_results_and_facades_opt_in() {
        let (store, dir) = store("note");
        let GateOutcome::Pending { mut record, .. } = store.gate(request("n1"), Utc::now()).unwrap() else {
            panic!("pending expected");
        };
        let _ = std::fs::remove_dir_all(dir);
        // Pending, or approved without a hold that ended approved: no note.
        assert_eq!(record.approved_after_wait(), None);
        record.wait = Some(ApprovalWait {
            id: "w".into(),
            started_at: "2026-10-18T00:00:00Z".into(),
            until: "2026-10-18T00:02:00Z".into(),
            thread_id: None,
            turn_id: None,
            ended_at: Some("2026-10-18T00:00:41.600Z".into()),
            end: Some("timeout".into()),
        });
        assert_eq!(record.approved_after_wait(), None);
        record.wait.as_mut().unwrap().end = Some("approved".into());
        let note = record.approved_after_wait().unwrap();
        assert_eq!(note, json!({
            "decision": "approved",
            "waitedSecs": 42,
            "approvalId": record.approval_id,
            "detail": "The owner approved this request after 42 s; it has been carried out.",
        }));
        let mut result = json!({"ok": true, "created": true});
        attach_approval_note(&mut result, note.clone());
        assert_eq!(result["approval"], note);
        assert_eq!(result["detail"], note["detail"]);
        let mut with_detail = json!({"ok": true, "detail": "Started."});
        attach_approval_note(&mut with_detail, note.clone());
        assert_eq!(with_detail["detail"], "The owner approved this request after 42 s; it has been carried out. Started.");
        assert!(facade_takes_approval_note(Some("delegations=2, approval-note=1")));
        assert!(facade_takes_approval_note(Some("approval-note=2")));
        assert!(!facade_takes_approval_note(Some("delegations=2")));
        assert!(!facade_takes_approval_note(Some("approval-note=0")));
        assert!(!facade_takes_approval_note(None));
    }

    #[test]
    fn without_a_waiter_the_gate_answers_at_once() {
        let (store, dir) = store("wait-none");
        let started = std::time::Instant::now();
        let outcome = gate_with_wait(&store, request("w7"), None, |_| {}, |_| {}).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(matches!(outcome, GateOutcome::Pending { created: true, .. }));
        assert!(store.list(false, Utc::now()).unwrap()[0].wait.is_none());
        assert_eq!(parse_wait_secs(Some("120")), Some(120));
        assert_eq!(parse_wait_secs(Some("999")), Some(APPROVAL_WAIT_SECS));
        assert_eq!(parse_wait_secs(Some("0")), None);
        assert_eq!(parse_wait_secs(Some("x")), None);
        assert_eq!(parse_wait_secs(None), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
