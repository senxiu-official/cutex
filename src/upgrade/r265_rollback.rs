//! r264 archive reader pinned to 8dba5103, alongside the unchanged r263 policy.
//! Both readers preserve the existing agent_bus v7 contract and shared safety gates.

use super::r264_rollback::object;
use anyhow::ensure;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Reader {
    R263,
    R264,
}

impl Reader {
    /// Exact deployment labels only. None leaves the existing legacy target
    /// parser in charge; it is not acceptance of any other target.
    pub(super) fn for_target(target: &str) -> Option<Self> {
        match target {
            "r263" => Some(Self::R263),
            "r264" => Some(Self::R264),
            _ => None,
        }
    }
}

pub(super) fn preflight_r264(home: &Path) -> anyhow::Result<()> {
    super::older_stores::preflight_preserved_stores(home, Reader::R264)
}

/// Closed writer contract, with only explicitly known optional members.
fn fields(value: &Value, required: &[&str], optional: &[&str]) -> anyhow::Result<()> {
    let value = object(value)?;
    ensure!(
        required.iter().all(|key| value.contains_key(*key))
            && value
                .keys()
                .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str())),
        "unknown/missing r264 archive contract fields"
    );
    Ok(())
}

fn disposition(value: &Value) -> anyhow::Result<()> {
    if let Some(kind) = value.get("archiveKind") {
        ensure!(
            kind.is_null() || matches!(kind.as_str(), Some("active" | "soft" | "permanent")),
            "unknown r264 archive kind"
        );
    }
    if let Some(restorable) = value.get("restorable") {
        ensure!(
            restorable.is_null() || restorable.is_boolean(),
            "invalid r264 restorable"
        );
    }
    // These are independently optional in the real r264 reader. Do not invent
    // a lifecycle consistency requirement that its historic records lack.
    Ok(())
}

pub(super) fn archive_marker(state: &Value) -> anyhow::Result<()> {
    for (key, marker) in object(state)? {
        if key.starts_with("archive_storage_contract_") {
            ensure!(
                key == "archive_storage_contract_v1",
                "unknown archive storage marker"
            );
            ensure!(
                *marker
                    == serde_json::json!({
                        "minimum_reader_release":"r264",
                        "binary_only_rollback_blocked_to":["r263"],
                        "reason":"archive_receipt_fields_and_permanent_enum_variants"
                    }),
                "unknown r264 archive storage marker contract"
            );
        }
    }
    Ok(())
}

pub(super) fn archive_receipt(value: &Value) -> anyhow::Result<()> {
    fields(
        value,
        &[
            "request",
            "request_sha256",
            "stage",
            "result",
            "error",
            "committed_at",
        ],
        &["archiveKind", "restorable"],
    )?;
    ensure!(
        matches!(
            value["stage"].as_str(),
            Some(
                "prepared"
                    | "stopped"
                    | "committed"
                    | "stopped_not_archived"
                    | "uncertain"
                    | "permanent_retirement_committed"
            )
        ),
        "unknown r264 archive stage"
    );
    let request = &value["request"];
    fields(request, &["action_id", "review"], &["reason"])?;
    let review = &request["review"];
    // Historical writers persisted these two digests before reviews became
    // informational. Accept that known history, not arbitrary extensions.
    fields(
        review,
        &["cutex_session_id", "formal_name", "operation"],
        &[
            "current_project_id",
            "revision",
            "runtime_generation",
            "durable_sha256",
            "authority_sha256",
        ],
    )?;
    for field in ["durable_sha256", "authority_sha256"] {
        if let Some(digest) = review.get(field) {
            serde_json::from_value::<crate::role_revision::Sha256>(digest.clone())?;
        }
    }
    ensure!(
        matches!(
            review["operation"].as_str(),
            Some("archive" | "restore" | "permanent_retire")
        ),
        "unknown r264 archive operation"
    );
    disposition(value)?;
    // Supplement closed raw-field/enum checks with IDs, hashes, time and typed
    // result validation. No repository open or serialization back to disk.
    serde_json::from_value::<crate::agent_management::AgentArchiveReceipt>(value.clone())?;
    Ok(())
}

pub(super) fn management_receipt(value: &Value) -> anyhow::Result<()> {
    fields(
        value,
        &[
            "schema",
            "action_id",
            "request_sha256",
            "operation",
            "project_id",
            "completed_at",
            "result",
        ],
        &["archiveKind", "restorable"],
    )?;
    ensure!(
        value["schema"] == "cutex/agent-management-receipt/v1",
        "unknown management receipt schema"
    );
    // Permanent retirement belongs to AgentArchiveOperation, not this enum.
    ensure!(
        matches!(
            value["operation"].as_str(),
            Some(
                "create"
                    | "query_managed"
                    | "online"
                    | "offline"
                    | "restart"
                    | "close"
                    | "replace"
                    | "grant_operator"
                    | "revoke_operator"
                    | "director_rotate"
            )
        ),
        "unknown r264 management operation"
    );
    let result = &value["result"];
    match result["kind"].as_str() {
        Some("created") => fields(result, &["kind", "agent", "observation"], &["message_id"]),
        Some("query_managed") => fields(result, &["kind", "authority", "agents"], &["operators"]),
        Some("lifecycle") => fields(result, &["kind", "agent", "observation"], &[]),
        Some("replaced") => fields(
            result,
            &[
                "kind",
                "predecessor_cutex_session_id",
                "successor",
                "observation",
            ],
            &["message_id"],
        ),
        Some("operator_granted") => fields(result, &["kind", "grant", "roster"], &[]),
        Some("operator_revoked") => fields(
            result,
            &["kind", "operator_cutex_session_id", "roster"],
            &[],
        ),
        Some("director_rotated") => fields(
            result,
            &[
                "kind",
                "predecessor_cutex_session_id",
                "successor",
                "observation",
                "authority",
            ],
            &["message_id"],
        ),
        _ => anyhow::bail!("unknown r264 management result"),
    }?;
    disposition(value)?;
    serde_json::from_value::<crate::agent_management::AgentManagementReceipt>(value.clone())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::older_stores;
    include!("r265_rollback_tests.rs");

    #[test]
    fn exact_real_predecessors_have_distinct_readers() {
        assert_eq!(Reader::for_target("r263"), Some(Reader::R263));
        assert_eq!(Reader::for_target("r264"), Some(Reader::R264));
        for target in ["r263", "r264"] {
            older_stores::validate_target(target).unwrap();
        }
    }

    #[test]
    fn unknown_and_noncanonical_labels_never_select_a_reader() {
        for target in [
            "", "r0263", "r0264", "r+264", "R264", "r264 ", " r264", "r264-dev", "8dba5103",
            "r260", "r261", "r265", "r999",
        ] {
            assert_eq!(Reader::for_target(target), None, "{target}");
            assert!(older_stores::validate_target(target).is_err(), "{target}");
        }
        // Existing historical handling remains outside this two-reader dispatch.
        assert_eq!(Reader::for_target("r262"), None);
        older_stores::validate_target("r262").unwrap();
    }

    #[test]
    fn r264_absent_source_is_preserved_without_export_outputs() {
        let root = Fixture::new();
        let home = root.0.join("absent");
        let displaced = home.join("unused-displaced");
        for result in [
            older_stores::preflight(&home, "r264"),
            older_stores::prepare(&home, &displaced, "r264").map(|notes| assert!(notes.is_empty())),
            older_stores::export_hot_stores(&home, "r264", true)
                .map(|notes| assert!(notes.is_empty())),
        ] {
            result.unwrap();
            assert!(!home.exists());
        }
        assert!(older_stores::export_hot_stores(&home, "r264", false).is_err());
        assert!(!home.exists());
    }
}
