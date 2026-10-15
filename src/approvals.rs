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
    WorkflowRun,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
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
    /// The owner denied this request.
    Denied(ApprovalRecord),
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
                    "Nothing was done yet: this needs the owner's approval ({}). The owner sees the request in the cutex desktop. Retry the same call after they approve; until then it answers approval_required. Do not poll in a loop: end your turn or set an alarm.",
                    record.summary
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
        }
    }
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

fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

impl ApprovalStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(
            crate::config::paths::runtime_dir()?.join("approvals/approvals.json"),
        ))
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
                        return Ok(GateOutcome::Pending {
                            record: record.clone(),
                            created: false,
                        })
                    }
                    ApprovalState::Denied => return Ok(GateOutcome::Denied(record.clone())),
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
                consumed_at: None,
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
                consumed_at: None,
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
            Ok(record.clone())
        })
        .map_err(|error| DecideError::Store(format!("{error:#}")))?
    }
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
        assert_eq!(denied.refusal().unwrap()["code"], "approval_denied");
        assert!(denied.refusal().unwrap()["detail"]
            .as_str()
            .unwrap()
            .contains("external service"));
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
}
