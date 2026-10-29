//! Management-owned part of the Director address extension (design 044):
//! the authority-transition sidecar around `bind_project_authority`
//! (§6: prepared intent -> authority CAS -> finalize / recover), the passive
//! Director-change notices (§3) and the review / adoption of start messages
//! that only a Director change invalidated (§5).
//!
//! Every record here lives in the owner-private extension store
//! (`runtime/director-address`, owned by the core [`ExtensionStateStore`]
//! implementation). Nothing is added to the r253 `deny_unknown_fields`
//! records (ProjectAuthority, ManagedAgentRecord, AgentActionRecord, ...): an
//! r253 binary never reads these records, and none of them grants authority by
//! itself. The current authority in the Agent Management roster always wins.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Digest};

use crate::agent_management::{
    current_project_id, AgentActionId, AgentActionPhase, AgentActionRecord, AgentManagementError,
    AgentManagementMessageMetadata, AgentManagementSnapshot, AgentOperationKind,
    AuthorityTransitionHook, AuthorityTransitionIntent, ProjectAuthority, ProjectAuthorityReceipt,
    ProjectId,
};
use crate::role_revision::CutexSessionId;

/// Version of [`ManagementExtension`].
pub const MANAGEMENT_EXTENSION_VERSION: u32 = 1;
/// Control type of a start message re-issued through an adoption receipt
/// (§5). Only the in-process Management principal may queue it, and only
/// [`validate_adopted_start`] admits it.
pub const START_ADOPTED_CONTROL_TYPE: &str = "cutex.agent_management.startAdopted.v1";
/// View of the passive Director-change notice (cutex service external input).
pub const DIRECTOR_CHANGE_NOTICE_VIEW: &str = "cutex.project.directorChanged.v1";

// ---------------------------------------------------------------- records

/// The management-owned part of the extension state (`management-v1.json`,
/// its own file; the core ownership store is separate). Unknown top-level
/// fields written by a newer build of the same version are kept on rewrite
/// (`extra`); unknown fields inside records are not. A file of a NEWER
/// version is never rewritten and refuses Director changes (fail closed,
/// [`ManagementHook::prepare`]).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementExtension {
    #[serde(default = "version")]
    pub version: u32,
    /// Authority epoch per project already accounted for (baseline at first
    /// open, then every finalized / recovered transition). A higher roster
    /// epoch without a transition record is a change that bypassed the hook.
    #[serde(default)]
    pub accounted_epochs: BTreeMap<ProjectId, u64>,
    /// Keyed by change id.
    #[serde(default)]
    pub transitions: BTreeMap<String, TransitionRecord>,
    /// Keyed by `notice_key(project, newEpoch, recipient)`.
    #[serde(default)]
    pub notices: BTreeMap<String, NoticeRecord>,
    /// Keyed by review id (derived from the source action / message).
    #[serde(default)]
    pub start_reviews: BTreeMap<String, StartReviewItem>,
    /// Keyed by adoption id (derived from the approving action id).
    #[serde(default)]
    pub adoptions: BTreeMap<String, AdoptionReceipt>,
    /// Source message ids an owner / Director discarded: never re-queued by a
    /// later scan.
    #[serde(default)]
    pub discarded_sources: BTreeSet<String>,
    /// Unknown top-level fields, preserved on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn version() -> u32 {
    MANAGEMENT_EXTENSION_VERSION
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeReason {
    /// First bind of a project: no previous Director, no notice.
    Initial,
    /// Same session, new epoch: only the fence moves; no notice, no handover.
    EpochRefresh,
    /// Owner / root rebound the Director without replacement evidence.
    OwnerAssigned,
    /// Rebound to the successor of a completed `member_replace` of the old
    /// Director, whose record is retired.
    ReplacementConfirmed,
    /// Found by recovery: the roster moved without this hook (old binary,
    /// lost prepare). Needs review; no chain is invented.
    Recovery,
    /// The owner vacated the seat (`clear_director`): no new Director, no
    /// member notice; only the owner may decide the old Director's work.
    Cleared,
}

/// Default handling of the old Director's project delegations (owner D1).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HandoverPolicy {
    /// No Director change (initial bind / epoch refresh).
    None,
    /// Trusted replacement and the old session is retired: project-stamped
    /// delegations move to the new Director automatically.
    AutoProjectStamped,
    /// Everything else: a review list for the owner / new Director.
    Review,
    /// No trustworthy evidence at all (legacy / bypassed change): only the
    /// owner may approve a scope.
    OwnerApprovedScopeOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransitionState {
    /// Written before the CAS; the CAS may or may not have happened.
    Prepared,
    /// The authority receipt proved the CAS; notices are in the outbox.
    Finalized,
    /// The CAS happened, but the authority had already moved on when this was
    /// finalized: kept for the audit chain, no notice / handover of its own.
    Superseded,
    /// Recovery found no receipt: the CAS never happened.
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionRecord {
    pub change_id: String,
    pub project_id: ProjectId,
    /// The authority bind action (its receipt is the commit witness).
    pub authority_action_id: Option<AgentActionId>,
    pub request_sha256: Option<String>,
    pub old_director: Option<CutexSessionId>,
    /// `None`: the seat was cleared (`newEpoch` is then the cleared epoch).
    pub new_director: Option<CutexSessionId>,
    pub old_epoch: Option<u64>,
    pub new_epoch: u64,
    pub reason: ChangeReason,
    pub handover: HandoverPolicy,
    /// The completed member_replace action proving the replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_action_id: Option<AgentActionId>,
    pub state: TransitionState,
    pub prepared_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<String>,
    /// Members the notices went to (finalized changes only).
    #[serde(default)]
    pub recipients: Vec<CutexSessionId>,
    /// The core delegation handover (`handover_project`) ran for this change.
    #[serde(default)]
    pub handover_dispatched: bool,
    /// `cutex/project/directorChanged` was appended to the event stream.
    #[serde(default)]
    pub event_emitted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeState {
    Pending,
    Delivered,
    /// A later change for the same recipient replaced it before delivery; the
    /// later notice carries the whole chain.
    Superseded,
}

/// One passive Director-change notice (§3): no model wake by default.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoticeRecord {
    pub key: String,
    pub change_id: String,
    pub project_id: ProjectId,
    pub recipient: CutexSessionId,
    pub old_director: Option<CutexSessionId>,
    pub new_director: CutexSessionId,
    pub new_epoch: u64,
    pub reason: ChangeReason,
    /// Earlier undelivered changes this notice summarises (A -> B -> C).
    #[serde(default)]
    pub chain: Vec<String>,
    pub passive: bool,
    pub wake: bool,
    pub state: NoticeState,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StartReviewState {
    /// Waiting for the current Director / owner.
    Open,
    /// An adoption receipt exists (see `adoption_id`).
    Adopted,
    Discarded,
    /// The source may have reached the native inbox: view / reconcile only,
    /// never resend.
    DeliveryUnknown,
}

/// A quarantined Management start whose ONLY defect is that the project
/// Director changed (§5). The source message stays quarantined.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReviewItem {
    pub review_id: String,
    pub project_id: ProjectId,
    pub source_action_id: AgentActionId,
    pub source_message_id: String,
    pub source_operation: AgentOperationKind,
    pub original_requester: CutexSessionId,
    pub original_director: CutexSessionId,
    pub target_session: CutexSessionId,
    pub content_sha256: String,
    /// Director when the review was opened (display / audit only).
    pub director_at_review: CutexSessionId,
    pub epoch_at_review: u64,
    /// CAS token for approve / discard; bumped on every state change.
    pub revision: u64,
    pub state: StartReviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_by: Option<CutexSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_by_owner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdoptionState {
    /// Approved; the fresh message may be queued.
    Approved,
    /// Queued to the bus with the fresh message id.
    Submitted,
    /// The native submission outcome is unknown: never resent blindly.
    SubmissionUnknown,
    Delivered,
    /// The authority moved before admission: the review was reopened.
    Stale,
}

/// Provider-owned proof that the current Director (or the owner) adopted a
/// quarantined start (§5 action 1). The original action / caller / metadata
/// are not changed; the fresh message references them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptionReceipt {
    pub adoption_id: String,
    pub approve_action_id: String,
    pub request_sha256: String,
    pub review_id: String,
    pub project_id: ProjectId,
    pub source_action_id: AgentActionId,
    pub source_message_id: String,
    pub content_sha256: String,
    pub target_session: CutexSessionId,
    pub approved_by: CutexSessionId,
    pub approved_by_owner: bool,
    /// Authority the approval was made under (the admission fence).
    pub approved_director: CutexSessionId,
    pub approved_epoch: u64,
    pub message_id: String,
    pub state: AdoptionState,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------- store seam

/// The core extension store (`director_address::Store`) implements this over
/// its management section: one locked, atomically written read-modify-write.
/// Lock order: the Agent Management roster lock (when held) comes first.
pub trait ExtensionStateStore: Send + Sync {
    fn read(&self) -> anyhow::Result<ManagementExtension>;
    /// Runs `update` on the current state and persists it when it returns
    /// `Ok(true)`; `Ok(false)` writes nothing.
    fn update(
        &self,
        update: &mut dyn FnMut(&mut ManagementExtension) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()>;
}

fn default_store_slot() -> &'static Mutex<Option<Arc<dyn ExtensionStateStore>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<dyn ExtensionStateStore>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Process-wide store, installed by core at startup (daemon and root CLI).
/// Without it the provider binds exactly as r253 (no hook).
pub fn set_default_store(store: Arc<dyn ExtensionStateStore>) {
    *default_store_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(store);
}

pub fn default_store() -> Option<Arc<dyn ExtensionStateStore>> {
    default_store_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The override store when set, else the default management file.
pub fn default_store_or_file() -> anyhow::Result<Arc<dyn ExtensionStateStore>> {
    match default_store() {
        Some(store) => Ok(store),
        None => Ok(Arc::new(ManagementStore::open_default()?)),
    }
}

pub fn read_default() -> anyhow::Result<ManagementExtension> {
    default_store_or_file()?.read()
}

/// The hook `AgentManagementProvider::open_default` installs: the override
/// store when one is set, else `<runtime>/director-address`.
pub fn default_authority_hook() -> Option<Arc<dyn AuthorityTransitionHook>> {
    let store = match default_store() {
        Some(store) => store,
        None => match ManagementStore::open_default() {
            Ok(store) => Arc::new(store) as Arc<dyn ExtensionStateStore>,
            Err(error) => {
                eprintln!("director-address: no runtime dir, authority hook disabled: {error:#}");
                return None;
            }
        },
    };
    Some(Arc::new(ManagementHook::new(store)) as Arc<dyn AuthorityTransitionHook>)
}

/// In-memory store (tests; also a seam for the core store's own tests).
#[derive(Default)]
pub struct MemoryExtensionStore(Mutex<ManagementExtension>);

impl ExtensionStateStore for MemoryExtensionStore {
    fn read(&self) -> anyhow::Result<ManagementExtension> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
    fn update(
        &self,
        update: &mut dyn FnMut(&mut ManagementExtension) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        if update(&mut next)? {
            *guard = next;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256Digest::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

pub fn content_sha256(content: &str) -> String {
    format!("{:x}", Sha256Digest::digest(content.as_bytes()))
}

pub fn change_id(project: &ProjectId, authority_action: &AgentActionId) -> String {
    format!("dchg-{}", &digest(&["change", project.as_str(), authority_action.as_str()])[..24])
}

pub fn notice_key(project: &ProjectId, new_epoch: u64, recipient: &CutexSessionId) -> String {
    format!("{}:{new_epoch}:{}", project.as_str(), recipient.as_str())
}

pub fn review_id(source_action: &AgentActionId, source_message: &str) -> String {
    format!("dsr-{}", &digest(&["start-review", source_action.as_str(), source_message])[..24])
}

fn store_error(error: anyhow::Error) -> AgentManagementError {
    eprintln!("director-address store: {error:#}");
    AgentManagementError::PersistenceUnavailable
}

/// The completed `member_replace` whose predecessor is `old` and successor is
/// `new` in `project`, with `old` retired. Replacement alone never moves the
/// authority; it is only the evidence for the reason / default policy.
pub fn trusted_replacement(
    snapshot: &AgentManagementSnapshot,
    project: &ProjectId,
    old: &CutexSessionId,
    new: &CutexSessionId,
) -> Option<AgentActionId> {
    let old_retired = snapshot.agents.get(old).is_some_and(|a| a.retired_at.is_some());
    let new_active = snapshot.agents.get(new).is_some_and(|a| a.retired_at.is_none());
    if !old_retired || !new_active {
        return None;
    }
    let mut found: Vec<&AgentActionId> = snapshot
        .phase_events
        .values()
        .filter(|e| {
            e.operation == AgentOperationKind::Replace
                && e.phase == AgentActionPhase::Complete
                && &e.project_id == project
                && e.predecessor_cutex_session_id.as_ref() == Some(old)
                && e.successor_cutex_session_id.as_ref() == Some(new)
        })
        .map(|e| &e.action_id)
        .collect();
    found.sort();
    found.dedup();
    // Two different replace actions claiming the same pair is not evidence.
    (found.len() == 1).then(|| found[0].clone())
}

/// Active current members of `project` (retired / other-project agents and
/// external grantees excluded), plus `extra` (the new Director).
pub fn current_members(
    snapshot: &AgentManagementSnapshot,
    project: &ProjectId,
) -> Vec<CutexSessionId> {
    snapshot
        .agents
        .values()
        .filter(|a| a.retired_at.is_none() && current_project_id(snapshot, a).as_ref() == Some(project))
        .map(|a| a.cutex_session_id.clone())
        .collect()
}

fn classify(
    intent: &AuthorityTransitionIntent,
    snapshot: &AgentManagementSnapshot,
) -> (ChangeReason, HandoverPolicy, Option<AgentActionId>) {
    let Some(next) = &intent.next else {
        return (ChangeReason::Cleared, HandoverPolicy::OwnerApprovedScopeOnly, None);
    };
    match &intent.previous {
        // A seat filled again after a clear: members learn of it; the cleared
        // Director's work already went to the owner with the clear.
        None if next.authority_epoch > 1 => (ChangeReason::OwnerAssigned, HandoverPolicy::None, None),
        None => (ChangeReason::Initial, HandoverPolicy::None, None),
        Some(prev) if prev.authorized_director_session == next.authorized_director_session => {
            (ChangeReason::EpochRefresh, HandoverPolicy::None, None)
        }
        Some(prev) => match trusted_replacement(
            snapshot,
            &intent.project_id,
            &prev.authorized_director_session,
            &next.authorized_director_session,
        ) {
            Some(action) => (ChangeReason::ReplacementConfirmed, HandoverPolicy::AutoProjectStamped, Some(action)),
            None => (ChangeReason::OwnerAssigned, HandoverPolicy::Review, None),
        },
    }
}

// ---------------------------------------------------------------- transitions

/// §6.1: records the prepared intent. Idempotent per authority action; a
/// different payload under the same action is refused (the provider refuses
/// it too, before this is reached).
pub fn prepare_transition(
    ext: &mut ManagementExtension,
    intent: &AuthorityTransitionIntent,
    snapshot: &AgentManagementSnapshot,
) -> Result<bool, AgentManagementError> {
    let id = change_id(&intent.project_id, &intent.action_id);
    if let Some(existing) = ext.transitions.get(&id) {
        return if existing.request_sha256.as_deref() == Some(intent.request_sha256.as_str()) {
            Ok(false)
        } else {
            Err(AgentManagementError::Conflict("director_change_payload_conflict"))
        };
    }
    let (reason, handover, replacement) = classify(intent, snapshot);
    ext.transitions.insert(
        id.clone(),
        TransitionRecord {
            change_id: id,
            project_id: intent.project_id.clone(),
            authority_action_id: Some(intent.action_id.clone()),
            request_sha256: Some(intent.request_sha256.as_str().to_string()),
            old_director: intent.previous.as_ref().map(|p| p.authorized_director_session.clone()),
            new_director: intent.next.as_ref().map(|n| n.authorized_director_session.clone()),
            old_epoch: intent.previous.as_ref().map(|p| p.authority_epoch),
            // A clear keeps the cleared epoch (the next assignment continues it).
            new_epoch: intent
                .next
                .as_ref()
                .or(intent.previous.as_ref())
                .map_or(0, |a| a.authority_epoch),
            reason,
            handover,
            replacement_action_id: replacement,
            state: TransitionState::Prepared,
            prepared_at: now(),
            settled_at: None,
            recipients: Vec::new(),
            handover_dispatched: false,
            event_emitted: false,
        },
    );
    Ok(true)
}

/// §6.2 / §6.3: settles the transition the receipt proves. Returns whether
/// anything changed. The receipt must match the prepared payload exactly.
pub fn finalize_transition(
    ext: &mut ManagementExtension,
    receipt: &ProjectAuthorityReceipt,
    snapshot: &AgentManagementSnapshot,
) -> Result<bool, AgentManagementError> {
    let project = &receipt.authority.project_id;
    let id = change_id(project, &receipt.action_id);
    let Some(record) = ext.transitions.get(&id).cloned() else {
        // Bound before the hook existed or without it: recovery accounts it.
        return Ok(false);
    };
    if record.request_sha256.as_deref() != Some(receipt.request_sha256.as_str())
        || record.new_epoch != receipt.authority.authority_epoch
        || record.new_director.as_ref() != Some(&receipt.authority.authorized_director_session)
    {
        return Err(AgentManagementError::Conflict("director_change_receipt_mismatch"));
    }
    settle_transition(ext, id, record, snapshot)
}

/// Finalize of an owner project mutation (`assign_director` /
/// `rotate_director` / `clear_director` / create with a Director): the
/// commit witness is the durable human-management mutation record of the
/// same action with the prepared digest. The prepared Director / epoch were
/// computed in the same roster transaction that wrote that record.
pub fn finalize_owner_transition(
    ext: &mut ManagementExtension,
    action_id: &AgentActionId,
    snapshot: &AgentManagementSnapshot,
) -> Result<bool, AgentManagementError> {
    let Some(record) = ext
        .transitions
        .values()
        .find(|t| t.authority_action_id.as_ref() == Some(action_id))
        .cloned()
    else {
        return Ok(false);
    };
    let Some(witness) = snapshot.human_management_project_mutations.get(action_id) else {
        // Not committed (yet): recovery aborts it if it never is.
        return Ok(false);
    };
    if record.request_sha256.as_deref() != Some(witness.request_sha256.as_str())
        || witness.receipt.project_id != record.project_id
    {
        return Err(AgentManagementError::Conflict("director_change_receipt_mismatch"));
    }
    let id = record.change_id.clone();
    settle_transition(ext, id, record, snapshot)
}

fn settle_transition(
    ext: &mut ManagementExtension,
    id: String,
    record: TransitionRecord,
    snapshot: &AgentManagementSnapshot,
) -> Result<bool, AgentManagementError> {
    let project = &record.project_id.clone();
    match record.state {
        TransitionState::Finalized | TransitionState::Superseded => return Ok(false),
        // A receipt exists after all (recovery ran before the persist was
        // visible): the receipt wins.
        TransitionState::Prepared | TransitionState::Aborted => {}
    }
    let current = snapshot.projects.get(project);
    let still_current = match &record.new_director {
        Some(director) => current
            .is_some_and(|a| a.authority_epoch == record.new_epoch && &a.authorized_director_session == director),
        None => current.is_none(),
    };
    let mut record = record;
    record.settled_at = Some(now());
    account_epoch(ext, project, record.new_epoch);
    if !still_current {
        record.state = TransitionState::Superseded;
        ext.transitions.insert(id, record);
        return Ok(true);
    }
    record.state = TransitionState::Finalized;
    if matches!(record.reason, ChangeReason::OwnerAssigned | ChangeReason::ReplacementConfirmed) {
        if let Some(director) = record.new_director.clone() {
            record.recipients = queue_notices(ext, &record, &director, snapshot);
        }
    }
    ext.transitions.insert(id, record);
    Ok(true)
}

fn account_epoch(ext: &mut ManagementExtension, project: &ProjectId, epoch: u64) {
    let slot = ext.accounted_epochs.entry(project.clone()).or_insert(0);
    *slot = (*slot).max(epoch);
}

/// §3: one passive notice per active member (and the new Director); the old
/// Director only when it is still active. Dedup `(project, newEpoch,
/// recipient)`; a pending notice of an earlier change to the same recipient
/// is superseded and listed in the new notice's chain.
fn queue_notices(
    ext: &mut ManagementExtension,
    record: &TransitionRecord,
    new_director: &CutexSessionId,
    snapshot: &AgentManagementSnapshot,
) -> Vec<CutexSessionId> {
    let mut recipients: BTreeSet<CutexSessionId> = current_members(snapshot, &record.project_id).into_iter().collect();
    if snapshot.agents.get(new_director).map_or(true, |a| a.retired_at.is_none()) {
        recipients.insert(new_director.clone());
    }
    for recipient in &recipients {
        let key = notice_key(&record.project_id, record.new_epoch, recipient);
        if ext.notices.contains_key(&key) {
            continue;
        }
        let mut chain = Vec::new();
        for earlier in ext.notices.values_mut() {
            if earlier.project_id == record.project_id
                && &earlier.recipient == recipient
                && earlier.state == NoticeState::Pending
                && earlier.new_epoch < record.new_epoch
            {
                earlier.state = NoticeState::Superseded;
                chain.extend(earlier.chain.iter().cloned());
                chain.push(earlier.change_id.clone());
            } else if earlier.project_id == record.project_id
                && &earlier.recipient == recipient
                && earlier.state == NoticeState::Delivered
                && Some(earlier.new_epoch) == record.old_epoch
            {
                // Queued but maybe unread: its frozen text names the previous
                // Director, so admission now suppresses it and this notice
                // carries the step instead.
                chain.extend(earlier.chain.iter().cloned());
                chain.push(earlier.change_id.clone());
            }
        }
        ext.notices.insert(
            key.clone(),
            NoticeRecord {
                key,
                change_id: record.change_id.clone(),
                project_id: record.project_id.clone(),
                recipient: recipient.clone(),
                old_director: record.old_director.clone(),
                new_director: new_director.clone(),
                new_epoch: record.new_epoch,
                reason: record.reason,
                chain,
                passive: true,
                wake: false,
                state: NoticeState::Pending,
                created_at: now(),
            },
        );
    }
    recipients.into_iter().collect()
}

/// Startup / periodic recovery under the provider lock (§6.3 / §6.4):
/// prepared intents are settled from the real receipts, and a roster epoch
/// above the accounted one without a record becomes a `Recovery` review item
/// (no chain invented, no notice). The very first run only sets the baseline.
pub fn recover(ext: &mut ManagementExtension, snapshot: &AgentManagementSnapshot) -> Result<bool, AgentManagementError> {
    let mut changed = false;
    let first_open = ext.accounted_epochs.is_empty() && ext.transitions.is_empty();
    let prepared: Vec<TransitionRecord> = ext
        .transitions
        .values()
        .filter(|t| t.state == TransitionState::Prepared)
        .cloned()
        .collect();
    for record in prepared {
        let action = record.authority_action_id.as_ref();
        let receipt = action.and_then(|action| snapshot.authority_receipts.get(action));
        let owner_witness = action.filter(|action| snapshot.human_management_project_mutations.contains_key(*action));
        match (receipt, owner_witness) {
            (Some(receipt), _) => changed |= finalize_transition(ext, receipt, snapshot)?,
            (None, Some(action)) => changed |= finalize_owner_transition(ext, &action.clone(), snapshot)?,
            (None, None) => {
                let mut record = record;
                record.state = TransitionState::Aborted;
                record.settled_at = Some(now());
                ext.transitions.insert(record.change_id.clone(), record);
                changed = true;
            }
        }
    }
    for (project, authority) in &snapshot.projects {
        let accounted = ext.accounted_epochs.get(project).copied();
        if first_open || accounted.is_none() && !ext.transitions.values().any(|t| &t.project_id == project) {
            if accounted != Some(authority.authority_epoch) {
                account_epoch(ext, project, authority.authority_epoch);
                changed = true;
            }
            continue;
        }
        let covered = ext
            .transitions
            .values()
            .any(|t| &t.project_id == project && t.new_epoch == authority.authority_epoch);
        if accounted.unwrap_or(0) < authority.authority_epoch && !covered {
            changed |= record_bypassed(ext, authority);
        }
    }
    Ok(changed)
}

fn record_bypassed(ext: &mut ManagementExtension, authority: &ProjectAuthority) -> bool {
    let id = format!(
        "dchg-{}",
        &digest(&["recovery", authority.project_id.as_str(), &authority.authority_epoch.to_string()])[..24]
    );
    if ext.transitions.contains_key(&id) {
        return false;
    }
    let previous = ext
        .transitions
        .values()
        .filter(|t| t.project_id == authority.project_id && t.state == TransitionState::Finalized)
        // A clear keeps the epoch of the seating it ends, so on equal epochs
        // the clear is the later change; then settle time and change id make
        // the choice independent of map order.
        .max_by_key(|t| (t.new_epoch, t.new_director.is_none(), t.settled_at.clone(), t.change_id.clone()))
        .map(|t| (t.new_director.clone(), t.new_epoch));
    let same = previous
        .as_ref()
        .is_some_and(|(d, _)| d.as_ref() == Some(&authority.authorized_director_session));
    ext.transitions.insert(
        id.clone(),
        TransitionRecord {
            change_id: id,
            project_id: authority.project_id.clone(),
            authority_action_id: None,
            request_sha256: None,
            // Unknown when the previous change is not on record (desktop-made
            // project, older binary): the owner decides, never the successor.
            old_director: previous.as_ref().and_then(|p| p.0.clone()),
            new_director: Some(authority.authorized_director_session.clone()),
            old_epoch: previous.as_ref().map(|p| p.1),
            new_epoch: authority.authority_epoch,
            reason: if same { ChangeReason::EpochRefresh } else { ChangeReason::Recovery },
            handover: if same { HandoverPolicy::None } else { HandoverPolicy::OwnerApprovedScopeOnly },
            replacement_action_id: None,
            state: TransitionState::Finalized,
            prepared_at: now(),
            settled_at: Some(now()),
            recipients: Vec::new(),
            handover_dispatched: false,
            event_emitted: false,
        },
    );
    account_epoch(ext, &authority.project_id, authority.authority_epoch);
    true
}

/// Core delegation handover for one finalized change:
/// `(project, from, to, trusted_retired, change_id) -> moved ask ids`.
/// Must be idempotent per change id (it is retried after a crash).
pub type HandoverFn = Arc<dyn Fn(&str, &str, &str, bool, &str) -> Result<Vec<String>, String> + Send + Sync>;

/// Owner-only scope recovery for a change without trustworthy provenance
/// (`OwnerApprovedScopeOnly`: recovered / bypassed change, cleared seat):
/// `(project, previous Director if known, current Director if any,
/// change_id) -> candidate ids`. The successor can never self-approve
/// these; idempotent per change id.
pub type RecoverFn =
    Arc<dyn Fn(&str, Option<&str>, Option<&str>, &str) -> Result<Vec<String>, String> + Send + Sync>;

fn core_recover() -> RecoverFn {
    Arc::new(|project, previous, current, change| {
        #[cfg(not(test))]
        return super::recover_project(project, previous, current, change).map_err(|e| format!("{e:?}"));
        // Unit tests never write the shared ask store implicitly.
        #[cfg(test)]
        {
            let _ = (project, previous, current, change);
            Err("owner scope recovery not installed in unit tests".to_string())
        }
    })
}

fn core_handover() -> HandoverFn {
    Arc::new(|project, from, to, trusted, change| {
        super::handover_project(project, from, to, trusted, change).map_err(|e| format!("{e:?}"))
    })
}

/// Appends the UI event of one finalized change (once per change id).
pub type ChangeEventFn = Arc<dyn Fn(&TransitionRecord) -> anyhow::Result<()> + Send + Sync>;

/// Method of the project Director-change event (design 044 §7). Old
/// desktops ignore it; params are [`director_changed_params`].
pub const DIRECTOR_CHANGED_METHOD: &str = "cutex/project/directorChanged";

pub fn director_changed_params(t: &TransitionRecord) -> serde_json::Value {
    let project_service_count = crate::services::store::ServiceStore::open_default()
        .ok()
        .and_then(|store| store.load().ok())
        .map(|registry| registry.services.values().filter(|service| {
            service.owner_scope == crate::services::store::ServiceOwnerScope::Project
                && service.project_id.as_deref() == Some(t.project_id.as_str())
        }).count());
    serde_json::json!({
        "schema": "cutex/project-director-changed/v1",
        "changeId": t.change_id,
        "projectId": t.project_id,
        "projectServiceCount": project_service_count,
        "oldDirector": t.old_director,
        "newDirector": t.new_director,
        "cleared": t.new_director.is_none(),
        "oldEpoch": t.old_epoch,
        "newEpoch": t.new_epoch,
        "reason": t.reason,
        "handover": t.handover,
        "replacementActionId": t.replacement_action_id,
        "authorityActionId": t.authority_action_id,
        "at": t.settled_at,
    })
}

static CHANGE_EVENT_SINK: OnceLock<ChangeEventFn> = OnceLock::new();

/// Overrides the owner-scoped event append (default:
/// `integration_events::append_project_director_changed`, keyed by change
/// id). `eventEmitted` is only set after a successful append.
pub fn set_change_event_sink(sink: ChangeEventFn) {
    let _ = CHANGE_EVENT_SINK.set(sink);
}

fn core_event() -> ChangeEventFn {
    Arc::new(|t| match CHANGE_EVENT_SINK.get() {
        Some(sink) => sink(t),
        #[cfg(not(test))]
        None => crate::management::v2::integration_events::append_project_director_changed(
            event_owner(t).as_str(),
            &t.change_id,
            director_changed_params(t),
        ),
        // Unit tests never write the shared event stream implicitly.
        #[cfg(test)]
        None => anyhow::bail!("project Director change event sink not installed"),
    })
}

/// The provider hook over an [`ExtensionStateStore`].
pub struct ManagementHook {
    store: Arc<dyn ExtensionStateStore>,
    handover: HandoverFn,
    recover_scope: RecoverFn,
    event: ChangeEventFn,
}

/// Stream owner of a change event: the new Director, or the vacating one.
fn event_owner(t: &TransitionRecord) -> CutexSessionId {
    t.new_director
        .clone()
        .or_else(|| t.old_director.clone())
        .expect("a reported change names a Director")
}

/// A seat filled with no prior Director on record (first bind, assignment
/// after a clear): owner-only `recover_project(project, None, current)` so
/// rows of a clear are retargeted and stamped work of an unhooked / unknown
/// predecessor surfaces for the owner. Nothing moves automatically.
fn is_seating(t: &TransitionRecord) -> bool {
    t.reason != ChangeReason::EpochRefresh && t.old_director.is_none() && t.new_director.is_some()
}

/// A Director change worth an event / handover (not a first bind or a
/// same-session epoch refresh).
fn is_director_change(t: &TransitionRecord) -> bool {
    !matches!(t.reason, ChangeReason::Initial | ChangeReason::EpochRefresh)
        && t.old_director != t.new_director
        && (t.old_director.is_some() || t.new_director.is_some())
}

impl ManagementHook {
    pub fn new(store: Arc<dyn ExtensionStateStore>) -> Self {
        Self { store, handover: core_handover(), recover_scope: core_recover(), event: core_event() }
    }

    pub fn with_recover_scope(mut self, recover_scope: RecoverFn) -> Self {
        self.recover_scope = recover_scope;
        self
    }

    pub fn with_event(mut self, event: ChangeEventFn) -> Self {
        self.event = event;
        self
    }

    pub fn with_handover(mut self, handover: HandoverFn) -> Self {
        self.handover = handover;
        self
    }

    pub fn store(&self) -> &Arc<dyn ExtensionStateStore> {
        &self.store
    }

    pub fn recover(&self, snapshot: &AgentManagementSnapshot) -> Result<(), AgentManagementError> {
        let mut failure = None;
        self.store
            .update(&mut |ext| match recover(ext, snapshot) {
                Ok(changed) => Ok(changed),
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            })
            .map_err(store_error)?;
        failure.map_or(Ok(()), Err)?;
        self.dispatch_handovers(snapshot)
    }

    /// Runs the core handover for every finalized, still-current Director
    /// change not yet dispatched, OUTSIDE the management lock, then records
    /// it. A failure leaves it for the next finalize / recovery.
    pub(crate) fn dispatch_handovers(&self, snapshot: &AgentManagementSnapshot) -> Result<(), AgentManagementError> {
        let ext = self.store.read().map_err(store_error)?;
        // Still the seat holder this change installed (compared by session,
        // so a later same-Director epoch refresh does not strand it).
        let seated = |t: &TransitionRecord| match (&t.new_director, snapshot.projects.get(&t.project_id)) {
            (Some(director), Some(current)) => &current.authorized_director_session == director,
            (None, current) => current.is_none(),
            (Some(_), None) => false,
        };
        let due: Vec<TransitionRecord> = ext
            .transitions
            .values()
            .filter(|t| {
                t.state == TransitionState::Finalized
                    && !t.handover_dispatched
                    && ((t.handover != HandoverPolicy::None && is_director_change(t)) || is_seating(t))
            })
            .cloned()
            .collect();
        // A later finalized change out of this change's new Director: the
        // chain is on record and that change's own handover (controller !=
        // current) covers this one's old work.
        let continued = |t: &TransitionRecord| {
            ext.transitions.values().any(|later| {
                later.project_id == t.project_id
                    && later.new_epoch > t.new_epoch
                    && later.state == TransitionState::Finalized
                    && later.old_director == t.new_director
                    && later.old_director.is_some()
            })
        };
        for t in ext.transitions.values().filter(|t| {
            t.state == TransitionState::Finalized && !t.event_emitted && is_director_change(t)
        }) {
            if let Err(error) = (self.event)(t) {
                eprintln!("director-address event for {} deferred: {error:#}", t.change_id);
                continue;
            }
            let id = t.change_id.clone();
            self.store
                .update(&mut |ext| {
                    Ok(match ext.transitions.get_mut(&id) {
                        Some(record) if !record.event_emitted => {
                            record.event_emitted = true;
                            true
                        }
                        _ => false,
                    })
                })
                .map_err(store_error)?;
        }
        for t in due {
            // §6.4: the policy decides who may act. Only a change with a
            // trustworthy old AND new Director goes to the successor's review
            // (or moves automatically when trusted); everything else becomes
            // owner-only candidates the successor cannot approve.
            let current = snapshot.projects.get(&t.project_id).map(|a| a.authorized_director_session.as_str());
            let outcome = match (t.handover, &t.old_director, &t.new_director) {
                // Deferred and the seat moved on: never inherit this change's
                // proof. Covered by the next change on record, else owner-only
                // candidates for the ACTUAL current Director.
                _ if !seated(&t) && continued(&t) => Ok(Vec::new()),
                _ if !seated(&t) => (self.recover_scope)(
                    t.project_id.as_str(),
                    t.old_director.as_ref().map(|d| d.as_str()),
                    current,
                    &t.change_id,
                ),
                (HandoverPolicy::AutoProjectStamped | HandoverPolicy::Review, Some(from), Some(to)) => (self.handover)(
                    t.project_id.as_str(),
                    from.as_str(),
                    to.as_str(),
                    t.handover == HandoverPolicy::AutoProjectStamped,
                    &t.change_id,
                ),
                _ => (self.recover_scope)(
                    t.project_id.as_str(),
                    t.old_director.as_ref().map(|d| d.as_str()),
                    t.new_director.as_ref().map(|d| d.as_str()),
                    &t.change_id,
                ),
            };
            if let Err(error) = outcome {
                eprintln!("director-address handover {} deferred: {error}", t.change_id);
                continue;
            }
            let id = t.change_id.clone();
            self.store
                .update(&mut |ext| {
                    Ok(match ext.transitions.get_mut(&id) {
                        Some(record) if !record.handover_dispatched => {
                            record.handover_dispatched = true;
                            true
                        }
                        _ => false,
                    })
                })
                .map_err(store_error)?;
        }
        Ok(())
    }
}

impl AuthorityTransitionHook for ManagementHook {
    fn recover(&self, snapshot: &AgentManagementSnapshot) -> Result<(), AgentManagementError> {
        ManagementHook::recover(self, snapshot)
    }

    /// Fail closed: without a durable intent the authority never moves. A
    /// sidecar written by a NEWER build (rollback) is left untouched and the
    /// bind is refused with an explicit unsupported-version error (this
    /// build cannot preserve that build's evidence).
    fn prepare(
        &self,
        intent: &AuthorityTransitionIntent,
        snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError> {
        let mut failure = None;
        let written = self.store.update(&mut |ext| match prepare_transition(ext, intent, snapshot) {
            Ok(changed) => Ok(changed),
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        });
        match written {
            Err(error) if is_newer_version(&error) => {
                return Err(AgentManagementError::ConflictDetail(format!(
                    "director_address_store_unsupported_version: {error:#}; run the newer cutex build (Director changes are refused until then, nothing was changed)"
                )));
            }
            other => other.map_err(store_error)?,
        }
        failure.map_or(Ok(()), Err)
    }

    fn finalize_owner_change(
        &self,
        action_id: &AgentActionId,
        snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError> {
        let mut failure = None;
        self.store
            .update(&mut |ext| match finalize_owner_transition(ext, action_id, snapshot) {
                Ok(changed) => Ok(changed),
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            })
            .map_err(store_error)?;
        failure.map_or(Ok(()), Err)?;
        self.dispatch_handovers(snapshot)
    }

    fn finalize(
        &self,
        receipt: &ProjectAuthorityReceipt,
        snapshot: &AgentManagementSnapshot,
    ) -> Result<(), AgentManagementError> {
        let mut failure = None;
        self.store
            .update(&mut |ext| match finalize_transition(ext, receipt, snapshot) {
                Ok(changed) => Ok(changed),
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            })
            .map_err(store_error)?;
        failure.map_or(Ok(()), Err)?;
        self.dispatch_handovers(snapshot)
    }
}

// ---------------------------------------------------------------- notices outbox

/// Pending passive notices for `recipient` (the bus worker sends them through
/// the authenticated cutex service input, passive, then marks them).
pub fn pending_notices(ext: &ManagementExtension, recipient: &CutexSessionId) -> Vec<NoticeRecord> {
    ext.notices
        .values()
        .filter(|n| &n.recipient == recipient && n.state == NoticeState::Pending)
        .cloned()
        .collect()
}

/// Whether a pending notice is still deliverable: the recipient is an active
/// member of the project now (a member who left is not told). The text is
/// built from the CURRENT authority so an A -> B -> C catch-up never says
/// "now B" after C took office.
pub fn notice_text(notice: &NoticeRecord, snapshot: &AgentManagementSnapshot) -> Option<String> {
    let current = snapshot.projects.get(&notice.project_id)?;
    // The current Director is told even without a ManagedAgentRecord (a
    // Director bound directly by the owner), unless its record is retired.
    let member = match snapshot.agents.get(&notice.recipient) {
        Some(a) => {
            a.retired_at.is_none()
                && (current_project_id(snapshot, a).as_ref() == Some(&notice.project_id)
                    || a.cutex_session_id == current.authorized_director_session)
        }
        None => notice.recipient == current.authorized_director_session,
    };
    if !member {
        return None;
    }
    let name = |id: &CutexSessionId| {
        snapshot
            .agents
            .get(id)
            .map(|a| format!("{} ({})", a.spec.name, crate::short_ids::display(id.as_str())))
            .unwrap_or_else(|| crate::short_ids::display(id.as_str()).to_string())
    };
    let reason = match notice.reason {
        ChangeReason::ReplacementConfirmed => "replacement confirmed",
        ChangeReason::OwnerAssigned => "assigned by the owner",
        ChangeReason::Recovery => "recovered change",
        ChangeReason::Cleared => "seat cleared",
        ChangeReason::Initial | ChangeReason::EpochRefresh => "authority refresh",
    };
    let before = notice.old_director.as_ref().map(name).unwrap_or_else(|| "none".into());
    let mut text = format!(
        "The Director of project {} changed: {} -> {} ({reason}; epoch {}).",
        notice.project_id.as_str(),
        before,
        name(&current.authorized_director_session),
        current.authority_epoch,
    );
    // A later same-Director epoch refresh is no Director change.
    let moved_on = current.authorized_director_session != notice.new_director;
    if moved_on || !notice.chain.is_empty() {
        text.push_str(&format!(" {} earlier change(s) are included in this summary.", notice.chain.len() + usize::from(moved_on)));
    }
    text.push_str(" Address the Director as `director`; nothing to do unless you have open project work. This is information, not a task.");
    Some(text)
}

pub fn mark_notice(ext: &mut ManagementExtension, key: &str, state: NoticeState) -> bool {
    match ext.notices.get_mut(key) {
        Some(notice) if notice.state == NoticeState::Pending && state != NoticeState::Pending => {
            notice.state = state;
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- start review / adoption

/// What made an unreviewed start invalid, when the only defect is the
/// Director: everything else (operation, phase, successor, caller) matched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectorChangedStart {
    pub project_id: ProjectId,
    pub source_action_id: AgentActionId,
    pub source_operation: AgentOperationKind,
    pub original_requester: CutexSessionId,
    pub original_director: CutexSessionId,
    pub target_session: CutexSessionId,
    pub current_director: CutexSessionId,
    pub current_epoch: u64,
}

/// Mirrors `validate_unreviewed_start`'s checks. `Some` only if every check
/// but the Director passes, the project has a known current Director that
/// differs, the metadata's Director provably was the project's Director at
/// an earlier epoch (a finalized change or a replacement chain names it), and
/// the target is still an active member. Anything else stays a plain
/// provenance quarantine.
pub fn classify_director_changed_start(
    metadata: &AgentManagementMessageMetadata,
    owner: &str,
    roster: &AgentManagementSnapshot,
    action: &AgentActionRecord,
    ext: &ManagementExtension,
) -> Option<DirectorChangedStart> {
    use AgentActionPhase as Phase;
    if !matches!(action.operation, AgentOperationKind::Create | AgentOperationKind::Replace) {
        return None;
    }
    if !matches!(
        action.phase,
        Phase::MessagePending
            | Phase::MessageQueued
            | Phase::PredecessorClosing
            | Phase::PredecessorClosed
            | Phase::SuccessorReady
            | Phase::Complete
            | Phase::OwnerActionRequired
    ) {
        return None;
    }
    let target = action.known_successor_cutex_session.as_ref()?;
    if target.as_str() != owner {
        return None;
    }
    let requester = metadata.requested_by_operator.as_ref().unwrap_or(&metadata.requested_by_director);
    if &action.caller_cutex_session != requester {
        return None;
    }
    let current = roster.projects.get(&action.project_id)?;
    if current.authorized_director_session == metadata.requested_by_director {
        return None;
    }
    // Trustworthy old Director: a recorded change away from it, or the
    // action's own caller was the Director of a replacement chain.
    let was_director = ext.transitions.values().any(|t| {
        t.project_id == action.project_id
            && matches!(t.state, TransitionState::Finalized | TransitionState::Superseded)
            && t.old_director.as_ref() == Some(&metadata.requested_by_director)
    }) || trusted_replacement(
        roster,
        &action.project_id,
        &metadata.requested_by_director,
        &current.authorized_director_session,
    )
    .is_some();
    if !was_director {
        return None;
    }
    let agent = roster.agents.get(target)?;
    if agent.retired_at.is_some() || current_project_id(roster, agent).as_ref() != Some(&action.project_id) {
        return None;
    }
    Some(DirectorChangedStart {
        project_id: action.project_id.clone(),
        source_action_id: action.action_id.clone(),
        source_operation: action.operation,
        original_requester: requester.clone(),
        original_director: metadata.requested_by_director.clone(),
        target_session: target.clone(),
        current_director: current.authorized_director_session.clone(),
        current_epoch: current.authority_epoch,
    })
}

/// Publishes the review item for a quarantined start (idempotent; a
/// discarded source never comes back). The source stays quarantined.
pub fn open_start_review(
    ext: &mut ManagementExtension,
    found: &DirectorChangedStart,
    source_message_id: &str,
    content: &str,
) -> bool {
    if ext.discarded_sources.contains(source_message_id) {
        return false;
    }
    let id = review_id(&found.source_action_id, source_message_id);
    if ext.start_reviews.contains_key(&id) {
        return false;
    }
    let at = now();
    ext.start_reviews.insert(
        id.clone(),
        StartReviewItem {
            review_id: id,
            project_id: found.project_id.clone(),
            source_action_id: found.source_action_id.clone(),
            source_message_id: source_message_id.to_string(),
            source_operation: found.source_operation,
            original_requester: found.original_requester.clone(),
            original_director: found.original_director.clone(),
            target_session: found.target_session.clone(),
            content_sha256: content_sha256(content),
            director_at_review: found.current_director.clone(),
            epoch_at_review: found.current_epoch,
            revision: 1,
            state: StartReviewState::Open,
            adoption_id: None,
            settled_by: None,
            settled_by_owner: None,
            settle_reason: None,
            created_at: at.clone(),
            updated_at: at,
        },
    );
    true
}

/// Who acts on a review: the owner (root credential) or an agent session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewActor {
    Owner,
    Session(CutexSessionId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDecision {
    pub action_id: String,
    pub review_id: String,
    pub expected_revision: u64,
    pub expected_epoch: u64,
    pub actor: ReviewActor,
    pub reason: Option<String>,
}

fn decision_digest(decision: &ReviewDecision, verb: &str) -> String {
    let actor = match &decision.actor {
        ReviewActor::Owner => "owner".to_string(),
        ReviewActor::Session(s) => s.as_str().to_string(),
    };
    digest(&[
        verb,
        &decision.review_id,
        &decision.expected_revision.to_string(),
        &decision.expected_epoch.to_string(),
        &actor,
        decision.reason.as_deref().unwrap_or(""),
    ])
}

/// Checks shared by approve / discard: the review revision, the epoch the
/// actor saw, and that the actor is the owner or the CURRENT Director.
fn check_decision<'a>(
    ext: &'a ManagementExtension,
    decision: &ReviewDecision,
    roster: &'a AgentManagementSnapshot,
    states: &[StartReviewState],
) -> Result<(&'a StartReviewItem, &'a ProjectAuthority), &'static str> {
    let item = ext.start_reviews.get(&decision.review_id).ok_or("start_review_not_found")?;
    if !states.contains(&item.state) {
        return Err("start_review_not_open");
    }
    if item.revision != decision.expected_revision {
        return Err("start_review_revision_conflict");
    }
    let authority = roster.projects.get(&item.project_id).ok_or("director_unassigned")?;
    if authority.authority_epoch != decision.expected_epoch {
        return Err("stale_project_authority");
    }
    match &decision.actor {
        ReviewActor::Owner => {}
        ReviewActor::Session(actor) if actor == &authority.authorized_director_session => {}
        ReviewActor::Session(_) => return Err("not_current_director"),
    }
    Ok((item, authority))
}

/// §5 action 1. Only an Open review whose source provably never reached the
/// runtime may be adopted; the caller passes that evidence
/// (`source_delivery_unknown`: the quarantine / native receipt says the
/// submission outcome is unknown -> the review becomes DeliveryUnknown and
/// nothing is approved). Idempotent per `action_id`.
pub fn approve_start_review(
    ext: &mut ManagementExtension,
    decision: &ReviewDecision,
    roster: &AgentManagementSnapshot,
    source_delivery_unknown: bool,
) -> Result<AdoptionReceipt, &'static str> {
    let adoption_id = format!("dsa-{}", &digest(&["adoption", &decision.action_id])[..24]);
    let request = decision_digest(decision, "approve");
    if let Some(existing) = ext.adoptions.get(&adoption_id) {
        return if existing.request_sha256 == request { Ok(existing.clone()) } else { Err("action_id_payload_conflict") };
    }
    let (item, authority) = check_decision(ext, decision, roster, &[StartReviewState::Open])?;
    let (item, authority) = (item.clone(), authority.clone());
    if source_delivery_unknown {
        let review = ext.start_reviews.get_mut(&item.review_id).expect("checked");
        review.state = StartReviewState::DeliveryUnknown;
        review.revision += 1;
        review.updated_at = now();
        return Err("source_delivery_unknown");
    }
    // The target must still be the source action's successor and an active
    // member of the same project.
    let action = roster.actions.get(&item.source_action_id).ok_or("source_action_absent")?;
    if action.known_successor_cutex_session.as_ref() != Some(&item.target_session)
        || action.external_message_id.as_deref() != Some(item.source_message_id.as_str())
    {
        return Err("source_action_changed");
    }
    let agent = roster.agents.get(&item.target_session).ok_or("target_absent")?;
    if agent.retired_at.is_some() || current_project_id(roster, agent).as_ref() != Some(&item.project_id) {
        return Err("target_not_current_member");
    }
    let (approved_by, approved_by_owner) = match &decision.actor {
        ReviewActor::Owner => (authority.authorized_director_session.clone(), true),
        ReviewActor::Session(actor) => (actor.clone(), false),
    };
    let at = now();
    let receipt = AdoptionReceipt {
        adoption_id: adoption_id.clone(),
        approve_action_id: decision.action_id.clone(),
        request_sha256: request,
        review_id: item.review_id.clone(),
        project_id: item.project_id.clone(),
        source_action_id: item.source_action_id.clone(),
        source_message_id: item.source_message_id.clone(),
        content_sha256: item.content_sha256.clone(),
        target_session: item.target_session.clone(),
        approved_by,
        approved_by_owner,
        approved_director: authority.authorized_director_session.clone(),
        approved_epoch: authority.authority_epoch,
        // Fresh message id; the source message keeps its own.
        message_id: format!("msg-adopted-{}", &digest(&["adopted-message", &adoption_id])[..32]),
        state: AdoptionState::Approved,
        created_at: at.clone(),
        updated_at: at.clone(),
    };
    let review = ext.start_reviews.get_mut(&item.review_id).expect("checked");
    review.state = StartReviewState::Adopted;
    review.adoption_id = Some(adoption_id.clone());
    review.settled_by = Some(receipt.approved_by.clone());
    review.settled_by_owner = Some(approved_by_owner);
    review.settle_reason = decision.reason.clone();
    review.revision += 1;
    review.updated_at = at;
    ext.adoptions.insert(adoption_id, receipt.clone());
    Ok(receipt)
}

/// §5 action 2: ends the review only (no member / delegation / history is
/// touched); the source never re-enters through a later scan. Replay of the
/// same decision returns the settled item.
pub fn discard_start_review(
    ext: &mut ManagementExtension,
    decision: &ReviewDecision,
    roster: &AgentManagementSnapshot,
) -> Result<StartReviewItem, &'static str> {
    let replay_key = decision_digest(decision, "discard");
    if let Some(item) = ext.start_reviews.get(&decision.review_id) {
        if item.state == StartReviewState::Discarded && item.settle_reason.as_deref() == Some(replay_key.as_str()) {
            return Ok(item.clone());
        }
    }
    let actor = match &decision.actor {
        ReviewActor::Owner => None,
        ReviewActor::Session(s) => Some(s.clone()),
    };
    let closable = [StartReviewState::Open, StartReviewState::DeliveryUnknown];
    let project = ext.start_reviews.get(&decision.review_id).map(|r| r.project_id.clone());
    let vacant = project.as_ref().is_some_and(|p| !roster.projects.contains_key(p));
    let (id, source, settled_by) = if vacant {
        // Cleared seat: only the owner may close it, fenced by the cleared
        // epoch and the revision; no Director is invented as the actor.
        let item = ext.start_reviews.get(&decision.review_id).expect("found");
        if actor.is_some() {
            return Err("director_unassigned");
        }
        if !closable.contains(&item.state) {
            return Err("start_review_not_open");
        }
        if item.revision != decision.expected_revision {
            return Err("start_review_revision_conflict");
        }
        let cleared_epoch = roster.project_states.get(&item.project_id).and_then(|s| s.last_director_epoch);
        if cleared_epoch != Some(decision.expected_epoch) {
            return Err("stale_project_authority");
        }
        (item.review_id.clone(), item.source_message_id.clone(), None)
    } else {
        // An unknown delivery can still be closed (view / reconcile only,
        // never resent), with the same epoch / revision / current-actor checks.
        let (item, authority) = check_decision(ext, decision, roster, &closable)?;
        let director = authority.authorized_director_session.clone();
        (item.review_id.clone(), item.source_message_id.clone(), Some(actor.clone().unwrap_or(director)))
    };
    let review = ext.start_reviews.get_mut(&id).expect("checked");
    review.state = StartReviewState::Discarded;
    review.settled_by_owner = Some(actor.is_none());
    review.settled_by = settled_by;
    review.settle_reason = Some(replay_key);
    review.revision += 1;
    review.updated_at = now();
    let out = review.clone();
    ext.discarded_sources.insert(source);
    Ok(out)
}

/// Records the bus outcome of an adopted message. `SubmissionUnknown` never
/// permits a resend: only reconciliation by the same message id moves it.
pub fn mark_adoption(ext: &mut ManagementExtension, adoption_id: &str, state: AdoptionState) -> Result<bool, &'static str> {
    use AdoptionState as S;
    let receipt = ext.adoptions.get_mut(adoption_id).ok_or("adoption_not_found")?;
    if receipt.state == state {
        return Ok(false);
    }
    let allowed = matches!(
        (receipt.state, state),
        (S::Approved, S::Submitted | S::SubmissionUnknown | S::Stale)
            | (S::Submitted, S::Delivered | S::SubmissionUnknown)
            | (S::SubmissionUnknown, S::Delivered | S::Submitted)
    );
    if !allowed {
        return Err("adoption_state_conflict");
    }
    receipt.state = state;
    receipt.updated_at = now();
    let review_id = receipt.review_id.clone();
    if state == S::Stale {
        // Re-review under the new Director: B's approval is never C's.
        if let Some(review) = ext.start_reviews.get_mut(&review_id) {
            review.state = StartReviewState::Open;
            review.adoption_id = None;
            review.revision += 1;
            review.updated_at = now();
        }
    }
    Ok(true)
}

/// Submitted -> Stale, only for an adopted message the bus provably refused
/// before admission ([`reconcile_adoption`]): reopens the review like any
/// stale approval. Every other caller goes through [`mark_adoption`].
fn mark_adoption_refused_unadmitted(ext: &mut ManagementExtension, adoption_id: &str) -> Result<bool, &'static str> {
    let receipt = ext.adoptions.get(adoption_id).ok_or("adoption_not_found")?;
    if receipt.state != AdoptionState::Submitted {
        return Err("adoption_state_conflict");
    }
    ext.adoptions.get_mut(adoption_id).expect("checked").state = AdoptionState::Approved;
    mark_adoption(ext, adoption_id, AdoptionState::Stale)
}

/// Whether the adopted message may be queued now (once, never after an
/// unknown submission).
pub fn may_submit(receipt: &AdoptionReceipt) -> bool {
    receipt.state == AdoptionState::Approved
}

/// Admission check of an adopted start (bus_bridge_external): provider
/// receipt, same epoch / Director as at approval, target still the source
/// action's active successor in the project, content unchanged.
pub fn validate_adopted_start(
    receipt: &AdoptionReceipt,
    owner: &str,
    message_id: Option<&str>,
    content: &str,
    roster: &AgentManagementSnapshot,
) -> Result<(), &'static str> {
    if !matches!(receipt.state, AdoptionState::Approved | AdoptionState::Submitted | AdoptionState::SubmissionUnknown) {
        return Err("adoption_not_admissible");
    }
    if message_id != Some(receipt.message_id.as_str()) {
        return Err("adoption_message_id_mismatch");
    }
    if receipt.target_session.as_str() != owner {
        return Err("adoption_recipient_conflict");
    }
    if content_sha256(content) != receipt.content_sha256 {
        return Err("adoption_content_changed");
    }
    let authority = roster.projects.get(&receipt.project_id).ok_or("director_unassigned")?;
    if authority.authority_epoch != receipt.approved_epoch
        || authority.authorized_director_session != receipt.approved_director
    {
        return Err("adoption_authority_changed");
    }
    let action = roster.actions.get(&receipt.source_action_id).ok_or("source_action_absent")?;
    if action.known_successor_cutex_session.as_ref() != Some(&receipt.target_session)
        || action.external_message_id.as_deref() != Some(receipt.source_message_id.as_str())
    {
        return Err("source_action_changed");
    }
    let agent = roster.agents.get(&receipt.target_session).ok_or("target_absent")?;
    if agent.retired_at.is_some() || current_project_id(roster, agent).as_ref() != Some(&receipt.project_id) {
        return Err("target_not_current_member");
    }
    Ok(())
}

/// Metadata + model text of an adopted start: the CURRENT Director is the
/// requester; the source action / message are referenced, not rewritten.
pub fn adopted_start_payload(receipt: &AdoptionReceipt, content: &str) -> (AgentManagementMessageMetadata, String) {
    let metadata = AgentManagementMessageMetadata {
        schema: crate::agent_management::AgentManagementSchema::V1,
        requested_by_director: receipt.approved_director.clone(),
        requested_by_operator: None,
    };
    let approver = if receipt.approved_by_owner {
        "the owner".to_string()
    } else {
        crate::short_ids::display(receipt.approved_by.as_str()).to_string()
    };
    let text = format!(
        "Requested by Director: {} (adopted by {approver} from start message {} of action {} after a Director change)\nAction: follow the explicit start instructions.\nInstructions:\n{content}",
        crate::short_ids::display(receipt.approved_director.as_str()),
        receipt.source_message_id,
        receipt.source_action_id.as_str(),
    );
    (metadata, text)
}

/// Looks up the adoption receipt of a bus message (by its fresh message id).
pub fn adoption_for_message(ext: &ManagementExtension, message_id: &str) -> Option<AdoptionReceipt> {
    ext.adoptions.values().find(|r| r.message_id == message_id).cloned()
}

// ---------------------------------------------------------------- file store

/// `<runtime>/director-address/management-v1.json`, atomically replaced under
/// `management-v1.lock`. Separate from the core ownership file.
#[derive(Clone, Debug)]
pub struct ManagementStore {
    root: std::path::PathBuf,
}

/// `management-v1.json` was written by a newer build.
#[derive(Debug)]
pub struct NewerVersion(pub u32);

impl std::fmt::Display for NewerVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "management-v1.json version {} is newer than this build", self.0)
    }
}

impl std::error::Error for NewerVersion {}

fn is_newer_version(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NewerVersion>().is_some()
}

impl ManagementStore {
    pub fn at(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("director-address")))
    }
    pub fn path(&self) -> std::path::PathBuf {
        self.root.join("management-v1.json")
    }
    fn load(&self) -> anyhow::Result<ManagementExtension> {
        match std::fs::read(self.path()) {
            Ok(bytes) => {
                let ext: ManagementExtension = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(
                    ext.version <= MANAGEMENT_EXTENSION_VERSION,
                    NewerVersion(ext.version)
                );
                Ok(ext)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(ManagementExtension { version: MANAGEMENT_EXTENSION_VERSION, ..Default::default() })
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl ExtensionStateStore for ManagementStore {
    fn read(&self) -> anyhow::Result<ManagementExtension> {
        self.load()
    }
    fn update(
        &self,
        update: &mut dyn FnMut(&mut ManagementExtension) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        use fs2::FileExt;
        std::fs::create_dir_all(&self.root)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("management-v1.lock"))?;
        lock.lock_exclusive()?;
        let mut state = self.load()?;
        if update(&mut state)? {
            state.version = MANAGEMENT_EXTENSION_VERSION;
            crate::config::atomic::write_private_pretty_json_atomic(&self.path(), &state, "Director address management")?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent_management::{
        AgentActionRecord, AgentManagementPhaseEvent, AgentManagementProvider, AgentManagementReceiptSchema,
        AgentManagementSchema, CurrentProjectMembership, ManagedAgentRecord, ManagedAgentSpec,
        ProjectAuthorityRequest,
    };
    use crate::role_revision::{Rfc3339, Sha256};

    const TS: &str = "2026-10-24T00:00:00Z";

    pub(crate) fn sid(id: &str) -> CutexSessionId {
        CutexSessionId::new(format!("cutex.{id}")).unwrap()
    }
    pub(crate) fn pid(id: &str) -> ProjectId {
        ProjectId::new(id).unwrap()
    }
    pub(crate) fn aid(id: &str) -> AgentActionId {
        AgentActionId::new(id).unwrap()
    }
    fn sha(seed: &str) -> Sha256 {
        Sha256::new(content_sha256(seed)).unwrap()
    }
    fn ts() -> Rfc3339 {
        Rfc3339::new(TS).unwrap()
    }

    fn agent(id: &str, project: Option<&str>, retired: bool) -> ManagedAgentRecord {
        ManagedAgentRecord {
            project_id: project.map(pid),
            created_by_director_session: None,
            created_by_operator_session: None,
            cutex_session_id: sid(id),
            native_session_id: format!("native-{id}"),
            spec: ManagedAgentSpec {
                name: id.into(),
                cwd: "D:\\work".into(),
                profile: Some("p".into()),
                runtime_backend: "host".into(),
                model: "m".into(),
                reasoning: "medium".into(),
                permissions: "default".into(),
                approval_policy: "never".into(),
                sandbox_mode: "workspace-write".into(),
                groups: vec![],
                expose_to_im: false,
                pin: false,
            },
            created_at: ts(),
            retired_at: retired.then(ts),
        }
    }

    /// Project "p1": Directors a (old) / b (new), members m1, m2; m3 in p2.
    fn roster(director: &str, epoch: u64, a_retired: bool) -> AgentManagementSnapshot {
        let mut s = AgentManagementSnapshot::empty();
        for (id, project, retired) in [
            ("a", Some("p1"), a_retired),
            ("b", Some("p1"), false),
            ("c", Some("p1"), false),
            ("m1", Some("p1"), false),
            ("m2", Some("p1"), false),
            ("gone", Some("p1"), true),
            ("m3", Some("p2"), false),
        ] {
            s.agents.insert(sid(id), agent(id, project, retired));
        }
        set_authority(&mut s, director, epoch);
        s
    }

    fn set_authority(s: &mut AgentManagementSnapshot, director: &str, epoch: u64) {
        s.projects.insert(
            pid("p1"),
            ProjectAuthority { project_id: pid("p1"), authorized_director_session: sid(director), authority_epoch: epoch, updated_at: ts() },
        );
    }

    fn add_replace(s: &mut AgentManagementSnapshot, action: &str, old: &str, new: &str) {
        s.phase_events.insert(
            format!("ev-{action}"),
            AgentManagementPhaseEvent {
                event_id: format!("ev-{action}"),
                action_id: aid(action),
                project_id: pid("p1"),
                operation: AgentOperationKind::Replace,
                phase: AgentActionPhase::Complete,
                phase_sequence: 9,
                committed_at: ts(),
                presentation_owner_cutex_session_id: sid(old),
                subject_cutex_session_id: Some(sid(new)),
                subject_agent_name: None,
                predecessor_cutex_session_id: Some(sid(old)),
                successor_cutex_session_id: Some(sid(new)),
                replace_policy: None,
                rotation_mode: None,
                authority_epoch: None,
            },
        );
    }

    fn intent(action: &str, prev: Option<(&str, u64)>, next: (&str, u64)) -> AuthorityTransitionIntent {
        AuthorityTransitionIntent {
            action_id: aid(action),
            request_sha256: sha(action),
            project_id: pid("p1"),
            previous: prev.map(|(d, e)| ProjectAuthority { project_id: pid("p1"), authorized_director_session: sid(d), authority_epoch: e, updated_at: ts() }),
            next: Some(ProjectAuthority { project_id: pid("p1"), authorized_director_session: sid(next.0), authority_epoch: next.1, updated_at: ts() }),
        }
    }

    fn receipt(i: &AuthorityTransitionIntent) -> ProjectAuthorityReceipt {
        ProjectAuthorityReceipt {
            schema: AgentManagementReceiptSchema::V1,
            action_id: i.action_id.clone(),
            request_sha256: i.request_sha256.clone(),
            authority: i.next.clone().expect("a bind names a Director"),
        }
    }

    fn change(ext: &mut ManagementExtension, before: &AgentManagementSnapshot, after: &AgentManagementSnapshot, i: &AuthorityTransitionIntent) -> TransitionRecord {
        assert!(prepare_transition(ext, i, before).unwrap());
        assert_eq!(ext.transitions[&change_id(&pid("p1"), &i.action_id)].state, TransitionState::Prepared);
        finalize_transition(ext, &receipt(i), after).unwrap();
        ext.transitions[&change_id(&pid("p1"), &i.action_id)].clone()
    }

    fn recipients(ext: &ManagementExtension, epoch: u64) -> Vec<String> {
        ext.notices.values().filter(|n| n.new_epoch == epoch).map(|n| n.recipient.as_str().to_string()).collect()
    }

    #[test]
    fn trusted_replacement_auto_hands_over_and_notices_active_members_passively() {
        let mut before = roster("a", 1, true);
        add_replace(&mut before, "rep-1", "a", "b");
        let mut after = before.clone();
        set_authority(&mut after, "b", 2);
        let mut ext = ManagementExtension::default();
        let t = change(&mut ext, &before, &after, &intent("bind-2", Some(("a", 1)), ("b", 2)));
        assert_eq!((t.state, t.reason, t.handover), (TransitionState::Finalized, ChangeReason::ReplacementConfirmed, HandoverPolicy::AutoProjectStamped));
        assert_eq!(t.replacement_action_id, Some(aid("rep-1")));
        // retired old Director, retired member and the other project's member are not told
        assert_eq!(recipients(&ext, 2), vec!["cutex.b", "cutex.c", "cutex.m1", "cutex.m2"]);
        assert!(ext.notices.values().all(|n| n.passive && !n.wake && n.state == NoticeState::Pending));
    }

    #[test]
    fn rebinding_without_replacement_evidence_is_review_and_old_active_director_is_told() {
        let before = roster("a", 1, false);
        let mut after = before.clone();
        set_authority(&mut after, "b", 2);
        let mut ext = ManagementExtension::default();
        let t = change(&mut ext, &before, &after, &intent("bind-2", Some(("a", 1)), ("b", 2)));
        assert_eq!((t.reason, t.handover), (ChangeReason::OwnerAssigned, HandoverPolicy::Review));
        assert!(recipients(&ext, 2).contains(&"cutex.a".to_string()));
        // a replace event alone, with the old session still active, is no proof
        let mut before = roster("a", 1, false);
        add_replace(&mut before, "rep-1", "a", "b");
        assert_eq!(trusted_replacement(&before, &pid("p1"), &sid("a"), &sid("b")), None);
        // two replace actions claiming the same pair are no proof either
        let mut before = roster("a", 1, true);
        add_replace(&mut before, "rep-1", "a", "b");
        add_replace(&mut before, "rep-2", "a", "b");
        assert_eq!(trusted_replacement(&before, &pid("p1"), &sid("a"), &sid("b")), None);
    }

    #[test]
    fn initial_bind_and_epoch_refresh_send_nothing() {
        let mut ext = ManagementExtension::default();
        let s = roster("a", 1, false);
        let t = change(&mut ext, &AgentManagementSnapshot::empty(), &s, &intent("init", None, ("a", 1)));
        assert_eq!((t.reason, t.handover), (ChangeReason::Initial, HandoverPolicy::None));
        let mut s2 = s.clone();
        set_authority(&mut s2, "a", 2);
        let t = change(&mut ext, &s, &s2, &intent("refresh", Some(("a", 1)), ("a", 2)));
        assert_eq!(t.reason, ChangeReason::EpochRefresh);
        assert!(ext.notices.is_empty());
        assert_eq!(ext.accounted_epochs[&pid("p1")], 2);
    }

    #[test]
    fn prepare_and_finalize_are_idempotent_and_payload_bound() {
        let before = roster("a", 1, false);
        let mut after = before.clone();
        set_authority(&mut after, "b", 2);
        let mut ext = ManagementExtension::default();
        let i = intent("bind-2", Some(("a", 1)), ("b", 2));
        change(&mut ext, &before, &after, &i);
        let snapshot = ext.clone();
        assert!(!prepare_transition(&mut ext, &i, &before).unwrap());
        assert!(!finalize_transition(&mut ext, &receipt(&i), &after).unwrap());
        assert_eq!(ext, snapshot, "replay changes nothing (no second notice)");
        let mut other = i.clone();
        other.request_sha256 = sha("different");
        assert_eq!(prepare_transition(&mut ext, &other, &before), Err(AgentManagementError::Conflict("director_change_payload_conflict")));
        assert_eq!(finalize_transition(&mut ext, &receipt(&other), &after), Err(AgentManagementError::Conflict("director_change_receipt_mismatch")));
    }

    #[test]
    fn a_to_b_to_c_supersedes_and_summarises_pending_notices() {
        let s1 = roster("a", 1, false);
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        let mut s3 = s2.clone();
        set_authority(&mut s3, "c", 3);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &s1, &s2, &intent("to-b", Some(("a", 1)), ("b", 2)));
        let to_c = change(&mut ext, &s2, &s3, &intent("to-c", Some(("b", 2)), ("c", 3)));
        let m1_old = ext.notices[&notice_key(&pid("p1"), 2, &sid("m1"))].clone();
        assert_eq!(m1_old.state, NoticeState::Superseded);
        let m1_new = ext.notices[&notice_key(&pid("p1"), 3, &sid("m1"))].clone();
        assert_eq!(m1_new.chain, vec![m1_old.change_id.clone()]);
        assert_eq!(m1_new.change_id, to_c.change_id);
        let text = notice_text(&m1_new, &s3).unwrap();
        assert!(text.contains("c (") && text.contains("epoch 3"), "{text}");
        // a B notice read only after C took office still names C
        let mut ext2 = ManagementExtension::default();
        change(&mut ext2, &s1, &s2, &intent("to-b", Some(("a", 1)), ("b", 2)));
        let late = ext2.notices[&notice_key(&pid("p1"), 2, &sid("m1"))].clone();
        let text = notice_text(&late, &s3).unwrap();
        assert!(text.contains("-> c (") && text.contains("earlier change"), "{text}");
        // a Director without a ManagedAgentRecord is still told
        let mut bare = s3.clone();
        bare.agents.remove(&sid("c"));
        assert!(notice_text(&m1_new, &bare).is_some());
        let mut c_notice = m1_new.clone();
        c_notice.recipient = sid("c");
        assert!(notice_text(&c_notice, &bare).is_some());
        // a member who left the project is not told
        let mut left = s3.clone();
        left.agents.get_mut(&sid("m2")).unwrap().retired_at = Some(ts());
        assert!(notice_text(&ext.notices[&notice_key(&pid("p1"), 3, &sid("m2"))], &left).is_none());
        let key = notice_key(&pid("p1"), 3, &sid("m1"));
        assert!(mark_notice(&mut ext, &key, NoticeState::Delivered));
        assert!(pending_notices(&ext, &sid("m1")).is_empty());
    }

    #[test]
    fn finalize_after_the_authority_moved_on_is_superseded_without_notices() {
        let s1 = roster("a", 1, false);
        let mut s3 = s1.clone();
        set_authority(&mut s3, "c", 3);
        let mut ext = ManagementExtension::default();
        let t = change(&mut ext, &s1, &s3, &intent("to-b", Some(("a", 1)), ("b", 2)));
        assert_eq!(t.state, TransitionState::Superseded);
        assert!(ext.notices.is_empty());
    }

    #[test]
    fn recovery_settles_prepared_intents_from_real_receipts_and_flags_bypassed_changes() {
        let s1 = roster("a", 1, false);
        let mut ext = ManagementExtension::default();
        // first run: baseline only
        assert!(recover(&mut ext, &s1).unwrap());
        assert_eq!(ext.accounted_epochs[&pid("p1")], 1);
        assert!(ext.transitions.is_empty());
        // crash after CAS, before finalize
        let i = intent("to-b", Some(("a", 1)), ("b", 2));
        prepare_transition(&mut ext, &i, &s1).unwrap();
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        s2.authority_receipts.insert(i.action_id.clone(), receipt(&i));
        recover(&mut ext, &s2).unwrap();
        assert_eq!(ext.transitions[&change_id(&pid("p1"), &i.action_id)].state, TransitionState::Finalized);
        assert!(!ext.notices.is_empty());
        // crash before CAS: no receipt -> aborted, no notice
        let j = intent("to-c", Some(("b", 2)), ("c", 3));
        prepare_transition(&mut ext, &j, &s2).unwrap();
        let notices = ext.notices.len();
        recover(&mut ext, &s2).unwrap();
        assert_eq!(ext.transitions[&change_id(&pid("p1"), &j.action_id)].state, TransitionState::Aborted);
        assert_eq!(ext.notices.len(), notices);
        // an old binary moved the authority without the hook
        let mut s4 = s2.clone();
        set_authority(&mut s4, "c", 4);
        recover(&mut ext, &s4).unwrap();
        let bypassed: Vec<_> = ext.transitions.values().filter(|t| t.reason == ChangeReason::Recovery).collect();
        assert_eq!(bypassed.len(), 1);
        assert_eq!(bypassed[0].handover, HandoverPolicy::OwnerApprovedScopeOnly);
        assert_eq!(bypassed[0].old_director, Some(sid("b")));
        assert_eq!(ext.notices.len(), notices, "a recovered change sends no notice");
        assert!(!recover(&mut ext, &s4).unwrap(), "recovery is idempotent");
    }

    struct FailingStore;
    impl ExtensionStateStore for FailingStore {
        fn read(&self) -> anyhow::Result<ManagementExtension> {
            anyhow::bail!("down")
        }
        fn update(&self, _: &mut dyn FnMut(&mut ManagementExtension) -> anyhow::Result<bool>) -> anyhow::Result<()> {
            anyhow::bail!("down")
        }
    }

    pub(crate) fn provider_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cutex-director-mgmt-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn bind(provider: &AgentManagementProvider, action: &str, director: &str, expected: Option<(&str, u64)>) -> Result<ProjectAuthorityReceipt, AgentManagementError> {
        provider.bind_project_authority(&ProjectAuthorityRequest {
            schema: AgentManagementSchema::V1,
            action_id: aid(action),
            project_id: pid("p1"),
            authorized_director_session: sid(director),
            expected_authorized_director_session: expected.map(|e| sid(e.0)),
            expected_authority_epoch: expected.map(|e| e.1),
        })
    }

    #[test]
    fn provider_hook_prepares_before_the_cas_finalizes_after_and_hands_over_once() {
        let root = provider_root("hook");
        let store: Arc<dyn ExtensionStateStore> = Arc::new(ManagementStore::at(root.join("director-address")));
        let calls: Arc<Mutex<Vec<(String, String, String, bool)>>> = Arc::default();
        let seen = calls.clone();
        let hook = ManagementHook::new(store.clone()).with_handover(Arc::new(move |p, f, t, trusted, _change| {
            seen.lock().unwrap().push((p.into(), f.into(), t.into(), trusted));
            Ok(vec![])
        }));
        let provider = AgentManagementProvider::open(root.join("am")).unwrap().with_authority_hook(Arc::new(hook));
        bind(&provider, "init", "a", None).unwrap();
        let receipt = bind(&provider, "to-b", "b", Some(("a", 1))).unwrap();
        assert_eq!(bind(&provider, "to-b", "b", Some(("a", 1))).unwrap(), receipt, "replay returns the receipt");
        let ext = store.read().unwrap();
        let t = &ext.transitions[&change_id(&pid("p1"), &aid("to-b"))];
        assert_eq!((t.state, t.handover, t.handover_dispatched), (TransitionState::Finalized, HandoverPolicy::Review, true));
        assert_eq!(*calls.lock().unwrap(), vec![("p1".into(), "cutex.a".into(), "cutex.b".into(), false)]);
        assert!(std::fs::read_to_string(root.join("director-address").join("management-v1.json")).unwrap().contains("\"handoverDispatched\": true"));
    }

    #[test]
    fn provider_refuses_the_bind_when_the_sidecar_cannot_prepare() {
        let root = provider_root("refuse");
        let plain = AgentManagementProvider::open(root.join("am")).unwrap();
        bind(&plain, "init", "a", None).unwrap();
        let provider = AgentManagementProvider::open(root.join("am")).unwrap().with_authority_hook(Arc::new(ManagementHook::new(Arc::new(FailingStore))));
        let before = provider.store().snapshot().unwrap();
        assert_eq!(bind(&provider, "to-b", "b", Some(("a", 1))), Err(AgentManagementError::PersistenceUnavailable));
        assert_eq!(provider.store().snapshot().unwrap(), before, "no CAS without a prepared intent");
    }

    #[test]
    fn handover_failure_is_retried_by_recovery_not_lost() {
        let root = provider_root("retry");
        let store: Arc<dyn ExtensionStateStore> = Arc::new(MemoryExtensionStore::default());
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (f, c) = (fail.clone(), count.clone());
        let handover: HandoverFn = Arc::new(move |_, _, _, _, _| {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if f.load(std::sync::atomic::Ordering::SeqCst) { Err("asks busy".into()) } else { Ok(vec![]) }
        });
        let provider = AgentManagementProvider::open(root.join("am")).unwrap()
            .with_authority_hook(Arc::new(ManagementHook::new(store.clone()).with_handover(handover.clone())));
        bind(&provider, "init", "a", None).unwrap();
        bind(&provider, "to-b", "b", Some(("a", 1))).unwrap();
        assert!(!store.read().unwrap().transitions[&change_id(&pid("p1"), &aid("to-b"))].handover_dispatched);
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        let hook = ManagementHook::new(store.clone()).with_handover(handover);
        hook.recover(&provider.store().snapshot().unwrap()).unwrap();
        hook.recover(&provider.store().snapshot().unwrap()).unwrap();
        assert!(store.read().unwrap().transitions[&change_id(&pid("p1"), &aid("to-b"))].handover_dispatched);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2, "one failed + one successful call, none after");
    }

    // ---- start review / adoption

    fn start_action(s: &mut AgentManagementSnapshot, caller: &str, target: &str) -> AgentActionRecord {
        let action = AgentActionRecord {
            created_by: None,
            action_id: aid("create-w"),
            request_sha256: sha("create-w"),
            operation: AgentOperationKind::Create,
            project_id: pid("p1"),
            caller_cutex_session: sid(caller),
            phase: AgentActionPhase::Complete,
            phase_sequence: 5,
            reserved_agent_name: None,
            reserved_agent_cwd: None,
            known_successor_cutex_session: Some(sid(target)),
            known_native_session_id: None,
            native_bootstrap_retryable: false,
            historical_runtime_occurrence_fence: None,
            external_message_id: Some("msg-start-w".into()),
            response: None,
            created_at: ts(),
            updated_at: ts(),
        };
        s.actions.insert(action.action_id.clone(), action.clone());
        action
    }

    fn meta(director: &str) -> AgentManagementMessageMetadata {
        AgentManagementMessageMetadata { schema: AgentManagementSchema::V1, requested_by_director: sid(director), requested_by_operator: None }
    }

    /// a created worker m1 with a start message, then the authority moved to b.
    fn changed_world() -> (ManagementExtension, AgentManagementSnapshot, AgentActionRecord) {
        let s1 = roster("a", 1, false);
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &s1, &s2, &intent("to-b", Some(("a", 1)), ("b", 2)));
        let action = start_action(&mut s2, "a", "m1");
        (ext, s2, action)
    }

    /// Bridge fixtures: worker cutex.m1 created by Director cutex.a with
    /// start "msg-start-w" ("do X"); the authority then moved to cutex.b.
    pub(crate) fn changed_world_fixture() -> (ManagementExtension, AgentManagementSnapshot) {
        let (ext, s, _) = changed_world();
        (ext, s)
    }

    /// The same world with the review approved by cutex.b (receipt Approved).
    pub(crate) fn adopted_fixture() -> (ManagementExtension, AgentManagementSnapshot, AdoptionReceipt) {
        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        let receipt = approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("b")), 1, 2), &s, false).unwrap();
        (ext, s, receipt)
    }

    pub(crate) fn move_authority(s: &mut AgentManagementSnapshot, director: &str, epoch: u64) {
        set_authority(s, director, epoch);
    }

    fn decision(action: &str, actor: ReviewActor, revision: u64, epoch: u64) -> ReviewDecision {
        ReviewDecision { action_id: action.into(), review_id: review_id(&aid("create-w"), "msg-start-w"), expected_revision: revision, expected_epoch: epoch, actor, reason: None }
    }

    #[test]
    fn only_a_director_change_opens_a_start_review() {
        let (ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        assert_eq!((found.original_director.clone(), found.current_director.clone()), (sid("a"), sid("b")));
        // other defects stay plain quarantines
        assert!(classify_director_changed_start(&meta("a"), "cutex.m2", &s, &action, &ext).is_none(), "wrong recipient");
        let mut forged = action.clone();
        forged.caller_cutex_session = sid("m2");
        assert!(classify_director_changed_start(&meta("a"), "cutex.m1", &s, &forged, &ext).is_none(), "caller mismatch");
        assert!(classify_director_changed_start(&meta("b"), "cutex.m1", &s, &action, &ext).is_none(), "Director did not change");
        assert!(classify_director_changed_start(&meta("x"), "cutex.m1", &s, &action, &ext).is_none(), "untrusted old Director");
        let mut early = action.clone();
        early.phase = AgentActionPhase::Prepared;
        assert!(classify_director_changed_start(&meta("a"), "cutex.m1", &s, &early, &ext).is_none(), "never reached its message phase");
        let mut retired = s.clone();
        retired.agents.get_mut(&sid("m1")).unwrap().retired_at = Some(ts());
        assert!(classify_director_changed_start(&meta("a"), "cutex.m1", &retired, &action, &ext).is_none(), "target retired");
    }

    #[test]
    fn current_director_adopts_once_and_admission_is_fenced_by_the_epoch() {
        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        assert!(open_start_review(&mut ext, &found, "msg-start-w", "do X"));
        assert!(!open_start_review(&mut ext, &found, "msg-start-w", "do X"), "idempotent");
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("a")), 1, 2), &s, false), Err("not_current_director"));
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("b")), 1, 1), &s, false), Err("stale_project_authority"));
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("b")), 7, 2), &s, false), Err("start_review_revision_conflict"));
        let r = approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("b")), 1, 2), &s, false).unwrap();
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Session(sid("b")), 1, 2), &s, false).unwrap(), r, "replay");
        assert_eq!(approve_start_review(&mut ext, &decision("ap-2", ReviewActor::Owner, 2, 2), &s, false), Err("start_review_not_open"), "a concurrent second decision loses");
        assert_ne!(r.message_id, "msg-start-w");
        assert!(may_submit(&r));
        let (m, _) = adopted_start_payload(&r, "do X");
        assert_eq!(m.requested_by_director, sid("b"));
        assert_eq!(validate_adopted_start(&r, "cutex.m1", Some(&r.message_id), "do X", &s), Ok(()));
        assert_eq!(validate_adopted_start(&r, "cutex.m1", Some(&r.message_id), "do Y", &s), Err("adoption_content_changed"));
        assert_eq!(validate_adopted_start(&r, "cutex.m2", Some(&r.message_id), "do X", &s), Err("adoption_recipient_conflict"));
        assert_eq!(validate_adopted_start(&r, "cutex.m1", Some("msg-start-w"), "do X", &s), Err("adoption_message_id_mismatch"));
        // the source action itself is not rewritten
        assert_eq!(s.actions[&aid("create-w")].caller_cutex_session, sid("a"));
        // B's approval is not C's
        let mut s3 = s.clone();
        set_authority(&mut s3, "c", 3);
        assert_eq!(validate_adopted_start(&r, "cutex.m1", Some(&r.message_id), "do X", &s3), Err("adoption_authority_changed"));
        assert!(mark_adoption(&mut ext, &r.adoption_id, AdoptionState::Stale).unwrap());
        let review = &ext.start_reviews[&r.review_id];
        assert_eq!((review.state, review.revision), (StartReviewState::Open, 3));
        let r2 = approve_start_review(&mut ext, &decision("ap-3", ReviewActor::Session(sid("c")), 3, 3), &s3, false).unwrap();
        assert_ne!(r2.message_id, r.message_id);
    }

    #[test]
    fn unknown_delivery_never_resends_and_discard_is_final() {
        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Owner, 1, 2), &s, true), Err("source_delivery_unknown"));
        assert_eq!(ext.start_reviews.values().next().unwrap().state, StartReviewState::DeliveryUnknown);
        assert!(ext.adoptions.is_empty());

        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        let gone = discard_start_review(&mut ext, &decision("d-1", ReviewActor::Session(sid("b")), 1, 2), &s).unwrap();
        assert_eq!(gone.state, StartReviewState::Discarded);
        assert_eq!(discard_start_review(&mut ext, &decision("d-1", ReviewActor::Session(sid("b")), 1, 2), &s).unwrap(), gone, "replay");
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Owner, 2, 2), &s, false), Err("start_review_not_open"));
        ext.start_reviews.clear();
        assert!(!open_start_review(&mut ext, &found, "msg-start-w", "do X"), "a discarded source is not re-queued");

        // an unknown submission of the adopted message is never resubmitted
        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        let r = approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Owner, 1, 2), &s, false).unwrap();
        assert!(r.approved_by_owner);
        mark_adoption(&mut ext, &r.adoption_id, AdoptionState::SubmissionUnknown).unwrap();
        assert!(!may_submit(&ext.adoptions[&r.adoption_id]));
        assert_eq!(mark_adoption(&mut ext, &r.adoption_id, AdoptionState::Approved), Err("adoption_state_conflict"));
    }

    #[test]
    fn an_unknown_delivery_review_can_still_be_discarded_under_current_checks() {
        let (mut ext, s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        assert_eq!(approve_start_review(&mut ext, &decision("ap-1", ReviewActor::Owner, 1, 2), &s, true), Err("source_delivery_unknown"));
        let revision = ext.start_reviews.values().next().unwrap().revision;
        // stale epoch / revision / a non-current actor are refused
        assert!(discard_start_review(&mut ext, &decision("d-0", ReviewActor::Owner, revision, 1), &s).is_err());
        assert!(discard_start_review(&mut ext, &decision("d-0", ReviewActor::Owner, revision - 1, 2), &s).is_err());
        assert!(discard_start_review(&mut ext, &decision("d-0", ReviewActor::Session(sid("a")), revision, 2), &s).is_err());
        let gone = discard_start_review(&mut ext, &decision("d-1", ReviewActor::Session(sid("b")), revision, 2), &s).unwrap();
        assert_eq!(gone.state, StartReviewState::Discarded);
        // approval still needs an open item
        assert_eq!(
            approve_start_review(&mut ext, &decision("ap-2", ReviewActor::Owner, gone.revision, 2), &s, false),
            Err("start_review_not_open")
        );
    }

    #[test]
    fn adoption_reconcile_follows_the_exact_bus_record_and_never_resends() {
        use AdoptionState as S;
        assert_eq!(reconcile_adoption(S::SubmissionUnknown, Some(("delivered", true)), false), Some(S::Delivered));
        assert_eq!(reconcile_adoption(S::Submitted, Some(("delivered", true)), false), Some(S::Delivered));
        assert_eq!(reconcile_adoption(S::SubmissionUnknown, Some(("pending", false)), false), Some(S::Submitted));
        assert_eq!(reconcile_adoption(S::Submitted, Some(("inbox_accepted", true)), false), None);
        // missing / failed / quarantined: stays as it is, nothing is resent
        assert_eq!(reconcile_adoption(S::SubmissionUnknown, None, false), None);
        assert_eq!(reconcile_adoption(S::Submitted, Some(("failed", false)), false), None);
        assert_eq!(reconcile_adoption(S::SubmissionUnknown, Some(("quarantined", false)), false), None);
        // refused before admission after B -> C: only a proven Submitted reopens
        assert_eq!(reconcile_adoption(S::Submitted, Some(("quarantined", false)), true), Some(S::Stale));
        assert_eq!(reconcile_adoption(S::Submitted, Some(("quarantined", true)), true), None, "native evidence");
        assert_eq!(reconcile_adoption(S::Submitted, Some(("quarantined", false)), false), None, "authority unchanged");
        assert_eq!(reconcile_adoption(S::SubmissionUnknown, Some(("quarantined", false)), true), None, "unknown is never absent");
        assert_eq!(reconcile_adoption(S::Submitted, None, true), None);
    }

    #[test]
    fn a_bus_queued_but_unadmitted_adoption_reopens_for_the_next_director() {
        let (mut ext, _, receipt) = adopted_fixture();
        mark_adoption(&mut ext, &receipt.adoption_id, AdoptionState::Submitted).unwrap();
        assert_eq!(mark_adoption(&mut ext, &receipt.adoption_id, AdoptionState::Stale), Err("adoption_state_conflict"), "not without evidence");
        // B -> C before admission: the bridge quarantined it (no native evidence)
        assert_eq!(reconcile_adoption(AdoptionState::Submitted, Some(("quarantined", false)), true), Some(AdoptionState::Stale));
        assert!(mark_adoption_refused_unadmitted(&mut ext, &receipt.adoption_id).unwrap());
        assert_eq!(ext.adoptions[&receipt.adoption_id].state, AdoptionState::Stale);
        assert!(!may_submit(&ext.adoptions[&receipt.adoption_id]), "never sent automatically");
        let review = &ext.start_reviews[&receipt.review_id];
        assert_eq!((review.state, review.adoption_id.as_ref()), (StartReviewState::Open, None), "C re-reviews it");
        assert_eq!(mark_adoption_refused_unadmitted(&mut ext, &receipt.adoption_id), Err("adoption_state_conflict"));
    }

    #[test]
    fn a_director_notice_is_admitted_only_with_its_own_record_and_recipient() {
        let mut before = roster("a", 1, true);
        add_replace(&mut before, "rep-1", "a", "b");
        let mut after = before.clone();
        set_authority(&mut after, "b", 2);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &before, &after, &intent("bind-2", Some(("a", 1)), ("b", 2)));
        let notice = ext.notices.values().next().cloned().expect("a notice");
        let id = notice_external_id(&notice.key);
        assert!(id.starts_with("director-change:"));
        let body = notice_text(&notice, &after).unwrap();
        assert!(!body.contains("Message Type:"), "the bridge adds the NOTICE header");
        assert_eq!(validate_director_notice(&ext, notice.recipient.as_str(), Some(&id), &after), Ok(()));
        assert_eq!(validate_director_notice(&ext, "cutex.stranger", Some(&id), &after), Err("notice_recipient_conflict"));
        assert_eq!(validate_director_notice(&ext, notice.recipient.as_str(), Some("director-change:forged"), &after), Err("notice_record_absent"));
        assert_eq!(validate_director_notice(&ext, notice.recipient.as_str(), None, &after), Err("notice_message_id_missing"));
        mark_notice(&mut ext, &notice.key, NoticeState::Superseded);
        assert_eq!(validate_director_notice(&ext, notice.recipient.as_str(), Some(&id), &after), Err("notice_not_deliverable"));
    }

    #[test]
    fn a_queued_but_unread_notice_is_held_after_the_next_change_and_the_new_one_carries_it() {
        let s1 = roster("a", 1, false);
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        let mut s3 = s2.clone();
        set_authority(&mut s3, "c", 3);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &s1, &s2, &intent("to-b", Some(("a", 1)), ("b", 2)));
        let b_key = notice_key(&pid("p1"), 2, &sid("m1"));
        let b_notice = ext.notices[&b_key].clone();
        // the A->B notice was queued on the bus (text frozen) but not read yet
        assert!(mark_notice(&mut ext, &b_key, NoticeState::Delivered));
        let b_id = notice_external_id(&b_key);
        assert_eq!(validate_director_notice(&ext, "cutex.m1", Some(&b_id), &s2), Ok(()));
        change(&mut ext, &s2, &s3, &intent("to-c", Some(("b", 2)), ("c", 3)));
        // never "now B" after C: the frozen B notice is held (silently)
        assert_eq!(validate_director_notice(&ext, "cutex.m1", Some(&b_id), &s3), Err("notice_superseded"));
        let c_key = notice_key(&pid("p1"), 3, &sid("m1"));
        let c_notice = ext.notices[&c_key].clone();
        assert_eq!(c_notice.chain, vec![b_notice.change_id.clone()], "the C notice carries the A->B step");
        assert_eq!(c_notice.state, NoticeState::Pending);
        assert_eq!(validate_director_notice(&ext, "cutex.m1", Some(&notice_external_id(&c_key)), &s3), Ok(()));
        let text = notice_text(&c_notice, &s3).unwrap();
        assert!(text.contains("-> c (") && text.contains("earlier change"), "{text}");
    }

    #[test]
    fn recovery_does_not_flag_an_epoch_a_prepared_record_covers() {
        let s1 = roster("a", 1, false);
        let mut ext = ManagementExtension::default();
        recover(&mut ext, &s1).unwrap();
        let i = intent("to-b", Some(("a", 1)), ("b", 2));
        prepare_transition(&mut ext, &i, &s1).unwrap();
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        s2.authority_receipts.insert(i.action_id.clone(), receipt(&i));
        recover(&mut ext, &s2).unwrap();
        let at_two: Vec<_> = ext.transitions.values().filter(|t| t.new_epoch == 2).collect();
        assert_eq!(at_two.len(), 1, "no duplicate Recovery record");
        assert_eq!(at_two[0].state, TransitionState::Finalized);
        assert_eq!(at_two[0].reason, ChangeReason::OwnerAssigned);
    }

    #[test]
    fn a_finalized_change_emits_its_ui_event_once_and_retries_a_failed_append() {
        let root = provider_root("event");
        let store: Arc<dyn ExtensionStateStore> = Arc::new(ManagementStore::at(root.join("director-address")));
        let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (seen, failing) = (events.clone(), fail.clone());
        let hook = Arc::new(
            ManagementHook::new(store.clone())
                .with_handover(Arc::new(|_, _, _, _, _| Ok(vec![])))
                .with_event(Arc::new(move |t| {
                    if failing.load(std::sync::atomic::Ordering::SeqCst) {
                        anyhow::bail!("event stream down");
                    }
                    seen.lock().unwrap().push(director_changed_params(t));
                    Ok(())
                })),
        );
        let provider = AgentManagementProvider::open(root.join("am")).unwrap().with_authority_hook(hook.clone());
        bind(&provider, "init", "a", None).unwrap();
        bind(&provider, "to-b", "b", Some(("a", 1))).unwrap();
        assert!(events.lock().unwrap().is_empty());
        assert!(!store.read().unwrap().transitions[&change_id(&pid("p1"), &aid("to-b"))].event_emitted);
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        provider.recover_authority_hook().unwrap();
        provider.recover_authority_hook().unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "one event per change, the initial bind has none");
        assert_eq!(events[0]["schema"], "cutex/project-director-changed/v1");
        assert_eq!((events[0]["oldDirector"].as_str(), events[0]["newDirector"].as_str()), (Some("cutex.a"), Some("cutex.b")));
        assert!(store.read().unwrap().transitions[&change_id(&pid("p1"), &aid("to-b"))].event_emitted);
    }

    // ------------------------------------------------ r254 review follow-ups

    pub(crate) type Calls = Arc<Mutex<Vec<String>>>;

    /// Hook over a file store recording handover / owner-scope / event calls.
    pub(crate) fn recording_hook(root: &std::path::Path) -> (Arc<dyn ExtensionStateStore>, Arc<ManagementHook>, Calls) {
        let store: Arc<dyn ExtensionStateStore> = Arc::new(ManagementStore::at(root.join("director-address")));
        let calls: Calls = Arc::default();
        let (h, r, e) = (calls.clone(), calls.clone(), calls.clone());
        let hook = ManagementHook::new(store.clone())
            .with_handover(Arc::new(move |p, f, t, trusted, _| {
                h.lock().unwrap().push(format!("handover {p} {f}->{t} trusted={trusted}"));
                Ok(vec![])
            }))
            .with_recover_scope(Arc::new(move |p, prev, cur, _| {
                r.lock().unwrap().push(format!("owner-scope {p} {prev:?}->{cur:?}"));
                Ok(vec![])
            }))
            .with_event(Arc::new(move |t| {
                e.lock().unwrap().push(format!("event {:?}->{:?} cleared={}", t.old_director.as_ref().map(|d| d.as_str().to_string()), t.new_director.as_ref().map(|d| d.as_str().to_string()), t.new_director.is_none()));
                Ok(())
            }));
        (store, Arc::new(hook), calls)
    }

    pub(crate) fn owner_mutation(
        action: &str,
        epoch: Option<u64>,
        operation: crate::management::control_plane::HumanManagementProjectMutationKind,
    ) -> crate::management::control_plane::HumanManagementProjectMutationRequest {
        crate::management::control_plane::HumanManagementProjectMutationRequest {
            schema: crate::management::control_plane::HumanManagementProjectMutationSchema::V1,
            action_id: aid(action),
            project_id: pid("p1"),
            expected_authority_epoch: epoch,
            expected_project_revision: None,
            operation,
        }
    }

    pub(crate) fn owner_provider(root: &std::path::Path, hook: Arc<ManagementHook>) -> AgentManagementProvider {
        let provider = AgentManagementProvider::open(root.join("am")).unwrap().with_authority_hook(hook);
        provider
            .store()
            .with_state(true, |mut state| {
                for id in ["a", "b", "c", "m1"] {
                    state.agents.insert(sid(id), agent(id, None, false));
                }
                Ok((state, (), true))
            })
            .unwrap();
        provider
    }

    pub(crate) fn run_owner(
        provider: &AgentManagementProvider,
        request: &crate::management::control_plane::HumanManagementProjectMutationRequest,
    ) -> Result<(), AgentManagementError> {
        provider
            .execute_project_mutation_for_management(
                &crate::management::control_plane::HumanManagementPrincipal::authenticated(),
                request,
            )
            .map(|_| ())
    }

    #[test]
    fn owner_rotate_and_clear_go_through_the_hook_with_notices_events_and_owner_only_scope() {
        use crate::management::control_plane::HumanManagementProjectMutationKind as K;
        let root = provider_root("owner");
        let (store, hook, calls) = recording_hook(&root);
        let provider = owner_provider(&root, hook);
        run_owner(&provider, &owner_mutation("create", None, K::CreateProject {
            presentation: crate::agent_management::ProjectPresentationInput {
                display_name: "P1".into(),
                badge_label: "P".into(),
                color: crate::agent_management::ProjectPaletteColor::Green,
            },
            workspace_root: None,
            director_cutex_session_id: Some(sid("a")),
            create_workspace_root: false,
        }))
        .unwrap();
        // the owner rotation route (/v2/director-rotations rebind) A -> B
        let rotate = owner_mutation("owner-rotation:r1:rebind", Some(1), K::RotateDirector { successor_cutex_session_id: sid("b") });
        run_owner(&provider, &rotate).unwrap();
        run_owner(&provider, &rotate).unwrap(); // replay
        let ext = store.read().unwrap();
        let t = &ext.transitions[&change_id(&pid("p1"), &aid("owner-rotation:r1:rebind"))];
        assert_eq!(
            (t.state, t.reason, t.handover, t.new_director.clone(), t.handover_dispatched, t.event_emitted),
            (TransitionState::Finalized, ChangeReason::OwnerAssigned, HandoverPolicy::Review, Some(sid("b")), true, true)
        );
        assert!(ext.notices.contains_key(&notice_key(&pid("p1"), 2, &sid("a"))), "the old (still active) Director is told");
        assert!(ext.notices.contains_key(&notice_key(&pid("p1"), 2, &sid("b"))));
        assert!(ext.notices.values().all(|n| n.passive && !n.wake));
        // owner clear: no new Director, owner-only scope, an event, no notice
        run_owner(&provider, &owner_mutation("clear", Some(2), K::ClearDirector)).unwrap();
        let ext = store.read().unwrap();
        let cleared = &ext.transitions[&change_id(&pid("p1"), &aid("clear"))];
        assert_eq!(
            (cleared.state, cleared.reason, cleared.handover, cleared.new_director.clone(), cleared.new_epoch),
            (TransitionState::Finalized, ChangeReason::Cleared, HandoverPolicy::OwnerApprovedScopeOnly, None, 2)
        );
        assert!(ext.notices.keys().all(|k| !k.contains(":2:") || ext.notices[k].change_id != cleared.change_id));
        // assignment after the clear continues the epoch and tells members
        run_owner(&provider, &owner_mutation("assign", None, K::AssignDirector { cutex_session_id: sid("c") })).unwrap();
        let ext = store.read().unwrap();
        let assigned = &ext.transitions[&change_id(&pid("p1"), &aid("assign"))];
        assert_eq!((assigned.reason, assigned.handover, assigned.new_epoch), (ChangeReason::OwnerAssigned, HandoverPolicy::None, 3));
        assert!(assigned.handover_dispatched, "owner-only seating scope ran");
        assert!(ext.notices.contains_key(&notice_key(&pid("p1"), 3, &sid("c"))));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                // initial seating: owner-only scope (no stamped work: a no-op)
                "owner-scope p1 None->Some(\"cutex.a\")".to_string(),
                "event Some(\"cutex.a\")->Some(\"cutex.b\") cleared=false".into(),
                "handover p1 cutex.a->cutex.b trusted=false".into(),
                "event Some(\"cutex.b\")->None cleared=true".into(),
                "owner-scope p1 Some(\"cutex.b\")->None".into(),
                // assignment after the clear retargets the clear's owner rows
                "event None->Some(\"cutex.c\") cleared=false".into(),
                "owner-scope p1 None->Some(\"cutex.c\")".into(),
            ]
        );
        // a refused owner mutation prepares nothing durable
        assert!(run_owner(&provider, &owner_mutation("bad", Some(9), K::RotateDirector { successor_cutex_session_id: sid("a") })).is_err());
        assert!(!store.read().unwrap().transitions.contains_key(&change_id(&pid("p1"), &aid("bad"))));
    }

    #[test]
    fn an_owner_mutation_intent_is_settled_by_recovery_from_the_durable_mutation_record() {
        use crate::management::control_plane::HumanManagementProjectMutationKind as K;
        let root = provider_root("owner-recover");
        let (store, hook, _) = recording_hook(&root);
        let provider = owner_provider(&root, hook.clone());
        run_owner(&provider, &owner_mutation("create", None, K::CreateProject {
            presentation: crate::agent_management::ProjectPresentationInput {
                display_name: "P1".into(),
                badge_label: "P".into(),
                color: crate::agent_management::ProjectPaletteColor::Green,
            },
            workspace_root: None,
            director_cutex_session_id: Some(sid("a")),
            create_workspace_root: false,
        }))
        .unwrap();
        run_owner(&provider, &owner_mutation("to-b", Some(1), K::RotateDirector { successor_cutex_session_id: sid("b") })).unwrap();
        // simulate a crash between commit and finalize
        let id = change_id(&pid("p1"), &aid("to-b"));
        store
            .update(&mut |ext| {
                let t = ext.transitions.get_mut(&id).unwrap();
                t.state = TransitionState::Prepared;
                t.settled_at = None;
                Ok(true)
            })
            .unwrap();
        // and a prepared intent whose mutation never committed
        store
            .update(&mut |ext| {
                let mut ghost = ext.transitions[&id].clone();
                ghost.change_id = change_id(&pid("p1"), &aid("never"));
                ghost.authority_action_id = Some(aid("never"));
                ext.transitions.insert(ghost.change_id.clone(), ghost);
                Ok(true)
            })
            .unwrap();
        provider.recover_authority_hook().unwrap();
        let ext = store.read().unwrap();
        assert_eq!(ext.transitions[&id].state, TransitionState::Finalized);
        assert_eq!(ext.transitions[&change_id(&pid("p1"), &aid("never"))].state, TransitionState::Aborted);
    }

    #[test]
    fn a_recovered_change_with_an_unknown_previous_director_gets_owner_only_candidates() {
        let root = provider_root("recovery-owner");
        let (store, hook, calls) = recording_hook(&root);
        let provider = AgentManagementProvider::open(root.join("am")).unwrap();
        // project made without the hook (desktop / older binary) ...
        bind(&provider, "init", "a", None).unwrap();
        hook.recover(&provider.store().snapshot().unwrap()).unwrap(); // baseline
        // ... and an unknown earlier history: the baseline knows no Director
        bind(&provider, "to-b", "b", Some(("a", 1))).unwrap();
        hook.recover(&provider.store().snapshot().unwrap()).unwrap();
        let ext = store.read().unwrap();
        let t = ext.transitions.values().find(|t| t.reason == ChangeReason::Recovery).expect("recovery record");
        assert_eq!((t.old_director.clone(), t.handover), (None, HandoverPolicy::OwnerApprovedScopeOnly));
        assert!(t.handover_dispatched);
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&"owner-scope p1 None->Some(\"cutex.b\")".to_string()), "{calls:?}");
        assert!(calls.iter().all(|c| !c.starts_with("handover")), "the successor never gets a self-approvable review: {calls:?}");
    }

    #[test]
    fn recovery_with_a_known_previous_director_is_still_owner_only() {
        let s1 = roster("a", 1, false);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &roster("a", 1, false), &s1, &intent("init", None, ("a", 1)));
        let mut s2 = s1.clone();
        set_authority(&mut s2, "b", 2);
        recover(&mut ext, &s2).unwrap();
        let t = ext.transitions.values().find(|t| t.reason == ChangeReason::Recovery).unwrap();
        assert_eq!((t.old_director.clone(), t.handover), (Some(sid("a")), HandoverPolicy::OwnerApprovedScopeOnly));
        let root = provider_root("recovery-known");
        let (store, hook, calls) = recording_hook(&root);
        store.update(&mut |state| { *state = ext.clone(); Ok(true) }).unwrap();
        hook.dispatch_handovers(&s2).unwrap();
        assert_eq!(*calls.lock().unwrap().last().unwrap(), "owner-scope p1 Some(\"cutex.a\")->Some(\"cutex.b\")");
        assert!(calls.lock().unwrap().iter().all(|c| !c.starts_with("handover")));
    }

    #[test]
    fn a_same_director_epoch_refresh_keeps_the_only_outstanding_notice_deliverable() {
        let mut before = roster("a", 1, true);
        add_replace(&mut before, "rep-1", "a", "b");
        let mut after = before.clone();
        set_authority(&mut after, "b", 2);
        let mut ext = ManagementExtension::default();
        change(&mut ext, &before, &after, &intent("to-b", Some(("a", 1)), ("b", 2)));
        let key = notice_key(&pid("p1"), 2, &sid("m1"));
        let id = notice_external_id(&key);
        mark_notice(&mut ext, &key, NoticeState::Delivered); // queued, maybe unread
        let mut refreshed = after.clone();
        set_authority(&mut refreshed, "b", 3);
        change(&mut ext, &after, &refreshed, &intent("refresh", Some(("b", 2)), ("b", 3)));
        assert!(ext.notices.values().all(|n| n.new_epoch != 3), "a refresh queues no notice");
        assert_eq!(validate_director_notice(&ext, "cutex.m1", Some(&id), &refreshed), Ok(()));
        let text = notice_text(&ext.notices[&key], &refreshed).unwrap();
        assert!(!text.contains("earlier change"), "{text}");
        // another Director still holds it back
        let mut moved = refreshed.clone();
        set_authority(&mut moved, "c", 4);
        assert_eq!(validate_director_notice(&ext, "cutex.m1", Some(&id), &moved), Err("notice_superseded"));
    }

    #[test]
    fn a_newer_sidecar_version_refuses_director_changes_without_touching_it() {
        let root = provider_root("newer");
        let (store, hook, _) = recording_hook(&root);
        let provider = AgentManagementProvider::open(root.join("am")).unwrap().with_authority_hook(hook);
        bind(&provider, "init", "a", None).unwrap();
        let path = root.join("director-address").join("management-v1.json");
        let text = std::fs::read_to_string(&path).unwrap().replace("\"version\": 1", "\"version\": 9");
        std::fs::write(&path, &text).unwrap();
        match bind(&provider, "to-b", "b", Some(("a", 1))) {
            Err(AgentManagementError::ConflictDetail(detail)) => {
                assert!(detail.starts_with("director_address_store_unsupported_version"), "{detail}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(provider.store().snapshot().unwrap().projects[&pid("p1")].authorized_director_session, sid("a"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "never rewritten");
        drop(store);
    }

    #[test]
    fn unknown_top_level_fields_survive_a_rewrite() {
        let root = provider_root("extra");
        let store = ManagementStore::at(root.clone());
        std::fs::write(store.path(), r#"{"version":1,"futureThing":{"x":1}}"#).unwrap();
        store.update(&mut |ext| { ext.discarded_sources.insert("m".into()); Ok(true) }).unwrap();
        let back = store.read().unwrap();
        assert_eq!(back.extra.get("futureThing"), Some(&serde_json::json!({"x":1})));
    }

    fn clear_seat(s: &mut AgentManagementSnapshot, cleared_epoch: u64) {
        s.projects.remove(&pid("p1"));
        s.project_states.insert(
            pid("p1"),
            crate::agent_management::ProjectStateRecord {
                project_id: pid("p1"),
                lifecycle: crate::agent_management::ProjectLifecycle::Active,
                revision: 3,
                created_at: ts(),
                updated_at: ts(),
                archived_at: None,
                workspace_root: None,
                last_director_epoch: Some(cleared_epoch),
            },
        );
    }

    #[test]
    fn only_the_owner_discards_a_start_review_while_the_seat_is_vacant() {
        let (mut ext, mut s, action) = changed_world();
        let found = classify_director_changed_start(&meta("a"), "cutex.m1", &s, &action, &ext).unwrap();
        open_start_review(&mut ext, &found, "msg-start-w", "do X");
        clear_seat(&mut s, 2);
        let d = |actor, revision, epoch| decision("d-1", actor, revision, epoch);
        assert_eq!(approve_start_review(&mut ext, &d(ReviewActor::Owner, 1, 2), &s, false), Err("director_unassigned"), "accept needs a Director");
        assert_eq!(discard_start_review(&mut ext, &d(ReviewActor::Session(sid("b")), 1, 2), &s), Err("director_unassigned"));
        assert_eq!(discard_start_review(&mut ext, &d(ReviewActor::Owner, 1, 1), &s), Err("stale_project_authority"));
        assert_eq!(discard_start_review(&mut ext, &d(ReviewActor::Owner, 0, 2), &s), Err("start_review_revision_conflict"));
        let gone = discard_start_review(&mut ext, &d(ReviewActor::Owner, 1, 2), &s).unwrap();
        assert_eq!((gone.state, gone.settled_by.clone(), gone.settled_by_owner), (StartReviewState::Discarded, None, Some(true)));
        assert_eq!(discard_start_review(&mut ext, &d(ReviewActor::Owner, 1, 2), &s).unwrap(), gone, "replay");
        assert!(ext.discarded_sources.contains("msg-start-w"));
    }

    #[test]
    fn a_start_quarantined_while_the_seat_was_vacant_is_reviewed_once_the_next_director_is_assigned() {
        // A created m1's start, then the owner cleared A before delivery.
        let mut s1 = roster("a", 1, false);
        let action = start_action(&mut s1, "a", "m1");
        let mut ext = ManagementExtension::default();
        let clear = AuthorityTransitionIntent {
            action_id: aid("clear"),
            request_sha256: sha("clear"),
            project_id: pid("p1"),
            previous: s1.projects.get(&pid("p1")).cloned(),
            next: None,
        };
        prepare_transition(&mut ext, &clear, &s1).unwrap();
        ext.transitions.get_mut(&change_id(&pid("p1"), &aid("clear"))).unwrap().state = TransitionState::Finalized;
        let mut vacant = s1.clone();
        clear_seat(&mut vacant, 1);
        let start: crate::agent_bus::model::AgentBusMessage = serde_json::from_value(serde_json::json!({
            "id":"bus-start-w","from":crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER,"to":"runtime-m1",
            "fromCutexSessionId":"cutex.a","toCutexSessionId":"cutex.m1","content":"do X","deliveryMode":"after_turn",
            "triggerTurn":true,"createdAtEpochSecs":1,
            "controlType":crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE,
            "controlPayload":{"schema":"cutex/agent-management/v1","requested_by_director":"cutex.a"},
            "externalMessageId":"msg-start-w"})).unwrap();
        assert_eq!(action.external_message_id.as_deref(), Some("msg-start-w"));
        let store = MemoryExtensionStore::default();
        store.update(&mut |state| { *state = ext.clone(); Ok(true) }).unwrap();
        let reads = std::cell::Cell::new(0);
        let starts = || {
            reads.set(reads.get() + 1);
            Ok(vec![("cutex.m1".to_string(), start.clone())])
        };
        let mut seen = None;
        // startup / first tick while vacant: nobody can review it yet
        assert_eq!(rescan_if_authority_changed(&vacant, &mut seen, &store, starts).unwrap(), 0);
        // unchanged roster: the journal is not read again
        assert_eq!(rescan_if_authority_changed(&vacant, &mut seen, &store, starts).unwrap(), 0);
        assert_eq!(reads.get(), 1);
        // the owner assigns B: the periodic pass opens exactly one review
        let mut assigned = vacant.clone();
        set_authority(&mut assigned, "b", 2);
        assert_eq!(rescan_if_authority_changed(&assigned, &mut seen, &store, starts).unwrap(), 1);
        assert_eq!(rescan_if_authority_changed(&assigned, &mut seen, &store, starts).unwrap(), 0);
        assert_eq!(reads.get(), 2);
        let ext = store.read().unwrap();
        let review = ext.start_reviews.values().next().unwrap();
        assert_eq!((ext.start_reviews.len(), review.state, review.director_at_review.clone()), (1, StartReviewState::Open, sid("b")));
        assert!(ext.adoptions.is_empty(), "nothing delivered before B acts");
        // a discarded source is never re-opened by a later rescan
        let mut ext2 = ext.clone();
        ext2.start_reviews.clear();
        ext2.discarded_sources.insert("msg-start-w".into());
        store.update(&mut |state| { *state = ext2.clone(); Ok(true) }).unwrap();
        let mut moved = assigned.clone();
        set_authority(&mut moved, "c", 3);
        assert_eq!(rescan_if_authority_changed(&moved, &mut seen, &store, starts).unwrap(), 0);
        assert!(store.read().unwrap().start_reviews.is_empty());
    }

    #[test]
    fn a_bypassed_change_after_a_clear_takes_the_clear_as_its_predecessor_in_any_map_order() {
        for (seat_id, clear_id) in [("dchg-0-seat", "dchg-z-clear"), ("dchg-z-seat", "dchg-0-clear")] {
            let mut ext = ManagementExtension::default();
            let s1 = roster("a", 1, false);
            change(&mut ext, &roster("a", 1, false), &s1, &intent("init", None, ("a", 1)));
            let mut s2 = s1.clone();
            set_authority(&mut s2, "b", 2);
            change(&mut ext, &s1, &s2, &intent("to-b", Some(("a", 1)), ("b", 2)));
            // re-key the B seating and add the clear of B at the same epoch 2
            let mut seat = ext.transitions.remove(&change_id(&pid("p1"), &aid("to-b"))).unwrap();
            seat.change_id = seat_id.into();
            let mut clear = seat.clone();
            clear.change_id = clear_id.into();
            clear.old_director = Some(sid("b"));
            clear.new_director = None;
            clear.reason = ChangeReason::Cleared;
            clear.handover = HandoverPolicy::OwnerApprovedScopeOnly;
            clear.settled_at = seat.settled_at.clone(); // same second
            ext.transitions.insert(seat.change_id.clone(), seat);
            ext.transitions.insert(clear.change_id.clone(), clear);
            // B is seated again without the hook (older binary) at epoch 3
            let mut s3 = s2.clone();
            set_authority(&mut s3, "b", 3);
            recover(&mut ext, &s3).unwrap();
            let bypassed = ext.transitions.values().find(|t| t.new_epoch == 3).expect("recorded");
            assert_eq!(
                (bypassed.old_director.clone(), bypassed.reason, bypassed.handover),
                (None, ChangeReason::Recovery, HandoverPolicy::OwnerApprovedScopeOnly),
                "the clear is the predecessor, not B's earlier seating ({seat_id} / {clear_id})"
            );
        }
    }

    #[test]
    fn management_file_store_round_trips_and_refuses_a_newer_version() {
        let root = provider_root("file");
        let store = ManagementStore::at(root.clone());
        store.update(&mut |ext| { ext.discarded_sources.insert("m".into()); Ok(true) }).unwrap();
        assert!(store.read().unwrap().discarded_sources.contains("m"));
        let text = std::fs::read_to_string(store.path()).unwrap();
        std::fs::write(store.path(), text.replace("\"version\": 1", "\"version\": 9")).unwrap();
        assert!(store.read().is_err());
        let _ = (CurrentProjectMembership { cutex_session_id: sid("x"), project_id: None, revision: 0, updated_at: ts() },);
    }
}

// ---------------------------------------------------------------- bus dispatch

/// What the bus dispatcher sends for this extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchKind {
    /// An approved adoption: control [`START_ADOPTED_CONTROL_TYPE`], after
    /// turn, fixed message id = receipt.messageId.
    AdoptedStart { adoption_id: String },
    /// A passive Director-change notice: control
    /// [`DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE`], passive, no wake.
    Notice { key: String },
}

/// Control type of a passive Director-change notice (admitted only with a
/// matching pending / delivered notice record).
pub const DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE: &str = "cutex.project.directorChanged.v1";

/// One message the bus process must queue through the in-process Management
/// principal. Only this module can build one (the principal is crate-private).
pub struct DirectorDispatch {
    pub principal: crate::agent_bus::identity::AgentManagementSystemPrincipal,
    pub metadata: AgentManagementMessageMetadata,
    pub target: CutexSessionId,
    pub text: String,
    pub external_message_id: String,
    pub kind: DispatchKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// The bus accepted it with the exact message id.
    Sent,
    /// The send outcome is unknown: an adoption is never resubmitted.
    Unknown,
}

fn notice_external_id(key: &str) -> String {
    format!("director-change:{}", &digest(&["notice", key])[..32])
}

/// Source content of a quarantined start (exact canonical envelope) and its
/// live first-message outcome ([`crate::agent_management::first_message::report`]:
/// `not_delivered` / `unknown` / `owner_discarded`).
fn source_start(source_message_id: &str, target: &CutexSessionId) -> anyhow::Result<Option<(String, String)>> {
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let messages = repository.management_start_messages()?;
    let Some(found) = crate::agent_management::first_message::find(&messages, source_message_id, target.as_str()) else {
        return Ok(None);
    };
    let canonical = repository.canonical_message(&found.message_id)?;
    let outcome = crate::agent_management::first_message::report(found)["outcome"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    Ok(Some((canonical.content, outcome)))
}

/// Everything due now: approved adoptions whose authority still holds (an
/// adoption whose epoch moved is marked Stale and its review reopened) and
/// pending notices whose recipient is still a member (others superseded).
pub fn pending_dispatches() -> anyhow::Result<Vec<DirectorDispatch>> {
    let store = default_store_or_file()?;
    let roster = crate::agent_management::AgentManagementStore::open_default()?.snapshot()?;
    let ext = store.read()?;
    let mut out = Vec::new();
    let mut stale = Vec::new();
    let mut reconcile = Vec::new();
    let mut gone = Vec::new();
    for receipt in ext.adoptions.values().filter(|r| may_submit(r)) {
        // Dispatch fence: the source must still be quarantined and provably
        // undelivered right now. Discarded, missing, or possibly in the
        // runtime: never send, never revive; the adoption goes to reconcile.
        let source = source_start(&receipt.source_message_id, &receipt.target_session)?;
        let Some((content, "not_delivered")) = source.as_ref().map(|(c, o)| (c.clone(), o.as_str())) else {
            eprintln!(
                "director-address adoption {} held for reconcile: source outcome {:?}",
                receipt.adoption_id,
                source.as_ref().map(|(_, o)| o.as_str())
            );
            reconcile.push(receipt.adoption_id.clone());
            continue;
        };
        match validate_adopted_start(receipt, receipt.target_session.as_str(), Some(&receipt.message_id), &content, &roster) {
            Ok(()) => {
                let (metadata, _) = adopted_start_payload(receipt, &content);
                out.push(DirectorDispatch {
                    principal: crate::agent_bus::identity::agent_management_system_principal(),
                    metadata,
                    target: receipt.target_session.clone(),
                    text: content,
                    external_message_id: receipt.message_id.clone(),
                    kind: DispatchKind::AdoptedStart { adoption_id: receipt.adoption_id.clone() },
                });
            }
            Err("adoption_authority_changed") => stale.push(receipt.adoption_id.clone()),
            Err(why) => eprintln!("director-address adoption {} not sent: {why}", receipt.adoption_id),
        }
    }
    for notice in ext.notices.values().filter(|n| n.state == NoticeState::Pending) {
        let Some(authority) = roster.projects.get(&notice.project_id) else { continue };
        match notice_text(notice, &roster) {
            Some(text) => out.push(DirectorDispatch {
                principal: crate::agent_bus::identity::agent_management_system_principal(),
                metadata: AgentManagementMessageMetadata {
                    schema: crate::agent_management::AgentManagementSchema::V1,
                    requested_by_director: authority.authorized_director_session.clone(),
                    requested_by_operator: None,
                },
                target: notice.recipient.clone(),
                text,
                external_message_id: notice_external_id(&notice.key),
                kind: DispatchKind::Notice { key: notice.key.clone() },
            }),
            None => gone.push(notice.key.clone()),
        }
    }
    // Reconcile sent / unknown adoptions from the bus record of exactly
    // their message id: never a resend, a missing record stays unknown.
    let mut settled = Vec::new();
    for receipt in ext
        .adoptions
        .values()
        .filter(|r| matches!(r.state, AdoptionState::Submitted | AdoptionState::SubmissionUnknown))
    {
        let snapshot = repository_snapshot(&receipt.message_id)?;
        let moved = roster.projects.get(&receipt.project_id).map_or(true, |a| {
            a.authority_epoch != receipt.approved_epoch || a.authorized_director_session != receipt.approved_director
        });
        if let Some(next) = reconcile_adoption(receipt.state, snapshot.as_ref().map(|(s, r)| (s.as_str(), *r)), moved) {
            settled.push((receipt.adoption_id.clone(), next));
        }
    }
    if !stale.is_empty() || !reconcile.is_empty() || !gone.is_empty() || !settled.is_empty() {
        store.update(&mut |ext| {
            let mut changed = false;
            for id in &stale {
                changed |= mark_adoption(ext, id, AdoptionState::Stale).unwrap_or(false);
            }
            for id in &reconcile {
                changed |= mark_adoption(ext, id, AdoptionState::SubmissionUnknown).unwrap_or(false);
            }
            for (id, next) in &settled {
                changed |= match next {
                    AdoptionState::Stale => mark_adoption_refused_unadmitted(ext, id).unwrap_or(false),
                    _ => mark_adoption(ext, id, *next).unwrap_or(false),
                };
            }
            for key in &gone {
                changed |= mark_notice(ext, key, NoticeState::Superseded);
            }
            Ok(changed)
        })?;
    }
    Ok(out)
}

/// Bus state of the adopted message `message_id` (exact Management sender,
/// adopted control type) and whether the runtime may hold it.
fn repository_snapshot(message_id: &str) -> anyhow::Result<Option<(String, bool)>> {
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    Ok(repository
        .snapshot_by_external_message_id(message_id, START_ADOPTED_CONTROL_TYPE)?
        .map(|snapshot| {
            let reached = snapshot.may_have_reached_runtime();
            (snapshot.state, reached)
        }))
}

/// Next state of a sent / unknown adoption from its bus record: delivered
/// -> Delivered; a live record -> Submitted; a Submitted adoption whose bus
/// record was quarantined with no native evidence after the authority moved
/// (`authority_moved`) -> Stale, so the new Director re-reviews it (never an
/// automatic resend). Anything else is unchanged: a missing record, native
/// evidence, or an unknown submission is never "provably absent".
pub fn reconcile_adoption(current: AdoptionState, bus: Option<(&str, bool)>, authority_moved: bool) -> Option<AdoptionState> {
    let next = match bus {
        Some(("quarantined", false)) if current == AdoptionState::Submitted && authority_moved => AdoptionState::Stale,
        Some(("delivered", _)) => AdoptionState::Delivered,
        Some(("pending" | "inbox_accepted" | "stale_held", _)) => AdoptionState::Submitted,
        _ => return None,
    };
    (next != current).then_some(next)
}

/// Records the send outcome of one dispatch.
pub fn record_dispatch(kind: &DispatchKind, outcome: DispatchOutcome) -> anyhow::Result<()> {
    let store = default_store_or_file()?;
    store.update(&mut |ext| {
        Ok(match (kind, outcome) {
            (DispatchKind::AdoptedStart { adoption_id }, DispatchOutcome::Sent) => {
                mark_adoption(ext, adoption_id, AdoptionState::Submitted).map_err(anyhow::Error::msg)?
            }
            (DispatchKind::AdoptedStart { adoption_id }, DispatchOutcome::Unknown) => {
                mark_adoption(ext, adoption_id, AdoptionState::SubmissionUnknown).map_err(anyhow::Error::msg)?
            }
            (DispatchKind::Notice { key }, DispatchOutcome::Sent) => mark_notice(ext, key, NoticeState::Delivered),
            // A notice keeps its fixed message id: the bus deduplicates a resend.
            (DispatchKind::Notice { .. }, DispatchOutcome::Unknown) => false,
        })
    })
}

/// Admission of a Director-change notice (bus_bridge_external): a record
/// with this message id, for this recipient, still deliverable.
pub fn validate_director_notice(
    ext: &ManagementExtension,
    owner: &str,
    message_id: Option<&str>,
    roster: &AgentManagementSnapshot,
) -> Result<(), &'static str> {
    let message_id = message_id.ok_or("notice_message_id_missing")?;
    let notice = ext
        .notices
        .values()
        .find(|n| notice_external_id(&n.key) == message_id)
        .ok_or("notice_record_absent")?;
    if notice.recipient.as_str() != owner {
        return Err("notice_recipient_conflict");
    }
    if notice.state == NoticeState::Superseded || notice_text(notice, roster).is_none() {
        return Err("notice_not_deliverable");
    }
    // The canonical text was frozen at queue time: once ANOTHER Director
    // holds the seat it would name the wrong one, and the newer notice covers
    // it. A same-Director epoch refresh queues no notice, so it must not
    // suppress this one (compare sessions, not epochs).
    if roster.projects.get(&notice.project_id).map(|a| &a.authorized_director_session) != Some(&notice.new_director) {
        return Err("notice_superseded");
    }
    Ok(())
}

/// Periodic repair (bus process): settles deferred finalizes, handovers and
/// UI events after a transient failure, under the provider locks.
pub fn recover_now() -> anyhow::Result<()> {
    let provider = crate::agent_management::AgentManagementProvider::open_default()?;
    provider.recover_authority_hook()?;
    repair_summaries();
    // A start quarantined while no Director could review it (seat cleared, or
    // an unknown chain) gets its review once the authority moves.
    let roster = provider.store().snapshot()?;
    let store = default_store_or_file()?;
    let mut seen = scanned_authorities().lock().unwrap_or_else(|e| e.into_inner());
    rescan_if_authority_changed(&roster, &mut seen, store.as_ref(), quarantined_starts)?;
    Ok(())
}

fn scanned_authorities() -> &'static Mutex<Option<String>> {
    static SEEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(None))
}

/// Fingerprint of every project's (Director, epoch): the rescan guard.
fn authority_fingerprint(roster: &AgentManagementSnapshot) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (project, a) in &roster.projects {
        parts.push(format!("{}={}@{}", project.as_str(), a.authorized_director_session.as_str(), a.authority_epoch));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    digest(&refs)
}

/// Quarantined starts whose only recorded defect is provenance, with their
/// exact canonical envelope: `(recipient, message)`.
fn quarantined_starts() -> anyhow::Result<Vec<(String, crate::agent_bus::model::AgentBusMessage)>> {
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let mut out = Vec::new();
    for start in repository.management_start_messages()? {
        let code = start.error.as_ref().and_then(|e| e.get("code")).and_then(|c| c.as_str());
        if start.state != "quarantined" || code != Some("management_start_provenance_invalid") {
            continue;
        }
        out.push((start.to_cutex_session_id.clone(), repository.canonical_message(&start.message_id)?));
    }
    Ok(out)
}

/// Scans the quarantined starts only when some project's authority changed
/// since the last scan (`seen`), so an unchanged roster costs no journal
/// read. Review publication is idempotent and never revives a discarded
/// source; the sources stay quarantined, nothing is delivered. Returns the
/// number of new review items.
pub(crate) fn rescan_if_authority_changed(
    roster: &AgentManagementSnapshot,
    seen: &mut Option<String>,
    store: &dyn ExtensionStateStore,
    starts: impl FnOnce() -> anyhow::Result<Vec<(String, crate::agent_bus::model::AgentBusMessage)>>,
) -> anyhow::Result<usize> {
    let fingerprint = authority_fingerprint(roster);
    if seen.as_deref() == Some(fingerprint.as_str()) {
        return Ok(0);
    }
    let mut opened = 0;
    for (owner, message) in starts()? {
        if crate::app_server::bus_bridge::publish_director_changed_review(&owner, &message, roster, store)? {
            opened += 1;
        }
    }
    *seen = Some(fingerprint);
    Ok(opened)
}

/// Core repair of handover summaries / notices whose apply step failed after
/// the review committed (review #10), regardless of `handoverDispatched`.
/// Runs after the provider locks are released (ask store + extension only).
fn repair_summaries() {
    match super::repair_handover_summaries() {
        Ok(0) => {}
        Ok(repaired) => eprintln!("director-address: repaired {repaired} handover summary(ies)"),
        Err(error) => eprintln!("director-address handover summary repair deferred: {error:?}"),
    }
}

/// Daemon start (bus process): recovery under the provider lock, then the
/// one-time scan of quarantined starts that a Director change invalidated
/// (the ui-widgets shape), found by their real action / message keys.
pub fn startup() -> anyhow::Result<usize> {
    let provider = crate::agent_management::AgentManagementProvider::open_default()?;
    provider.recover_authority_hook()?;
    repair_summaries();
    let store = default_store_or_file()?;
    let roster = provider.store().snapshot()?;
    // Startup always scans once (`seen` starts empty), then only on change.
    let mut seen = scanned_authorities().lock().unwrap_or_else(|e| e.into_inner());
    *seen = None;
    rescan_if_authority_changed(&roster, &mut seen, store.as_ref(), quarantined_starts)
}
