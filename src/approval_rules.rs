//! Host-side automatic owner approvals (design 047).
//!
//! Rules live in the owner approval store so matching, the consumed approval
//! record and the audit row are committed by one private atomic write under
//! the same lock. No Agent Bus route exposes rule mutation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::approvals::{
    request_digest, ApprovalDecider, ApprovalKind, ApprovalRecord, ApprovalRequest, ApprovalRisk,
    ApprovalState, ApprovalStore, StoreFile,
};

const MAX_RULES: usize = 500;
const MAX_AUDIT: usize = 2_000;
const CONFIRMATION_MINUTES: i64 = 5;
const DIRECTOR_GRANTED_BY: &str = "role:director";
const DIRECTOR_KINDS: [ApprovalKind; 3] = [
    ApprovalKind::ServiceLifetime,
    ApprovalKind::AskDelivery,
    ApprovalKind::BrowserOpen,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleSubject {
    #[serde(rename = "type")]
    pub subject_type: String,
    pub cutex_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowConstraint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleConstraints {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub profile_ids: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowConstraint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryMode {
    Turn,
    Hour,
    Today,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnBinding {
    pub runtime_generation: u64,
    pub turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleExpiry {
    pub mode: ExpiryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnBinding>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRuleDraft {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub subject: RuleSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub kinds: BTreeSet<ApprovalKind>,
    #[serde(default)]
    pub constraints: RuleConstraints,
    pub expiry: RuleExpiry,
    #[serde(default)]
    pub high_risk_kinds_explicit: BTreeSet<ApprovalKind>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRule {
    pub rule_id: String,
    pub enabled: bool,
    pub subject: RuleSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub kinds: BTreeSet<ApprovalKind>,
    #[serde(default)]
    pub constraints: RuleConstraints,
    pub expiry: RuleExpiry,
    #[serde(default)]
    pub high_risk_kinds_explicit: BTreeSet<ApprovalKind>,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_matched_at: Option<String>,
    #[serde(default)]
    pub expired: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DirectorRuleOverride {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    disabled_kinds: BTreeSet<ApprovalKind>,
    #[serde(default)]
    revision: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectorGrant {
    session_id: String,
    display_name: Option<String>,
    project_id: String,
    granted_at: String,
}

impl ApprovalRule {
    pub fn draft(&self) -> ApprovalRuleDraft {
        ApprovalRuleDraft {
            enabled: self.enabled,
            subject: self.subject.clone(),
            project_id: self.project_id.clone(),
            kinds: self.kinds.clone(),
            constraints: self.constraints.clone(),
            expiry: self.expiry.clone(),
            high_risk_kinds_explicit: self.high_risk_kinds_explicit.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRuleAudit {
    pub audit_id: String,
    pub at: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_revision: Option<u64>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_cutex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ApprovalKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<ApprovalRisk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub result: String,
    #[serde(default)]
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfirmationToken {
    token: String,
    draft_sha256: String,
    expires_at: String,
    #[serde(default)]
    consumed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuleError {
    Invalid(String),
    Conflict(u64),
    NotFound,
    ConfirmationRequired(String),
    Store(String),
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Store(message) => f.write_str(message),
            Self::Conflict(revision) => write!(f, "rule revision changed to {revision}"),
            Self::NotFound => f.write_str("approval rule not found"),
            Self::ConfirmationRequired(phrase) => {
                write!(f, "typed confirmation required: {phrase}")
            }
        }
    }
}

fn always_high(kind: ApprovalKind) -> bool {
    matches!(
        kind,
        ApprovalKind::ManagedService
            | ApprovalKind::StaticRoot
            | ApprovalKind::BrowserClaim
            | ApprovalKind::WorkflowRun
    )
}

/// Kinds whose production request path carries an authenticated runtime/turn
/// identity. Ask delivery and workflows are durable/background operations,
/// so a composer turn cannot safely scope them.
fn turn_capable(kind: ApprovalKind) -> bool {
    matches!(
        kind,
        ApprovalKind::ManagedService
            | ApprovalKind::StaticRoot
            | ApprovalKind::ServiceLifetime
            | ApprovalKind::AgentProfile
            | ApprovalKind::BrowserOpen
            | ApprovalKind::BrowserClaim
    )
}

pub fn risk_for(request: &ApprovalRequest) -> ApprovalRisk {
    match request.kind {
        ApprovalKind::ManagedService
        | ApprovalKind::StaticRoot
        | ApprovalKind::BrowserClaim
        | ApprovalKind::WorkflowRun => ApprovalRisk::High,
        ApprovalKind::BrowserOpen => ApprovalRisk::Medium,
        ApprovalKind::ServiceLifetime => {
            let Some(details) = request.details.as_object() else {
                return ApprovalRisk::High;
            };
            // Production sends a descriptive `grants` string. Authority is
            // represented by these two fields; missing or malformed facts
            // fail closed because this is a composite approval.
            let Some(managed) = details.get("managedExecution").and_then(Value::as_bool) else {
                return ApprovalRisk::High;
            };
            let static_root = match details.get("staticRoot") {
                None | Some(Value::Null) => false,
                Some(Value::String(root)) if !root.is_empty() => true,
                Some(_) => return ApprovalRisk::High,
            };
            if managed || static_root {
                ApprovalRisk::High
            } else {
                ApprovalRisk::Medium
            }
        }
        ApprovalKind::AgentProfile => {
            if [
                "sensitive",
                "highCost",
                "profileSensitive",
                "profileHighCost",
            ]
            .iter()
            .any(|key| request.details[*key].as_bool() == Some(true))
            {
                ApprovalRisk::High
            } else {
                ApprovalRisk::Medium
            }
        }
        ApprovalKind::AskDelivery => {
            let Some(details) = request.details.as_object() else {
                return ApprovalRisk::High;
            };
            let Some(differences) = details.get("differences").and_then(Value::as_array) else {
                return ApprovalRisk::High;
            };
            let Some(versions) = details.get("versions").and_then(Value::as_object) else {
                return ApprovalRisk::High;
            };
            let known_snapshot = request.reason == "target_capabilities_not_subset"
                && versions.get("requester").and_then(Value::as_str).is_some()
                && versions.get("target").and_then(Value::as_str).is_some()
                && versions.get("requesterCaps").and_then(Value::as_str).is_some()
                && versions.get("targetCaps").and_then(Value::as_str).is_some()
                && versions.get("projectPolicy").and_then(Value::as_str).is_some();
            if !known_snapshot || !differences.is_empty() {
                // `reach` differences include cross-project authority; every
                // other difference is likewise beyond the caller envelope.
                ApprovalRisk::High
            } else {
                ApprovalRisk::Medium
            }
        }
    }
}

fn validate_draft(draft: &ApprovalRuleDraft) -> Result<(), RuleError> {
    if draft.subject.subject_type != "agent" {
        return Err(RuleError::Invalid("subject.type must be agent".into()));
    }
    if !draft.subject.cutex_session_id.starts_with("cutex.")
        || draft.subject.cutex_session_id.len() > 160
    {
        return Err(RuleError::Invalid("invalid subject cutexSessionId".into()));
    }
    if draft.kinds.is_empty() || draft.kinds.len() > 8 {
        return Err(RuleError::Invalid(
            "kinds must contain 1..=8 explicit kinds".into(),
        ));
    }
    if !draft.high_risk_kinds_explicit.is_subset(&draft.kinds) {
        return Err(RuleError::Invalid(
            "highRiskKindsExplicit must be a subset of kinds".into(),
        ));
    }
    if draft
        .kinds
        .iter()
        .any(|kind| always_high(*kind) && !draft.high_risk_kinds_explicit.contains(kind))
    {
        return Err(RuleError::Invalid(
            "every always-high-risk kind must be explicitly confirmed".into(),
        ));
    }
    if draft
        .project_id
        .as_deref()
        .is_some_and(|id| id.is_empty() || id.len() > 160)
    {
        return Err(RuleError::Invalid("invalid projectId".into()));
    }
    if draft.constraints.profile_ids.len() > 100
        || draft
            .constraints
            .profile_ids
            .iter()
            .any(|id| id.is_empty() || id.len() > 160)
    {
        return Err(RuleError::Invalid("invalid profileIds".into()));
    }
    match draft.expiry.mode {
        ExpiryMode::Turn => {
            let turn = draft.expiry.turn.as_ref().ok_or_else(|| {
                RuleError::Invalid("turn expiry needs runtimeGeneration and turnId".into())
            })?;
            if turn.runtime_generation == 0 || turn.turn_id.is_empty() || turn.turn_id.len() > 200 {
                return Err(RuleError::Invalid("invalid turn expiry binding".into()));
            }
            if draft.kinds.iter().any(|kind| !turn_capable(*kind)) {
                return Err(RuleError::Invalid(
                    "turn expiry is unavailable for ask_delivery and workflow_run".into(),
                ));
            }
        }
        _ if draft.expiry.turn.is_some() => {
            return Err(RuleError::Invalid(
                "turn binding is only valid for turn expiry".into(),
            ))
        }
        _ => {}
    }
    Ok(())
}

fn normalized_draft(
    mut draft: ApprovalRuleDraft,
    now: DateTime<Utc>,
) -> Result<ApprovalRuleDraft, RuleError> {
    validate_draft(&draft)?;
    draft.expiry.expires_at = match draft.expiry.mode {
        ExpiryMode::Turn => None,
        ExpiryMode::Hour => Some((now + Duration::hours(1)).to_rfc3339()),
        ExpiryMode::Today => {
            let local = now.with_timezone(&Local);
            let midnight = local
                .date_naive()
                .succ_opt()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .and_then(|naive| Local.from_local_datetime(&naive).earliest())
                .ok_or_else(|| RuleError::Invalid("host timezone has no next midnight".into()))?;
            Some(midnight.with_timezone(&Utc).to_rfc3339())
        }
        ExpiryMode::Revoked => None,
    };
    Ok(draft)
}

fn draft_digest(draft: &ApprovalRuleDraft) -> String {
    // Hour/today timestamps are chosen by the host separately for confirmation and mutation.
    // Bind the authority-bearing shape, not those two clock reads.
    let mut bound = draft.clone();
    if matches!(bound.expiry.mode, ExpiryMode::Hour | ExpiryMode::Today) {
        bound.expiry.expires_at = None;
    }
    request_digest(&serde_json::to_value(bound).expect("rule draft serializes"))
}

pub fn confirmation_phrase(draft: &ApprovalRuleDraft) -> String {
    let who = draft
        .subject
        .display_name
        .as_deref()
        .unwrap_or(&draft.subject.cutex_session_id);
    let mut kinds = draft
        .high_risk_kinds_explicit
        .iter()
        .map(|kind| {
            serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    kinds.sort();
    format!("AUTO {who} {}", kinds.join(","))
}

fn mark_expired(rule: &mut ApprovalRule, now: DateTime<Utc>) {
    if rule.expired {
        return;
    }
    if rule
        .expiry
        .expires_at
        .as_deref()
        .and_then(crate::approvals::timestamp)
        .is_some_and(|expires| expires <= now)
    {
        rule.expired = true;
        rule.enabled = false;
    }
}

fn append_audit(store: &mut StoreFile, row: ApprovalRuleAudit) {
    store.approval_rule_audit.push(row);
    if store.approval_rule_audit.len() > MAX_AUDIT {
        let remove = store.approval_rule_audit.len() - MAX_AUDIT;
        store.approval_rule_audit.drain(..remove);
    }
}

fn audit(action: &str, result: &str, now: DateTime<Utc>) -> ApprovalRuleAudit {
    ApprovalRuleAudit {
        audit_id: format!("ara-{}", uuid::Uuid::new_v4().simple()),
        at: now.to_rfc3339(),
        action: action.into(),
        rule_id: None,
        rule_revision: None,
        actor: "owner".into(),
        requester_cutex_session_id: None,
        project_id: None,
        kind: None,
        risk: None,
        request_sha256: None,
        approval_id: None,
        result: result.into(),
        details: Value::Null,
    }
}

fn consume_confirmation(
    store: &mut StoreFile,
    token: Option<&str>,
    draft: &ApprovalRuleDraft,
    now: DateTime<Utc>,
) -> Result<(), RuleError> {
    if draft.high_risk_kinds_explicit.is_empty() {
        return Ok(());
    }
    let phrase = confirmation_phrase(draft);
    let digest = draft_digest(draft);
    let Some(found) = token.and_then(|token| {
        store
            .approval_rule_confirmations
            .iter_mut()
            .find(|row| row.token == token)
    }) else {
        return Err(RuleError::ConfirmationRequired(phrase));
    };
    if found.consumed
        || found.draft_sha256 != digest
        || crate::approvals::timestamp(&found.expires_at).is_none_or(|at| at <= now)
    {
        return Err(RuleError::ConfirmationRequired(phrase));
    }
    found.consumed = true;
    Ok(())
}

fn optional_limit_expands(old: Option<u64>, new: Option<u64>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => new > old,
        (Some(_), None) => true,
        _ => false,
    }
}

fn constraints_expand(old: &RuleConstraints, new: &RuleConstraints) -> bool {
    let profiles_expand = !old.profile_ids.is_empty()
        && (new.profile_ids.is_empty() || !new.profile_ids.is_subset(&old.profile_ids));
    let workflow_expand = match (&old.workflow, &new.workflow) {
        (Some(_), None) => true,
        (Some(old), Some(new)) => {
            optional_limit_expands(old.max_agents, new.max_agents)
                || optional_limit_expands(old.max_tokens, new.max_tokens)
        }
        _ => false,
    };
    profiles_expand || workflow_expand
}

fn expiry_expands(old: &RuleExpiry, new: &RuleExpiry) -> bool {
    match (old.mode, new.mode) {
        (ExpiryMode::Revoked, _) => false,
        (_, ExpiryMode::Revoked) => true,
        (ExpiryMode::Turn, ExpiryMode::Turn) => old.turn != new.turn,
        (ExpiryMode::Turn, _) => true,
        (_, ExpiryMode::Turn) => false,
        _ => {
            let old_at = old
                .expires_at
                .as_deref()
                .and_then(crate::approvals::timestamp);
            let new_at = new
                .expires_at
                .as_deref()
                .and_then(crate::approvals::timestamp);
            matches!((old_at, new_at), (Some(old), Some(new)) if new > old)
        }
    }
}

fn approval_kind_name(kind: ApprovalKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn director_rule_id(session_id: &str, project_id: &str, kind: ApprovalKind) -> String {
    let digest = Sha256::digest(format!("{session_id}\0{project_id}\0{}", approval_kind_name(kind)));
    format!(
        "aar-role-director-{}-{}",
        approval_kind_name(kind).replace('_', "-"),
        &format!("{digest:x}")[..16]
    )
}

fn directors_from_snapshot(
    snapshot: &crate::agent_management::AgentManagementSnapshot,
) -> Vec<DirectorGrant> {
    use crate::agent_management::ProjectLifecycle;
    let mut grants = snapshot
        .projects
        .values()
        .filter(|authority| {
            snapshot
                .project_states
                .get(&authority.project_id)
                .is_none_or(|state| state.lifecycle == ProjectLifecycle::Active)
        })
        .filter_map(|authority| {
            let agent = snapshot.agents.get(&authority.authorized_director_session)?;
            if agent.retired_at.is_some()
                || snapshot
                    .reversible_archive_projection
                    .get(&authority.authorized_director_session)
                    .copied()
                    .unwrap_or(false)
            {
                return None;
            }
            Some(DirectorGrant {
                session_id: authority.authorized_director_session.as_str().to_owned(),
                display_name: Some(agent.spec.name.clone()),
                project_id: authority.project_id.as_str().to_owned(),
                granted_at: authority.updated_at.as_str().to_owned(),
            })
        })
        .collect::<Vec<_>>();
    grants.sort_by(|left, right| {
        (&left.project_id, &left.session_id).cmp(&(&right.project_id, &right.session_id))
    });
    grants
}

fn current_directors() -> Result<Vec<DirectorGrant>, RuleError> {
    let provider = crate::agent_management::AgentManagementProvider::open_default()
        .map_err(|error| RuleError::Store(format!("{error:#}")))?;
    let snapshot = provider
        .current_name_snapshot()
        .map_err(|error| RuleError::Store(error.to_string()))?;
    Ok(directors_from_snapshot(&snapshot))
}

pub(crate) fn current_director_for(request: &ApprovalRequest) -> Option<DirectorGrant> {
    let project_id = request.project_id.as_deref()?;
    current_directors().ok()?.into_iter().find(|grant| {
        grant.session_id == request.requester_cutex_session_id && grant.project_id == project_id
    })
}

fn director_rule(
    grant: &DirectorGrant,
    kind: ApprovalKind,
    override_row: Option<&DirectorRuleOverride>,
) -> ApprovalRule {
    let enabled = !override_row.is_some_and(|row| row.disabled_kinds.contains(&kind));
    let updated_at = override_row
        .filter(|row| !row.updated_at.is_empty())
        .map(|row| row.updated_at.clone())
        .unwrap_or_else(|| grant.granted_at.clone());
    ApprovalRule {
        rule_id: director_rule_id(&grant.session_id, &grant.project_id, kind),
        enabled,
        subject: RuleSubject {
            subject_type: "agent".into(),
            cutex_session_id: grant.session_id.clone(),
            display_name: grant.display_name.clone(),
        },
        project_id: Some(grant.project_id.clone()),
        kinds: [kind].into_iter().collect(),
        constraints: RuleConstraints::default(),
        expiry: RuleExpiry {
            mode: ExpiryMode::Revoked,
            expires_at: None,
            turn: None,
        },
        high_risk_kinds_explicit: BTreeSet::new(),
        revision: override_row.map_or(1, |row| row.revision.max(1)),
        created_at: grant.granted_at.clone(),
        updated_at,
        created_by: "system".into(),
        source: Some("role".into()),
        granted_by: Some(DIRECTOR_GRANTED_BY.into()),
        last_matched_at: None,
        expired: false,
    }
}

fn role_rule_matches_request(
    store: &StoreFile,
    grant: &DirectorGrant,
    request: &ApprovalRequest,
    risk: ApprovalRisk,
) -> Option<ApprovalRule> {
    if request.project_id.as_deref() != Some(grant.project_id.as_str())
        || request.requester_cutex_session_id != grant.session_id
        || !DIRECTOR_KINDS.contains(&request.kind)
        || risk == ApprovalRisk::High
    {
        return None;
    }
    let rule = director_rule(
        grant,
        request.kind,
        store.director_rule_overrides.get(&grant.session_id),
    );
    rule.enabled.then_some(rule)
}

fn set_director_rule_enabled_in(
    store: &mut StoreFile,
    grant: &DirectorGrant,
    kind: ApprovalKind,
    expected_revision: u64,
    enabled: bool,
    now: DateTime<Utc>,
    submitted: Option<&ApprovalRuleDraft>,
) -> Result<(u64, ApprovalRule, bool), RuleError> {
    if store.rules_revision != expected_revision {
        return Err(RuleError::Conflict(store.rules_revision));
    }
    let before = director_rule(
        grant,
        kind,
        store.director_rule_overrides.get(&grant.session_id),
    );
    if let Some(submitted) = submitted {
        let mut expected = before.draft();
        expected.enabled = enabled;
        if submitted != &expected {
            return Err(RuleError::Invalid(
                "role-granted rules only allow changing enabled".into(),
            ));
        }
    }
    if before.enabled == enabled {
        return Ok((store.rules_revision, before, false));
    }
    store.rules_revision += 1;
    let row = store
        .director_rule_overrides
        .entry(grant.session_id.clone())
        .or_default();
    if enabled {
        row.disabled_kinds.remove(&kind);
    } else {
        row.disabled_kinds.insert(kind);
    }
    row.revision = before.revision.saturating_add(1);
    row.updated_at = now.to_rfc3339();
    let rule = director_rule(grant, kind, Some(row));
    let mut audit = audit(
        "updated",
        if enabled { "role_enabled" } else { "role_disabled" },
        now,
    );
    audit.rule_id = Some(rule.rule_id.clone());
    audit.rule_revision = Some(rule.revision);
    audit.requester_cutex_session_id = Some(grant.session_id.clone());
    audit.project_id = Some(grant.project_id.clone());
    audit.kind = Some(kind);
    audit.details = json!({"grantedBy":DIRECTOR_GRANTED_BY,"enabled":enabled});
    append_audit(store, audit);
    Ok((store.rules_revision, rule, true))
}

impl ApprovalStore {
    pub fn rule_catalog() -> Value {
        let rows = [
            (ApprovalKind::ManagedService, "high", false),
            (ApprovalKind::ServiceLifetime, "dynamic", true),
            (ApprovalKind::StaticRoot, "high", false),
            (ApprovalKind::AgentProfile, "dynamic", true),
            (ApprovalKind::AskDelivery, "dynamic", true),
            (ApprovalKind::BrowserClaim, "high", false),
            (ApprovalKind::BrowserOpen, "medium", false),
            (ApprovalKind::WorkflowRun, "high", false),
        ];
        json!(rows
            .into_iter()
            .map(|(kind, risk, composite)| json!({
                "kind": kind, "risk": risk, "compositeRisk": composite,
                "constraints": match kind {
                    ApprovalKind::AgentProfile => json!(["profileIds"]),
                    ApprovalKind::WorkflowRun => json!(["maxAgents", "maxTokens"]),
                    _ => json!([]),
                }
            }))
            .collect::<Vec<_>>())
    }

    pub fn confirm_rule(
        &self,
        draft: ApprovalRuleDraft,
        typed_phrase: &str,
        now: DateTime<Utc>,
    ) -> Result<(String, String), RuleError> {
        let draft = normalized_draft(draft, now)?;
        let phrase = confirmation_phrase(&draft);
        if draft.high_risk_kinds_explicit.is_empty() || typed_phrase != phrase {
            return Err(RuleError::ConfirmationRequired(phrase));
        }
        self.with_store(|store| {
            store.approval_rule_confirmations.retain(|row| {
                !row.consumed
                    && crate::approvals::timestamp(&row.expires_at).is_some_and(|at| at > now)
            });
            let token = format!("arc-{}", uuid::Uuid::new_v4().simple());
            store.approval_rule_confirmations.push(ConfirmationToken {
                token: token.clone(),
                draft_sha256: draft_digest(&draft),
                expires_at: (now + Duration::minutes(CONFIRMATION_MINUTES)).to_rfc3339(),
                consumed: false,
            });
            (token, phrase)
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))
    }

    pub fn list_rules(&self, now: DateTime<Utc>) -> Result<(u64, Vec<ApprovalRule>), RuleError> {
        let grants = current_directors()?;
        let snapshot = self
            .rule_list_snapshot()
            .map_err(|error| RuleError::Store(format!("{error:#}")))?;
        let mut rules = snapshot.rules;
        let expires = rules.iter_mut().any(|rule| {
            let before = rule.expired;
            mark_expired(rule, now);
            before != rule.expired
        });
        if !expires {
            for grant in &grants {
                for kind in DIRECTOR_KINDS {
                    rules.push(director_rule(
                        grant,
                        kind,
                        snapshot.director_overrides.get(&grant.session_id),
                    ));
                }
            }
            return Ok((snapshot.revision, rules));
        }
        // Persist newly expired flags exactly as before, under the write lock
        // and against the latest file (a concurrent owner mutation may win).
        self.with_store(|store| {
            for rule in &mut store.approval_rules {
                mark_expired(rule, now);
            }
            let mut rules = store.approval_rules.clone();
            for grant in &grants {
                for kind in DIRECTOR_KINDS {
                    rules.push(director_rule(
                        grant,
                        kind,
                        store.director_rule_overrides.get(&grant.session_id),
                    ));
                }
            }
            (store.rules_revision, rules)
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))
    }

    pub fn create_rule(
        &self,
        expected_revision: u64,
        draft: ApprovalRuleDraft,
        token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(u64, ApprovalRule), RuleError> {
        let draft = normalized_draft(draft, now)?;
        self.with_store(|store| {
            if store.rules_revision != expected_revision {
                return Err(RuleError::Conflict(store.rules_revision));
            }
            if store.approval_rules.len() >= MAX_RULES {
                return Err(RuleError::Invalid("approval rule capacity reached".into()));
            }
            consume_confirmation(store, token, &draft, now)?;
            store.rules_revision += 1;
            let rule = ApprovalRule {
                rule_id: format!("aar-{}", uuid::Uuid::new_v4().simple()),
                enabled: draft.enabled,
                subject: draft.subject,
                project_id: draft.project_id,
                kinds: draft.kinds,
                constraints: draft.constraints,
                expiry: draft.expiry,
                high_risk_kinds_explicit: draft.high_risk_kinds_explicit,
                revision: 1,
                created_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                created_by: "owner".into(),
                source: None,
                granted_by: None,
                last_matched_at: None,
                expired: false,
            };
            let mut row = audit("created", "allowed", now);
            row.rule_id = Some(rule.rule_id.clone());
            row.rule_revision = Some(rule.revision);
            append_audit(store, row);
            store.approval_rules.push(rule.clone());
            Ok((store.rules_revision, rule))
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))?
    }

    pub fn replace_rule(
        &self,
        id: &str,
        expected_revision: u64,
        draft: ApprovalRuleDraft,
        token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(u64, ApprovalRule), RuleError> {
        if id.starts_with("aar-role-director-") {
            return self.replace_director_rule(id, expected_revision, draft, now);
        }
        let mut draft = normalized_draft(draft, now)?;
        self.with_store(|store| {
            if store.rules_revision != expected_revision {
                return Err(RuleError::Conflict(store.rules_revision));
            }
            let index = store
                .approval_rules
                .iter()
                .position(|rule| rule.rule_id == id)
                .ok_or(RuleError::NotFound)?;
            let old = store.approval_rules[index].clone();
            let authority_expands = (!old.enabled && draft.enabled)
                || draft.subject != old.subject
                || !draft.kinds.is_subset(&old.kinds)
                || draft.project_id != old.project_id
                || constraints_expand(&old.constraints, &draft.constraints)
                || !draft
                    .high_risk_kinds_explicit
                    .is_subset(&old.high_risk_kinds_explicit);
            // Removing authority must not renew hour/today rules just because
            // normalized_draft computes deadlines at mutation time. Preserve only
            // the deadline: a changed turn binding must still count as expansion.
            // Re-enabling, scope changes and widening still normalize afresh.
            if !authority_expands && draft.expiry.mode == old.expiry.mode {
                draft.expiry.expires_at = old.expiry.expires_at.clone();
            }
            let expands = authority_expands || expiry_expands(&old.expiry, &draft.expiry);
            if expands && !draft.high_risk_kinds_explicit.is_empty() {
                consume_confirmation(store, token, &draft, now)?;
            }
            store.rules_revision += 1;
            let rule = ApprovalRule {
                rule_id: old.rule_id,
                enabled: draft.enabled,
                subject: draft.subject,
                project_id: draft.project_id,
                kinds: draft.kinds,
                constraints: draft.constraints,
                expiry: draft.expiry,
                high_risk_kinds_explicit: draft.high_risk_kinds_explicit,
                revision: old.revision + 1,
                created_at: old.created_at,
                updated_at: now.to_rfc3339(),
                created_by: old.created_by,
                source: old.source,
                granted_by: old.granted_by,
                last_matched_at: old.last_matched_at,
                expired: false,
            };
            store.approval_rules[index] = rule.clone();
            let mut row = audit(
                "updated",
                if expands { "expanded" } else { "narrowed" },
                now,
            );
            row.rule_id = Some(rule.rule_id.clone());
            row.rule_revision = Some(rule.revision);
            append_audit(store, row);
            Ok((store.rules_revision, rule))
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))?
    }

    fn replace_director_rule(
        &self,
        id: &str,
        expected_revision: u64,
        draft: ApprovalRuleDraft,
        now: DateTime<Utc>,
    ) -> Result<(u64, ApprovalRule), RuleError> {
        let normalized = normalized_draft(draft, now)?;
        let (revision, rule, _) = self.set_director_rule_enabled(
            id,
            expected_revision,
            normalized.enabled,
            now,
            Some(&normalized),
        )?;
        Ok((revision, rule))
    }

    fn set_director_rule_enabled(
        &self,
        id: &str,
        expected_revision: u64,
        enabled: bool,
        now: DateTime<Utc>,
        submitted: Option<&ApprovalRuleDraft>,
    ) -> Result<(u64, ApprovalRule, bool), RuleError> {
        let target = current_directors()?.into_iter().find_map(|grant| {
            DIRECTOR_KINDS
                .into_iter()
                .find(|kind| director_rule_id(&grant.session_id, &grant.project_id, *kind) == id)
                .map(|kind| (grant, kind))
        });
        let Some((grant, kind)) = target else {
            return Err(RuleError::NotFound);
        };
        self.with_store(|store| {
            set_director_rule_enabled_in(
                store,
                &grant,
                kind,
                expected_revision,
                enabled,
                now,
                submitted,
            )
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))?
    }

    pub fn delete_rule(
        &self,
        id: &str,
        expected_revision: u64,
        now: DateTime<Utc>,
    ) -> Result<(u64, bool), RuleError> {
        if id.starts_with("aar-role-director-") {
            let (revision, _, changed) =
                self.set_director_rule_enabled(id, expected_revision, false, now, None)?;
            return Ok((revision, changed));
        }
        self.with_store(|store| {
            let Some(index) = store.approval_rules.iter().position(|rule| rule.rule_id == id) else {
                // A retry after an uncertain response observes the completed revoke.
                return Ok((store.rules_revision, false));
            };
            if store.rules_revision != expected_revision {
                return Err(RuleError::Conflict(store.rules_revision));
            }
            store.approval_rules.remove(index);
            store.rules_revision += 1;
            let mut row = audit("revoked", "revoked", now);
            row.rule_id = Some(id.into());
            append_audit(store, row);
            Ok((store.rules_revision, true))
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))?
    }

    pub fn rule_audit(
        &self,
        cursor: usize,
        limit: usize,
    ) -> Result<(Vec<ApprovalRuleAudit>, Option<usize>), RuleError> {
        self.with_store(|store| {
            let rows = store
                .approval_rule_audit
                .iter()
                .rev()
                .skip(cursor)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            let next = (cursor + rows.len() < store.approval_rule_audit.len())
                .then_some(cursor + rows.len());
            (rows, next)
        })
        .map_err(|error| RuleError::Store(format!("{error:#}")))
    }
}

fn constraint_match(rule: &ApprovalRule, request: &ApprovalRequest) -> bool {
    if !rule.constraints.profile_ids.is_empty()
        && request.details["profile"]
            .as_str()
            .is_none_or(|id| !rule.constraints.profile_ids.contains(id))
    {
        return false;
    }
    if let Some(limit) = &rule.constraints.workflow {
        if limit.max_agents.is_some_and(|max| {
            request
                .details
                .get("maxAgents")
                .and_then(Value::as_u64)
                .is_none_or(|value| value > max)
        }) {
            return false;
        }
        if limit.max_tokens.is_some_and(|max| {
            request
                .details
                .get("maxTokens")
                .and_then(Value::as_u64)
                .is_none_or(|value| value > max)
        }) {
            return false;
        }
    }
    true
}

fn matches(
    rule: &ApprovalRule,
    request: &ApprovalRequest,
    risk: ApprovalRisk,
    now: DateTime<Utc>,
) -> bool {
    if !rule.enabled
        || rule.expired
        || rule.subject.cutex_session_id != request.requester_cutex_session_id
        || !rule.kinds.contains(&request.kind)
    {
        return false;
    }
    if rule
        .project_id
        .as_deref()
        .is_some_and(|project| request.project_id.as_deref() != Some(project))
    {
        return false;
    }
    if risk == ApprovalRisk::High && !rule.high_risk_kinds_explicit.contains(&request.kind) {
        return false;
    }
    if rule
        .expiry
        .expires_at
        .as_deref()
        .and_then(crate::approvals::timestamp)
        .is_some_and(|at| at <= now)
    {
        return false;
    }
    if let Some(turn) = &rule.expiry.turn {
        if request.details["runtimeGeneration"].as_u64() != Some(turn.runtime_generation)
            || request.details["turnId"].as_str() != Some(turn.turn_id.as_str())
        {
            return false;
        }
    }
    constraint_match(rule, request)
}

/// Called only while the approval store lock is held.
pub(crate) fn auto_decide(
    store: &mut StoreFile,
    request: &ApprovalRequest,
    now: DateTime<Utc>,
    consume: bool,
    director_grant: Option<&DirectorGrant>,
) -> Option<ApprovalRecord> {
    for rule in &mut store.approval_rules {
        mark_expired(rule, now);
    }
    let risk = risk_for(request);
    let index = store
        .approval_rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| matches(rule, request, risk, now))
        .max_by_key(|(_, rule)| {
            (
                rule.project_id.is_some(),
                !rule.constraints.profile_ids.is_empty(),
                rule.revision,
            )
        })
        .map(|(index, _)| index);
    let (rule_id, rule_revision, audit_actor, constraints, expiry_mode, decision_by, note, granted_by) =
        if let Some(index) = index {
            let rule = &mut store.approval_rules[index];
            rule.last_matched_at = Some(now.to_rfc3339());
            (
                rule.rule_id.clone(),
                rule.revision,
                format!("auto:{}", rule.created_by),
                serde_json::to_value(&rule.constraints).unwrap_or(Value::Null),
                serde_json::to_value(rule.expiry.mode).unwrap_or(Value::Null),
                format!("auto:rule/{}", rule.rule_id),
                "Automatically approved by an owner Cutex rule".to_string(),
                Value::Null,
            )
        } else {
            let rule = role_rule_matches_request(store, director_grant?, request, risk)?;
            (
                rule.rule_id.clone(),
                rule.revision,
                format!("auto:{DIRECTOR_GRANTED_BY}"),
                serde_json::to_value(&rule.constraints).unwrap_or(Value::Null),
                serde_json::to_value(rule.expiry.mode).unwrap_or(Value::Null),
                "auto:role/director".into(),
                "Automatically approved by the Director role".into(),
                json!(DIRECTOR_GRANTED_BY),
            )
        };
    // A browser claim may skip the generic yes/no decision, but only an authenticated owner
    // selecting a concrete tab may consume it. Keep that actionable request in the pending list.
    let owner_must_select_tab = request.kind == ApprovalKind::BrowserClaim;
    let state = if owner_must_select_tab {
        ApprovalState::Pending
    } else if consume {
        ApprovalState::Consumed
    } else {
        ApprovalState::Approved
    };
    let approval_id = format!("apr-{}", uuid::Uuid::new_v4().simple());
    let record = ApprovalRecord {
        approval_id: approval_id.clone(),
        kind: request.kind,
        reason: request.reason.clone(),
        requester_cutex_session_id: request.requester_cutex_session_id.clone(),
        requester_name: request.requester_name.clone(),
        project_id: request.project_id.clone(),
        summary: request.summary.clone(),
        details: request.details.clone(),
        request_sha256: request.request_sha256.clone(),
        state,
        created_at: now.to_rfc3339(),
        decided_at: Some(now.to_rfc3339()),
        note: Some(note),
        decided_by: Some(ApprovalDecider {
            by: decision_by,
            connection: None,
            peer: None,
            client: BTreeMap::new(),
        }),
        consumed_at: (consume && !owner_must_select_tab).then(|| now.to_rfc3339()),
        wait: None,
        risk: Some(risk),
        matched_rule_id: Some(rule_id.clone()),
        auto_decision: true,
        rule_revision: Some(rule_revision),
    };
    store.approvals.push(record.clone());
    append_audit(
        store,
        ApprovalRuleAudit {
            audit_id: format!("ara-{}", uuid::Uuid::new_v4().simple()),
            at: now.to_rfc3339(),
            action: "matched".into(),
            rule_id: Some(rule_id),
            rule_revision: Some(rule_revision),
            actor: audit_actor,
            requester_cutex_session_id: Some(request.requester_cutex_session_id.clone()),
            project_id: request.project_id.clone(),
            kind: Some(request.kind),
            risk: Some(risk),
            request_sha256: Some(request.request_sha256.clone()),
            approval_id: Some(approval_id),
            result: "allowed".into(),
            details: json!({"constraints": constraints, "expiryMode": expiry_mode, "state": state, "grantedBy": granted_by}),
        },
    );
    Some(record)
}

pub fn has_rollback_blockers(path: &std::path::Path) -> anyhow::Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_slice(&bytes).context("approval store is corrupt")?;
    Ok(value["rulesRevision"].as_u64().unwrap_or(0) > 0
        || value["approvalRules"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
        || value["approvalRuleAudit"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        || value["approvalRuleConfirmations"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        || value["directorRuleOverrides"]
            .as_object()
            .is_some_and(|rows| !rows.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: ApprovalKind, mode: ExpiryMode) -> ApprovalRuleDraft {
        ApprovalRuleDraft {
            enabled: true,
            subject: RuleSubject {
                subject_type: "agent".into(),
                cutex_session_id: "cutex.agent".into(),
                display_name: Some("Eva".into()),
            },
            project_id: Some("desktop".into()),
            kinds: [kind].into_iter().collect(),
            constraints: RuleConstraints::default(),
            expiry: RuleExpiry {
                mode,
                expires_at: None,
                turn: None,
            },
            high_risk_kinds_explicit: always_high(kind).then_some(kind).into_iter().collect(),
        }
    }

    fn request(kind: ApprovalKind, details: Value) -> ApprovalRequest {
        ApprovalRequest {
            kind,
            reason: "test".into(),
            requester_cutex_session_id: "cutex.agent".into(),
            requester_name: Some("Eva".into()),
            project_id: Some("desktop".into()),
            summary: "test".into(),
            details,
            request_sha256: "sha".into(),
        }
    }

    fn legacy_rule_list(store: &ApprovalStore, now: DateTime<Utc>) -> (u64, Vec<ApprovalRule>) {
        let grants = current_directors().unwrap();
        store
            .with_store(|data| {
                for rule in &mut data.approval_rules {
                    mark_expired(rule, now);
                }
                let mut rules = data.approval_rules.clone();
                for grant in &grants {
                    for kind in DIRECTOR_KINDS {
                        rules.push(director_rule(
                            grant,
                            kind,
                            data.director_rule_overrides.get(&grant.session_id),
                        ));
                    }
                }
                (data.rules_revision, rules)
            })
            .unwrap()
    }

    fn large_history_fixture(rows: usize) -> (ApprovalStore, std::path::PathBuf, DateTime<Utc>) {
        let dir =
            std::env::temp_dir().join(format!("approval-rules-large-{}", uuid::Uuid::new_v4()));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        for (revision, kind) in [
            ApprovalKind::BrowserOpen,
            ApprovalKind::ServiceLifetime,
            ApprovalKind::AskDelivery,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .create_rule(
                    revision as u64,
                    draft(
                        kind,
                        if revision == 0 {
                            ExpiryMode::Hour
                        } else {
                            ExpiryMode::Revoked
                        },
                    ),
                    None,
                    now,
                )
                .unwrap();
        }
        store.with_store(|data| {
            data.approval_rule_audit = (0..rows).map(|index| serde_json::from_value(json!({
                "auditId": format!("synthetic-{index}"), "at": now.to_rfc3339(), "action": "matched",
                "actor": "fixture", "result": "auto_approved", "details": { "synthetic": "x".repeat(512) }
            })).unwrap()).collect();
        }).unwrap();
        (store, dir, now)
    }

    #[test]
    fn large_history_rule_lists_match_legacy_and_persist_new_expirations() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let (store, dir, now) = large_history_fixture(10_000);
        let before = std::fs::read(&store.path).unwrap();
        let expected = legacy_rule_list(&store, now);
        assert_eq!(store.list_rules(now).unwrap(), expected);
        assert_eq!(
            ApprovalStore::at(&store.path).list_rules(now).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read(&store.path).unwrap(),
            before,
            "a fresh list never rewrites history"
        );
        let reference = ApprovalStore::at(dir.join("reference.json"));
        std::fs::write(&reference.path, &before).unwrap();
        let later = now + Duration::hours(2);
        assert_eq!(
            store.list_rules(later).unwrap(),
            legacy_rule_list(&reference, later)
        );
        let after: StoreFile =
            serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
        assert_eq!(after.rules_revision, 3);
        assert!(after.approval_rules[0].expired && !after.approval_rules[0].enabled);
        assert_eq!(after.approval_rule_audit.len(), 10_000);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_rule_list_never_authorizes_a_gate_after_an_external_disable() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let (store, dir, now) = large_history_fixture(10);
        assert!(store.list_rules(now).unwrap().1[0].enabled);
        ApprovalStore::at(&store.path)
            .with_store(|data| {
                data.approval_rules[0].enabled = false;
                data.rules_revision += 1;
            })
            .unwrap();
        assert!(matches!(
            store
                .gate(
                    request(
                        ApprovalKind::BrowserOpen,
                        json!({"url":"https://docs.example.test"})
                    ),
                    now
                )
                .unwrap(),
            crate::approvals::GateOutcome::Pending { .. }
        ));
        let (revision, rules) = store.list_rules(now).unwrap();
        assert_eq!(revision, 4);
        assert!(!rules[0].enabled);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Run alone with --ignored --nocapture --test-threads=1. Both implementations
    /// use the same process, synthetic store, clock, directors and build profile.
    #[test]
    #[ignore = "synthetic before/after timing probe"]
    fn benchmark_rule_list_large_history() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        for count in [0, 1_000, 10_000] {
            let (store, dir, now) = large_history_fixture(count);
            let before = std::fs::read(&store.path).unwrap();
            let mut old = Vec::new();
            let mut fast = Vec::new();
            let mut cold_ms = 0.0;
            for round in 0..12 {
                let start = std::time::Instant::now();
                let expected = legacy_rule_list(&store, now);
                old.push(start.elapsed().as_secs_f64() * 1000.0);
                let start = std::time::Instant::now();
                let actual = store.list_rules(now).unwrap();
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                if round == 0 {
                    cold_ms = elapsed;
                } else {
                    fast.push(elapsed);
                }
                assert_eq!(actual, expected);
            }
            assert_eq!(std::fs::read(&store.path).unwrap(), before);
            old.sort_by(f64::total_cmp);
            fast.sort_by(f64::total_cmp);
            println!(
                "{}",
                json!({"auditRows":count,"storeBytes":before.len(),"legacyMedianMs":(old[5]+old[6])/2.0,
                "firstReadMs":cold_ms,"warmMedianMs":fast[5],"warmMaxMs":fast.last(),"samples":12})
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn high_risk_needs_bound_one_use_confirmation_and_audits_match() {
        let dir = std::env::temp_dir().join(format!("approval-rules-{}", uuid::Uuid::new_v4()));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        let high_draft = draft(ApprovalKind::ManagedService, ExpiryMode::Revoked);
        assert!(matches!(
            store.create_rule(0, high_draft.clone(), None, now),
            Err(RuleError::ConfirmationRequired(_))
        ));
        let phrase = confirmation_phrase(&high_draft);
        let (token, _) = store
            .confirm_rule(high_draft.clone(), &phrase, now)
            .unwrap();
        store.create_rule(0, high_draft, Some(&token), now).unwrap();
        let result = store
            .gate(request(ApprovalKind::ManagedService, json!({})), now)
            .unwrap();
        let crate::approvals::GateOutcome::Approved(record) = result else {
            panic!()
        };
        assert!(record.auto_decision);
        assert_eq!(record.state, ApprovalState::Consumed);
        assert!(record.decided_by.unwrap().by.starts_with("auto:rule/aar-"));
        assert_eq!(
            store.rule_audit(0, 10).unwrap().0[0]
                .request_sha256
                .as_deref(),
            Some("sha")
        );
        assert!(matches!(
            store.create_rule(
                1,
                draft(ApprovalKind::ManagedService, ExpiryMode::Revoked),
                Some(&token),
                now
            ),
            Err(RuleError::ConfirmationRequired(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn project_constraints_dynamic_risk_and_expiry_fail_closed() {
        use crate::services::agent_api::{lifetime_approval_request, AgentServicePolicy};
        use crate::services::static_files::CheckedRoot;
        use crate::services::store::Caller;

        let dir = std::env::temp_dir().join(format!("approval-rules-{}", uuid::Uuid::new_v4()));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        store
            .create_rule(
                0,
                draft(ApprovalKind::ServiceLifetime, ExpiryMode::Hour),
                None,
                now,
            )
            .unwrap();
        let policy = AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: None,
            requester_name: Some("Eva".into()),
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        };
        let caller = Caller::Agent {
            cutex_session_id: "cutex.agent".into(),
            project_id: Some("desktop".into()),
        };
        let simple = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "simple",
            &json!({"runtime":"external"}),
            false,
            None,
        )
        .unwrap();
        assert_eq!(risk_for(&simple), ApprovalRisk::Medium);
        assert!(matches!(
            store.gate(simple.clone(), now),
            Ok(crate::approvals::GateOutcome::Approved(_))
        ));
        let high = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "managed",
            &json!({"runtime":"managed","command":"serve {port}"}),
            true,
            None,
        )
        .unwrap();
        assert_eq!(risk_for(&high), ApprovalRisk::High);
        assert!(matches!(
            store.gate(high, now),
            Ok(crate::approvals::GateOutcome::Pending { .. })
        ));
        let root = CheckedRoot {
            path: if cfg!(windows) { r"C:\outside" } else { "/outside" }.into(),
            needs_approval: true,
            allowed_roots: vec![],
        };
        let static_root = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "static",
            &json!({"runtime":"static"}),
            false,
            Some(&root),
        )
        .unwrap();
        assert_eq!(risk_for(&static_root), ApprovalRisk::High);
        let mut malformed = simple.clone();
        malformed.details.as_object_mut().unwrap().remove("managedExecution");
        assert_eq!(risk_for(&malformed), ApprovalRisk::High);
        let wrong = ApprovalRequest {
            project_id: Some("other".into()),
            request_sha256: "other".into(),
            ..simple
        };
        assert!(matches!(
            store.gate(wrong, now),
            Ok(crate::approvals::GateOutcome::Pending { .. })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_profile_and_workflow_constraints_use_real_fields() {
        use crate::agent_management::ManagedAgentSpec;
        use crate::workflows::engine::{approval_request as workflow_request, ProfileFact};
        use crate::workflows::model::{Author, Limits, RunRecord, RunState, StepCounts};

        let dir = std::env::temp_dir().join(format!(
            "approval-rules-constraints-{}",
            uuid::Uuid::new_v4()
        ));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();

        let mut profile_draft = draft(ApprovalKind::AgentProfile, ExpiryMode::Revoked);
        profile_draft.constraints.profile_ids.insert("profile-a".into());
        let (_, profile_rule) = store.create_rule(0, profile_draft, None, now).unwrap();
        let cwd = if cfg!(windows) { r"C:\work" } else { "/work" };
        let spec: ManagedAgentSpec = serde_json::from_value(json!({
            "name":"child","cwd":cwd,"profile":"profile-a","runtime_backend":"native",
            "model":"gpt","reasoning":"high","permissions":"workspace-write",
            "approval_policy":"on-request","sandbox_mode":"workspace-write","groups":["workers"]
        }))
        .unwrap();
        let profile_request = crate::agent_bus::server::agent_profile_approval_request(
            "cutex.agent",
            Some("Eva".into()),
            Some("desktop".into()),
            "create",
            &spec,
            "profile-a",
            "profile-sha".into(),
        );
        assert_eq!(profile_request.details["profile"], "profile-a");
        assert!(constraint_match(&profile_rule, &profile_request));
        let mut other_profile = profile_request.clone();
        other_profile.details["profile"] = json!("profile-b");
        assert!(!constraint_match(&profile_rule, &other_profile));

        let mut workflow_draft = draft(ApprovalKind::WorkflowRun, ExpiryMode::Revoked);
        workflow_draft.constraints.workflow = Some(WorkflowConstraint {
            max_agents: Some(10),
            max_tokens: Some(2_000),
        });
        let (token, _) = store
            .confirm_rule(
                workflow_draft.clone(),
                &confirmation_phrase(&workflow_draft),
                now,
            )
            .unwrap();
        let (_, workflow_rule) = store
            .create_rule(1, workflow_draft, Some(&token), now)
            .unwrap();
        let run = RunRecord {
            run_id: "run-1".into(),
            seq: 1,
            name: "review".into(),
            description: "Review the change".into(),
            phases: vec![],
            author: Author::Agent {
                cutex_session_id: "cutex.agent".into(),
                name: Some("Eva".into()),
            },
            project_id: Some("desktop".into()),
            cwd: cwd.into(),
            script_sha256: "0".repeat(64),
            args: Value::Null,
            args_sha256: "1".repeat(64),
            limits: Limits {
                max_agents: 8,
                max_concurrency: 4,
                max_tokens: 1_500,
                hard_stop: false,
            },
            default_profile: Some("profile-a".into()),
            step_profiles: vec!["profile-a".into()],
            models: vec![],
            approval_id: None,
            state: RunState::PendingApproval,
            stop_reason: None,
            error: None,
            current_phase: None,
            counts: StepCounts::default(),
            tokens: 0,
            result: None,
            resumed_from: None,
            draft_path: None,
            warnings: vec![],
            created_at: now.to_rfc3339(),
            started_at: None,
            ended_at: None,
            start_action_id: None,
            notified: false,
            revision: 0,
        };
        let workflow_request = workflow_request(
            &run,
            &[ProfileFact {
                name: "profile-a".into(),
                runtime: "codex".into(),
            }],
        );
        assert_eq!(workflow_request.details["maxAgents"], 8);
        assert_eq!(workflow_request.details["maxTokens"], 1_500);
        assert!(constraint_match(&workflow_rule, &workflow_request));
        let mut too_large = workflow_request;
        too_large.details["maxAgents"] = json!(11);
        assert!(!constraint_match(&workflow_rule, &too_large));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn turn_rules_use_production_turn_identity_and_reject_background_kinds() {
        use crate::browser_relay::approval::{approval_request, Context};
        use crate::browser_relay::auth::AgentContext;
        use crate::browser_relay::commands::AgentCommand;
        use crate::services::agent_api::{lifetime_approval_request, AgentServicePolicy};
        use crate::services::store::Caller;

        let now = Utc::now();
        let mut browser_draft = draft(ApprovalKind::BrowserOpen, ExpiryMode::Turn);
        browser_draft.project_id = None;
        browser_draft.expiry.turn = Some(TurnBinding {
            runtime_generation: 7,
            turn_id: "turn-7".into(),
        });
        validate_draft(&browser_draft).unwrap();
        let rule = ApprovalRule {
            rule_id: "aar-test".into(),
            enabled: true,
            subject: browser_draft.subject.clone(),
            project_id: None,
            kinds: browser_draft.kinds.clone(),
            constraints: RuleConstraints::default(),
            expiry: browser_draft.expiry.clone(),
            high_risk_kinds_explicit: BTreeSet::new(),
            revision: 1,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            created_by: "owner".into(),
            source: None,
            granted_by: None,
            last_matched_at: None,
            expired: false,
        };
        let agent = AgentContext {
            session: "cutex.agent".into(),
            runtime: "runtime".into(),
            thread: "thread-7".into(),
            generation: 7,
            turn: "turn-7".into(),
            credential_identity: "fixture".into(),
        };
        let context = Context {
            agent: &agent,
            agent_name: "Eva",
            host: "local",
            device: "desktop",
            generation: "connection",
            full_access: false,
        };
        let input: AgentCommand = serde_json::from_value(json!({
            "requestId":"request-1","tool":"browser_open",
            "arguments":{"title":"Docs","url":"https://example.com"}
        }))
        .unwrap();
        let browser = approval_request(&context, &input);
        assert_eq!(browser.details["turnId"], "turn-7");
        assert!(matches(&rule, &browser, risk_for(&browser), now));

        for kind in [ApprovalKind::AskDelivery, ApprovalKind::WorkflowRun] {
            let mut unsupported = draft(kind, ExpiryMode::Turn);
            unsupported.expiry.turn = browser_draft.expiry.turn.clone();
            assert!(matches!(
                validate_draft(&unsupported),
                Err(RuleError::Invalid(_))
            ));
        }

        let dir = std::env::temp_dir().join(format!(
            "approval-rules-turn-service-{}",
            uuid::Uuid::new_v4()
        ));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let mut service_draft = draft(ApprovalKind::ServiceLifetime, ExpiryMode::Turn);
        service_draft.expiry.turn = browser_draft.expiry.turn.clone();
        store.create_rule(0, service_draft, None, now).unwrap();
        let policy = AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: None,
            requester_name: Some("Eva".into()),
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        };
        let caller = Caller::Agent {
            cutex_session_id: "cutex.agent".into(),
            project_id: Some("desktop".into()),
        };
        let service = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "svc",
            &json!({"runtime":"external"}),
            false,
            None,
        )
        .unwrap();
        let waiter = crate::approvals::Waiter::new(
            0,
            Some("thread-7".into()),
            Some("turn-7".into()),
            Some(7),
            || true,
            || true,
        );
        assert!(matches!(
            crate::approvals::gate_with_wait(&store, service, Some(&waiter), |_| {}, |_| {}),
            Ok(crate::approvals::GateOutcome::Approved(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_narrow_of_multitype_clock_rules_keeps_deadline_without_confirmation() {
        // Today crosses the host's midnight: narrowing must not revive yesterday's grant.
        let now = Local.with_ymd_and_hms(2026, 10, 20, 23, 50, 0).earliest().unwrap().with_timezone(&Utc);
        let later = now + Duration::minutes(20);
        for mode in [ExpiryMode::Hour, ExpiryMode::Today] {
            for change in ["kind", "highRisk", "constraints", "unchanged"] {
                let dir = std::env::temp_dir().join(format!("approval-exact-narrow-{}", uuid::Uuid::new_v4()));
                let store = ApprovalStore::at(dir.join("approvals.json"));
                let mut original = draft(ApprovalKind::ManagedService, mode);
                original.kinds.insert(ApprovalKind::ServiceLifetime);
                original.high_risk_kinds_explicit.insert(ApprovalKind::ServiceLifetime);
                let (token, _) = store.confirm_rule(original.clone(), &confirmation_phrase(&original), now).unwrap();
                let (_, created) = store.create_rule(0, original.clone(), Some(&token), now).unwrap();
                let mut narrowed = original;
                match change {
                    "kind" => { narrowed.kinds.remove(&ApprovalKind::ServiceLifetime); narrowed.high_risk_kinds_explicit.remove(&ApprovalKind::ServiceLifetime); }
                    "highRisk" => { narrowed.high_risk_kinds_explicit.remove(&ApprovalKind::ServiceLifetime); }
                    "constraints" => { narrowed.constraints.profile_ids.insert("safe-profile".into()); }
                    _ => {}
                }
                // A client deadline cannot change what the host preserves.
                narrowed.expiry.expires_at = Some((later + Duration::days(10)).to_rfc3339());
                let (revision, updated) = store.replace_rule(&created.rule_id, 1, narrowed, None, later).unwrap();
                assert_eq!(revision, 2);
                assert_eq!(updated.expiry, created.expiry, "{mode:?}/{change}");
                assert!(updated.enabled);
                assert!(!updated.high_risk_kinds_explicit.is_empty());
                let audit = store.rule_audit(0, 1).unwrap().0.pop().unwrap();
                assert_eq!(audit.action, "updated");
                assert_eq!(audit.result, "narrowed");
                let saved = store.with_store(|data| data.approval_rules[0].clone()).unwrap();
                assert_eq!(saved.expiry, created.expiry);
                if mode == ExpiryMode::Today {
                    let mut expired = updated;
                    mark_expired(&mut expired, later);
                    assert!(expired.expired, "a narrowed rule cannot revive after midnight");
                }
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn clock_rule_widening_still_confirms_and_renews() {
        let now = Local.with_ymd_and_hms(2026, 10, 20, 23, 50, 0).earliest().unwrap().with_timezone(&Utc);
        let later = now + Duration::minutes(20);
        for mode in [ExpiryMode::Hour, ExpiryMode::Today] {
            for change in ["kind", "highRisk", "scope", "constraints"] {
                let dir = std::env::temp_dir().join(format!("approval-clock-expand-{}", uuid::Uuid::new_v4()));
                let store = ApprovalStore::at(dir.join("approvals.json"));
                let mut original = draft(ApprovalKind::ManagedService, mode);
                original.kinds.insert(ApprovalKind::ServiceLifetime);
                original.constraints.profile_ids.insert("safe-profile".into());
                let (token, _) = store.confirm_rule(original.clone(), &confirmation_phrase(&original), now).unwrap();
                let (_, created) = store.create_rule(0, original.clone(), Some(&token), now).unwrap();
                let mut widened = original;
                match change {
                    "kind" => { widened.kinds.insert(ApprovalKind::BrowserClaim); widened.high_risk_kinds_explicit.insert(ApprovalKind::BrowserClaim); }
                    "highRisk" => { widened.high_risk_kinds_explicit.insert(ApprovalKind::ServiceLifetime); }
                    "scope" => { widened.project_id = None; }
                    "constraints" => { widened.constraints.profile_ids.clear(); }
                    _ => unreachable!()
                }
                assert!(matches!(store.replace_rule(&created.rule_id, 1, widened.clone(), None, later), Err(RuleError::ConfirmationRequired(_))), "{mode:?}/{change}");
                assert_eq!(store.with_store(|data| data.rules_revision).unwrap(), 1);
                let (token, _) = store.confirm_rule(widened.clone(), &confirmation_phrase(&widened), later).unwrap();
                let (_, updated) = store.replace_rule(&created.rule_id, 1, widened, Some(&token), later).unwrap();
                assert!(crate::approvals::timestamp(updated.expiry.expires_at.as_deref().unwrap()).unwrap()
                    > crate::approvals::timestamp(created.expiry.expires_at.as_deref().unwrap()).unwrap());
                assert_eq!(store.rule_audit(0, 1).unwrap().0[0].result, "expanded");
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn clock_expiry_confirmation_survives_second_clock_read_and_extension_reconfirms() {
        let dir =
            std::env::temp_dir().join(format!("approval-rules-clock-{}", uuid::Uuid::new_v4()));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        let high = draft(ApprovalKind::ManagedService, ExpiryMode::Hour);
        let (token, _) = store
            .confirm_rule(high.clone(), &confirmation_phrase(&high), now)
            .unwrap();
        let (_, created) = store
            .create_rule(0, high.clone(), Some(&token), now + Duration::seconds(2))
            .unwrap();
        let mut paused = high.clone();
        paused.enabled = false;
        let (_, paused) = store
            .replace_rule(
                &created.rule_id,
                1,
                paused,
                None,
                now + Duration::minutes(5),
            )
            .unwrap();
        assert_eq!(paused.expiry.expires_at, created.expiry.expires_at);
        assert!(matches!(
            store.replace_rule(
                &created.rule_id,
                2,
                high.clone(),
                None,
                now + Duration::minutes(5)
            ),
            Err(RuleError::ConfirmationRequired(_))
        ));
        let later = now + Duration::minutes(5);
        let (token, _) = store
            .confirm_rule(high.clone(), &confirmation_phrase(&high), later)
            .unwrap();
        store
            .replace_rule(
                &created.rule_id,
                2,
                high,
                Some(&token),
                later + Duration::seconds(2),
            )
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn browser_claim_rule_never_selects_or_consumes_a_tab() {
        let dir =
            std::env::temp_dir().join(format!("approval-rules-claim-{}", uuid::Uuid::new_v4()));
        let store = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        let high = draft(ApprovalKind::BrowserClaim, ExpiryMode::Revoked);
        let (token, _) = store
            .confirm_rule(high.clone(), &confirmation_phrase(&high), now)
            .unwrap();
        store.create_rule(0, high, Some(&token), now).unwrap();
        let claim = request(
            ApprovalKind::BrowserClaim,
            json!({"hostId":"local","deviceId":"desktop-local","connectionGeneration":"g","threadId":"thread-1","runtimeGeneration":1}),
        );
        let first = store.request_browser_claim(claim.clone(), now).unwrap();
        assert_eq!(first.state, ApprovalState::Pending);
        assert!(first.auto_decision);
        assert!(first.consumed_at.is_none());
        assert_eq!(
            store.request_browser_claim(claim, now).unwrap().approval_id,
            first.approval_id
        );
        assert!(store
            .consume_browser_claim(
                &first.approval_id,
                "cutex.agent",
                "local",
                "desktop-local",
                "g",
                "wrong-thread",
                1,
                now
            )
            .unwrap()
            .is_none());
        let consumed = store
            .consume_browser_claim(
                &first.approval_id,
                "cutex.agent",
                "local",
                "desktop-local",
                "g",
                "thread-1",
                1,
                now,
            )
            .unwrap()
            .unwrap();
        assert_eq!(consumed.state, ApprovalState::Consumed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn director_role_defaults_match_only_medium_production_facts_and_audit_the_role() {
        use crate::services::agent_api::{lifetime_approval_request, AgentServicePolicy};
        use crate::services::store::Caller;

        let now = Utc::now();
        let grant = DirectorGrant {
            session_id: "cutex.agent".into(),
            display_name: Some("Eva".into()),
            project_id: "desktop".into(),
            granted_at: now.to_rfc3339(),
        };
        let policy = AgentServicePolicy {
            boundary: None,
            managed_within_envelope: false,
            approvals: None,
            requester_name: Some("Eva".into()),
            waiter: None,
            director_default_project: false,
            project_ambiguous: false,
            revalidate_authority: false,
        };
        let caller = Caller::Agent {
            cutex_session_id: "cutex.agent".into(),
            project_id: Some("desktop".into()),
        };
        let medium = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "external",
            &json!({"runtime":"external"}),
            false,
            None,
        )
        .unwrap();
        let high = lifetime_approval_request(
            &policy,
            &caller,
            "register",
            "managed",
            &json!({"runtime":"managed","command":"serve {port}"}),
            true,
            None,
        )
        .unwrap();
        let mut store = StoreFile::default();
        assert!(auto_decide(&mut store, &medium, now, true, None).is_none());
        assert!(store.approvals.is_empty());
        assert!(auto_decide(&mut store, &high, now, true, Some(&grant)).is_none());
        let approved = auto_decide(&mut store, &medium, now, true, Some(&grant)).unwrap();
        assert_eq!(approved.decided_by.unwrap().by, "auto:role/director");
        assert_eq!(approved.risk, Some(ApprovalRisk::Medium));
        assert_eq!(store.approval_rule_audit.last().unwrap().actor, "auto:role:director");

        let (revision, disabled, changed) = set_director_rule_enabled_in(
            &mut store,
            &grant,
            ApprovalKind::ServiceLifetime,
            0,
            false,
            now,
            None,
        )
        .unwrap();
        assert!(changed);
        assert!(!disabled.enabled);
        assert!(role_rule_matches_request(
            &store,
            &grant,
            &medium,
            ApprovalRisk::Medium
        )
        .is_none());
        let persisted = serde_json::to_vec(&store).unwrap();
        let mut reopened: StoreFile = serde_json::from_slice(&persisted).unwrap();
        assert!(reopened.director_rule_overrides["cutex.agent"]
            .disabled_kinds
            .contains(&ApprovalKind::ServiceLifetime));
        let (_, enabled, changed) = set_director_rule_enabled_in(
            &mut reopened,
            &grant,
            ApprovalKind::ServiceLifetime,
            revision,
            true,
            now + Duration::seconds(1),
            None,
        )
        .unwrap();
        assert!(changed);
        assert!(enabled.enabled);
        assert!(reopened.director_rule_overrides["cutex.agent"]
            .disabled_kinds
            .is_empty());
    }

    #[test]
    fn director_role_projection_disappears_on_archive_demotion_or_project_archive() {
        use crate::agent_management::{
            AgentManagementSnapshot, ManagedAgentRecord, ManagedAgentSpec, ProjectAuthority,
            ProjectId, ProjectLifecycle, ProjectStateRecord,
        };
        use crate::role_revision::{CutexSessionId, Rfc3339};

        let at = Rfc3339::new("2026-10-24T00:00:00Z").unwrap();
        let project = ProjectId::new("desktop").unwrap();
        let session = CutexSessionId::new("cutex.agent").unwrap();
        let spec: ManagedAgentSpec = serde_json::from_value(json!({
            "name":"Eva","cwd":if cfg!(windows) { r"C:\work" } else { "/work" },
            "profile":"default","runtime_backend":"host","model":"gpt",
            "reasoning":"high","permissions":"default","approval_policy":"never",
            "sandbox_mode":"danger-full-access","groups":[]
        }))
        .unwrap();
        let mut snapshot = AgentManagementSnapshot::empty();
        snapshot.agents.insert(
            session.clone(),
            ManagedAgentRecord {
                project_id: Some(project.clone()),
                created_by_director_session: None,
                created_by_operator_session: None,
                cutex_session_id: session.clone(),
                native_session_id: "native-eva".into(),
                spec,
                created_at: at.clone(),
                retired_at: None,
            },
        );
        snapshot.projects.insert(
            project.clone(),
            ProjectAuthority {
                project_id: project.clone(),
                authorized_director_session: session.clone(),
                authority_epoch: 1,
                updated_at: at.clone(),
            },
        );
        assert_eq!(directors_from_snapshot(&snapshot).len(), 1);

        snapshot.reversible_archive_projection.insert(session.clone(), true);
        assert!(directors_from_snapshot(&snapshot).is_empty());
        snapshot.reversible_archive_projection.insert(session.clone(), false);
        snapshot.project_states.insert(
            project.clone(),
            ProjectStateRecord {
                project_id: project.clone(),
                lifecycle: ProjectLifecycle::Archived,
                revision: 2,
                created_at: at.clone(),
                updated_at: at.clone(),
                archived_at: Some(at.clone()),
                workspace_root: None,
                last_director_epoch: None,
            },
        );
        assert!(directors_from_snapshot(&snapshot).is_empty());
        snapshot.project_states.get_mut(&project).unwrap().lifecycle = ProjectLifecycle::Active;
        snapshot.projects.clear();
        assert!(directors_from_snapshot(&snapshot).is_empty());
    }
}
