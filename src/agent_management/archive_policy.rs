//! Shared archive projection and permanent-retirement confirmation policy.
//! No lifecycle writes, runtime calls, or authority inferred from wire fields.
use super::*;

/// Existing data without its store lock is unavailable, not an empty authority.
/// The generic store has an empty-new-install fallback; lifecycle gates must
/// distinguish that fallback from partially missing durable files.
pub fn archive_authority_snapshot() -> Result<AgentManagementSnapshot, AgentManagementError> {
    let store = super::super::AgentManagementStore::open_default()
        .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    let has_data=store.root().join("agent-management-v1.json").try_exists()
        .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    let has_lock=store.root().join("agent-management-v1.lock").try_exists()
        .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
    if has_data && !has_lock { return Err(AgentManagementError::InvalidStore); }
    store.snapshot()
}

/// Authority failure is never permission to start/send/restore. Keep this
/// separate from record.is_retired(): the session projection may lag a tombstone.
pub fn ensure_agent_not_permanently_retired(record: &CutexSessionRecord) -> Result<(), AgentManagementError> {
    let state = archive_authority_snapshot()?;
    ensure_agent_not_permanently_retired_in(&state, record)?;
    agent_archive_disposition(&state, record).map(|_| ())
}

pub fn ensure_agent_not_permanently_retired_in(state: &AgentManagementSnapshot, record: &CutexSessionRecord) -> Result<(), AgentManagementError> {
    let id = CutexSessionId::new(record.cutex_session_id.clone()).map_err(|_| AgentManagementError::InvalidStore)?;
    if is_permanently_retired(state, &id) { return Err(AgentManagementError::Conflict("permanent_retirement_is_not_runnable")); }
    Ok(())
}

/// Boolean fail-closed gate for existing runtime/routing predicates.
pub fn agent_execution_blocked(record: &CutexSessionRecord) -> bool {
    record.is_retired() || ensure_agent_not_permanently_retired(record).is_err()
}

/// The same fail-closed gate against one successfully loaded authority view.
/// Callers must reject unavailable authority, never substitute an empty view.
pub fn agent_execution_blocked_in(
    state: &AgentManagementSnapshot,
    record: &CutexSessionRecord,
) -> bool {
    record.is_retired()
        || ensure_agent_not_permanently_retired_in(state, record).is_err()
        || agent_archive_disposition(state, record).is_err()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveKind {
    Active,
    Soft,
    Permanent,
}

/// `restorable` describes the archive kind, not a promise that Restore will
/// succeed now (name collisions and runtime fences remain execute-time checks).
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentArchiveDisposition {
    pub archive_kind: AgentArchiveKind,
    pub restorable: bool,
}

impl AgentArchiveDisposition {
    fn from_flags(session_retired: bool, permanently_retired: bool) -> Self {
        let archive_kind = if permanently_retired {
            AgentArchiveKind::Permanent
        } else if session_retired {
            AgentArchiveKind::Soft
        } else {
            AgentArchiveKind::Active
        };
        Self {
            archive_kind,
            restorable: archive_kind == AgentArchiveKind::Soft,
        }
    }
}

/// Call with a successfully loaded authority snapshot, never a default
/// snapshot substituted for a read failure. The permanent roster tombstone
/// takes precedence even over a stale/active session projection.
pub fn agent_archive_disposition(
    state: &AgentManagementSnapshot,
    record: &CutexSessionRecord,
) -> Result<AgentArchiveDisposition, AgentManagementError> {
    let id = CutexSessionId::new(record.cutex_session_id.clone())
        .map_err(|_| AgentManagementError::InvalidStore)?;
    if !is_permanently_retired(state, &id) && legacy_archive_ambiguous(state, record) {
        return Err(AgentManagementError::Conflict(
            "legacy_close_requires_owner_review",
        ));
    }
    Ok(AgentArchiveDisposition::from_flags(
        record.is_retired(),
        is_permanently_retired(state, &id),
    ))
}

/// A pre-r264 Close could commit the session before writing its permanent
/// roster tombstone. Never infer reversibility from that missing tombstone.
pub(super) fn legacy_archive_ambiguous(
    state: &AgentManagementSnapshot,
    record: &CutexSessionRecord,
) -> bool {
    if !record.is_retired() {
        return false;
    }
    let proven_soft = state.agent_archive_actions.values().any(|r| {
        r.stage == AgentArchiveStage::Committed
            && r.request.review.operation == AgentArchiveOperation::Archive
            && r.result.as_ref().is_some_and(|saved| {
                saved.cutex_session_id == record.cutex_session_id
                    && saved.durable_revision() == record.durable_revision()
            })
    });
    if proven_soft {
        return false;
    }
    state.phase_events.values().any(|event| {
        let target_matches = event
            .subject_cutex_session_id
            .as_ref()
            .is_some_and(|id| id.as_str() == record.cutex_session_id)
            || event
                .predecessor_cutex_session_id
                .as_ref()
                .is_some_and(|id| id.as_str() == record.cutex_session_id);
        target_matches
            && state.actions.get(&event.action_id).is_some_and(|action| {
                action.operation == AgentOperationKind::Close
                    && action.phase != AgentActionPhase::NoWrite
                    || action.operation == AgentOperationKind::Replace
                        && matches!(
                            action.phase,
                            AgentActionPhase::PredecessorClosing
                                | AgentActionPhase::PredecessorClosed
                        )
            })
    })
}

/// Owner-only destructive entry must call this under its execution/mutation
/// locks against the current formal name, before stopping or changing state.
/// The principal comes from the authenticated owner boundary, never a request.
/// Pasted names are valid: only exact content matters, not input provenance.
pub fn confirm_permanent_agent_retirement(
    _principal: &HumanManagementPrincipal,
    current_formal_name: &str,
    confirmed_name: &str,
) -> Result<(), AgentManagementError> {
    validate_permanent_retirement_name(current_formal_name, confirmed_name)
}

pub(super) fn validate_permanent_retirement_name(
    current_formal_name: &str,
    confirmed_name: &str,
) -> Result<(), AgentManagementError> {
    if current_formal_name.is_empty() || current_formal_name != confirmed_name {
        return Err(AgentManagementError::Conflict(
            "permanent_retirement_name_confirmation_required",
        ));
    }
    Ok(())
}

pub(super) fn is_permanently_retired(state: &AgentManagementSnapshot, id: &CutexSessionId) -> bool {
    state
        .agents
        .get(id)
        .is_some_and(|agent| agent.retired_at.is_some())
        || state.agent_archive_audit.values().any(|receipt| {
            receipt.request.review.cutex_session_id == *id
                && receipt.request.review.operation == AgentArchiveOperation::PermanentRetire
                && matches!(
                    receipt.stage,
                    AgentArchiveStage::PermanentRetirementCommitted | AgentArchiveStage::Committed
                )
        })
}

// Reviews are informational and may change between retries. Target and
// operation are not: a historical receipt cannot authorize another target.
pub(super) fn validate_archive_replay_target(
    request: &AgentArchiveRequest,
    receipt: &AgentArchiveReceipt,
) -> Result<(), AgentManagementError> {
    if request.review.cutex_session_id != receipt.request.review.cutex_session_id
        || request.review.operation != receipt.request.review.operation
        || (request.review.operation == AgentArchiveOperation::PermanentRetire
            && request.review.formal_name != receipt.request.review.formal_name)
        || (request
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("managed-request:"))
            && request.reason != receipt.request.reason)
    {
        return Err(AgentManagementError::Conflict(
            "archive_action_target_conflict",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_gate_with_snapshot_rejects_retired_and_malformed_identity() {
        let state = AgentManagementSnapshot::empty();
        let mut record = CutexSessionRecord::new_at(
            "cutex.archive-policy-test".into(),
            Some("archive-policy-native".into()),
            "host-a".into(),
            "/tmp".into(),
            None,
            "2026-10-27T00:00:00Z".into(),
        ).unwrap();
        assert!(!agent_execution_blocked_in(&state, &record));
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        assert!(agent_execution_blocked_in(&state, &record));
        record.archive_state = crate::session::model::CutexSessionArchiveState::Active;
        record.cutex_session_id.clear();
        assert!(agent_execution_blocked_in(&state, &record));
    }

    #[test]
    fn permanent_history_wins_over_every_session_state() {
        for session_retired in [false, true] {
            let value = AgentArchiveDisposition::from_flags(session_retired, true);
            assert_eq!(value.archive_kind, AgentArchiveKind::Permanent);
            assert!(!value.restorable);
        }
    }

    #[test]
    fn only_soft_archives_are_restorable_and_wire_fields_are_camel_case() {
        assert_eq!(
            serde_json::to_value(AgentArchiveDisposition::from_flags(true, false)).unwrap(),
            serde_json::json!({"archiveKind":"soft", "restorable":true})
        );
        assert_eq!(
            serde_json::to_value(AgentArchiveDisposition::from_flags(false, false)).unwrap(),
            serde_json::json!({"archiveKind":"active", "restorable":false})
        );
    }

    #[test]
    fn confirmation_accepts_exact_pasted_unicode_name_and_rejects_stale_names() {
        let owner = HumanManagementPrincipal::authenticated();
        assert!(confirm_permanent_agent_retirement(&owner, "嘉豪 🦅", "嘉豪 🦅").is_ok());
        for (current, supplied) in [
            ("", ""),
            ("Kestrel", "kestrel"),
            ("Kestrel", " Kestrel"),
            ("Renamed", "Kestrel"),
        ] {
            assert!(confirm_permanent_agent_retirement(&owner, current, supplied).is_err());
        }
    }
}

/// Deployment preflight can inspect this raw extension without decoding new enums.
/// r263 rejects both new receipt fields and permanent operation/stage variants.
pub(super) fn mark_archive_reader_requirement(state: &mut AgentManagementSnapshot) {
    state.extra.insert("archive_storage_contract_v1".into(), serde_json::json!({
        "minimum_reader_release":"r264", "binary_only_rollback_blocked_to":["r263"],
        "reason":"archive_receipt_fields_and_permanent_enum_variants"
    }));
}
