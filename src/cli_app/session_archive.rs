//! Thin CLI/TUI adapter for the typed Retire/Restore provider transaction.

use std::fmt;

use serde::Serialize;
use serde_json::json;

use cutex::session::model::CutexSessionRecord;
use cutex::session::service::{
    cutex_session_display_name, cutex_session_is_managed,
    cutex_session_key_for_user_id_including_retired,
};
use cutex::session::store::load_cutex_session_store;
use cutex::ui::format::compact_home_path;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveSessionView {
    pub cutex_session_id: String,
    pub revision: u64,
    pub lifecycle: String,
    pub runtime_generation: u64,
    pub status: &'static str,
    pub timestamp: Option<String>,
    pub agent: String,
    pub profile: Option<String>,
    pub managed_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveTransitionReceipt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_request: Option<cutex::agent_management::AgentArchiveRequest>,
    pub cutex_session_id: String,
    pub revision: u64,
    pub lifecycle: String,
    pub runtime_generation: u64,
    pub status: String,
    pub timestamp: Option<String>,
}

#[derive(Debug)]
struct ArchiveCommandError {
    stage: String,
    code: String,
    message: String,
    retryable: bool,
    details: serde_json::Value,
    outcome_unknown: bool,
}

impl ArchiveCommandError {
    fn route_error(code: &str, message: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            stage: "route".to_string(),
            code: code.to_string(),
            message: message.into(),
            retryable: false,
            details,
            outcome_unknown: false,
        }
    }

    fn json_envelope(&self) -> serde_json::Value {
        json!({
            "stage": self.stage,
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "details": self.details,
            "outcomeUnknown": self.outcome_unknown,
        })
    }

    fn human_message(&self) -> String {
        format_archive_error_fields(
            &self.message,
            &self.code,
            &self.details,
            self.outcome_unknown,
        )
    }
}

impl fmt::Display for ArchiveCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.human_message())
    }
}

impl std::error::Error for ArchiveCommandError {}

#[derive(Debug)]
struct ArchiveJsonProcessError(ArchiveCommandError);

impl fmt::Display for ArchiveJsonProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for ArchiveJsonProcessError {}

pub(crate) fn json_process_error(error: &anyhow::Error) -> Option<serde_json::Value> {
    error
        .downcast_ref::<ArchiveJsonProcessError>()
        .map(|error| error.0.json_envelope())
}

pub(crate) fn cmd_session_retired(json_output: bool) -> anyhow::Result<()> {
    let sessions = retired_sessions()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!("No reversibly archived durable Agents are known.");
        return Ok(());
    }
    for session in sessions {
        println!(
            "{}  profile={}  path={}  archived={}  revision={}",
            session.agent,
            session.profile.as_deref().unwrap_or("-"),
            session.managed_path.as_deref().unwrap_or("-"),
            session.timestamp.as_deref().unwrap_or("-"),
            session.revision,
        );
    }
    Ok(())
}

pub(crate) fn cmd_session_retire(
    id: &str,
    reason: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    let receipt = retire_for_command(id, reason).map_err(|error| {
        if json_output {
            anyhow::Error::new(ArchiveJsonProcessError(error))
        } else {
            anyhow::Error::new(error)
        }
    })?;
    present_receipt("Archived", &receipt, json_output)
}

pub(crate) fn cmd_session_restore(id: &str, json_output: bool) -> anyhow::Result<()> {
    let receipt = restore_for_command(id).map_err(|error| {
        if json_output {
            anyhow::Error::new(ArchiveJsonProcessError(error))
        } else {
            anyhow::Error::new(error)
        }
    })?;
    present_receipt("Restored", &receipt, json_output)
}

pub(crate) fn retired_sessions() -> anyhow::Result<Vec<ArchiveSessionView>> {
    let store = load_cutex_session_store()?;
    let roster = cutex::agent_management::AgentManagementStore::open_default()?.snapshot()?;
    let mut sessions = store
        .sessions
        .values()
        .filter(|record| record.is_retired())
        .filter(|record| cutex_session_is_managed(record))
        .filter(|record| {
            !roster.agents.values().any(|a| {
                a.cutex_session_id.as_str() == record.cutex_session_id && a.retired_at.is_some()
            })
        })
        .map(archive_session_view)
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.cutex_session_id.cmp(&right.cutex_session_id));
    Ok(sessions)
}

fn retire_for_command(
    id: &str,
    reason: Option<&str>,
) -> Result<ArchiveTransitionReceipt, ArchiveCommandError> {
    guarded_archive_command(
        id,
        cutex::agent_management::AgentArchiveOperation::Archive,
        reason,
    )
}

fn restore_for_command(id: &str) -> Result<ArchiveTransitionReceipt, ArchiveCommandError> {
    guarded_archive_command(
        id,
        cutex::agent_management::AgentArchiveOperation::Restore,
        None,
    )
}

fn guarded_archive_command(
    id: &str,
    operation: cutex::agent_management::AgentArchiveOperation,
    reason: Option<&str>,
) -> Result<ArchiveTransitionReceipt, ArchiveCommandError> {
    // Resolve the user-facing ID (exact cutex ID, native thread ID or unique
    // name; archived included) against this home's durable store first, as
    // before the guarded service existed: an unknown target stays
    // `session_not_found` without needing the Management credential or a
    // running service, and the guarded service always gets the exact ID.
    let record = load_including_retired_for_command(id)?;
    let exact_id = record.cutex_session_id;
    let result = (|| -> anyhow::Result<_> {
        let client = super::management_control_plane::ManagementControlClient::connect()?;
        guarded_archive_with_client(&client, &exact_id, operation, reason)
    })();
    result.map_err(|error| guarded_archive_error(error, id))
}

fn guarded_archive_with_client(
    client: &super::management_control_plane::ManagementControlClient,
    exact_id: &str,
    operation: cutex::agent_management::AgentArchiveOperation,
    reason: Option<&str>,
) -> anyhow::Result<ArchiveTransitionReceipt> {
    let review = client.review_agent_archive(&cutex::agent_management::AgentArchiveReviewRequest {
        cutex_session_id: cutex::role_revision::CutexSessionId::new(exact_id.to_string())
            .map_err(|_| anyhow::anyhow!("exact durable Agent ID required"))?,
        operation,
    })?;
    let request = cutex::agent_management::AgentArchiveRequest {
        reason: reason.map(str::to_string),
        action_id: cutex::agent_management::AgentActionId::new(format!(
            "cli-archive-{}",
            uuid::Uuid::new_v4()
        ))?,
        review,
    };
    execute_confirmed_archive_with_client(client, &request)
}

/// Typed command errors pass through. A Management answer that the durable
/// Agent does not exist (it vanished between the local lookup and the
/// review) keeps the `session_not_found` contract; anything else is a genuine
/// guard failure (`guarded_archive_failed`).
fn guarded_archive_error(error: anyhow::Error, id: &str) -> ArchiveCommandError {
    let error = match error.downcast::<ArchiveCommandError>() {
        Ok(error) => return error,
        Err(error) => error,
    };
    if management_reports_missing_agent(&error) {
        return ArchiveCommandError::route_error(
            "session_not_found",
            format!("cutex session is not known: {id}"),
            json!({ "cutexSessionId": id }),
        );
    }
    ArchiveCommandError::route_error(
        "guarded_archive_failed",
        format!("{error:#}"),
        json!({ "cutexSessionId": id }),
    )
}

/// The archive review answers a missing durable Agent with
/// `409 conflict: durable_agent_missing`; a typed 404 code is accepted too.
fn management_reports_missing_agent(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<cutex::http::client::HttpStatusError>()
        .is_some_and(|status| match status.code.as_deref() {
            Some("session_not_found" | "durable_agent_missing") => true,
            Some("conflict") => status
                .message
                .as_deref()
                .is_some_and(|message| message.trim_end().ends_with("durable_agent_missing")),
            _ => false,
        })
}

pub(super) fn execute_confirmed_archive(
    request: &cutex::agent_management::AgentArchiveRequest,
) -> anyhow::Result<ArchiveTransitionReceipt> {
    let client = super::management_control_plane::ManagementControlClient::connect()?;
    execute_confirmed_archive_with_client(&client, request)
}

pub(super) fn execute_confirmed_archive_with_client(
    client: &super::management_control_plane::ManagementControlClient,
    request: &cutex::agent_management::AgentArchiveRequest,
) -> anyhow::Result<ArchiveTransitionReceipt> {
    let receipt = client
        .execute_agent_archive(request)
        .map_err(|error| ArchiveCommandError {
            stage: "archive_response".into(),
            code: "archive_response_unconfirmed".into(),
            message: format!(
                "{error:#}; replay exact action {} to reconcile",
                request.action_id
            ),
            retryable: true,
            outcome_unknown: true,
            details: json!({ "retryRequest": request }),
        })?;
    if receipt.stage != cutex::agent_management::AgentArchiveStage::Committed {
        return Err(ArchiveCommandError {
            stage: format!("{:?}", receipt.stage),
            code: "archive_not_committed".into(),
            message: receipt
                .error
                .clone()
                .unwrap_or_else(|| "Archive did not complete; no rollback is claimed".into()),
            retryable: receipt.stage
                != cutex::agent_management::AgentArchiveStage::StoppedNotArchived,
            outcome_unknown: receipt.stage == cutex::agent_management::AgentArchiveStage::Uncertain,
            details: json!({ "receipt": receipt, "retryRequest": request }),
        }
        .into());
    }
    let record = receipt
        .result
        .ok_or_else(|| anyhow::anyhow!("archive receipt omitted result"))?;
    let lifecycle = if record.is_retired() {
        "archived"
    } else {
        "active"
    }
    .into();
    Ok(ArchiveTransitionReceipt {
        retry_request: Some(request.clone()),
        cutex_session_id: record.cutex_session_id,
        revision: record.revision,
        lifecycle,
        runtime_generation: record.runtime_generation,
        status: "offline".into(),
        timestamp: record.retired_at,
    })
}

fn load_including_retired_for_command(id: &str) -> Result<CutexSessionRecord, ArchiveCommandError> {
    let store = load_cutex_session_store().map_err(|error| {
        ArchiveCommandError::route_error(
            "session_lookup_failed",
            format!("failed to load durable Cutex sessions: {error:#}"),
            json!({ "cutexSessionId": id }),
        )
    })?;
    let key = cutex_session_key_for_user_id_including_retired(&store, id).ok_or_else(|| {
        ArchiveCommandError::route_error(
            "session_not_found",
            format!("cutex session is not known: {id}"),
            json!({ "cutexSessionId": id }),
        )
    })?;
    store.sessions.get(&key).cloned().ok_or_else(|| {
        ArchiveCommandError::route_error(
            "session_not_found",
            format!("cutex session disappeared while reloading: {key}"),
            json!({ "cutexSessionId": id }),
        )
    })
}

fn archive_session_view(record: &CutexSessionRecord) -> ArchiveSessionView {
    ArchiveSessionView {
        cutex_session_id: record.cutex_session_id.clone(),
        revision: record.durable_revision(),
        lifecycle: "archived".to_string(),
        runtime_generation: record.runtime_generation,
        status: "offline",
        timestamp: record.retired_at.clone(),
        agent: cutex_session_display_name(record),
        profile: record.profile.clone(),
        managed_path: record.managed_cwd.as_deref().map(compact_home_path),
    }
}

fn present_receipt(
    verb: &str,
    receipt: &ArchiveTransitionReceipt,
    json_output: bool,
) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(receipt)?);
    } else {
        println!(
            "{verb} {} (revision {}, {}, runtime generation {})",
            receipt.cutex_session_id,
            receipt.revision,
            receipt.lifecycle,
            receipt.runtime_generation
        );
    }
    Ok(())
}

#[cfg(test)]
fn format_archive_error(
    error: &cutex::management::v2::user_input::UserInputExecutionError,
) -> String {
    format_archive_error_fields(
        &error.message,
        &error.code,
        &error.details,
        error.outcome_unknown,
    )
}

fn format_archive_error_fields(
    message: &str,
    code: &str,
    details: &serde_json::Value,
    outcome_unknown: bool,
) -> String {
    let resync = outcome_unknown || details["resyncRequired"] == true;
    let suffix = resync
        .then_some("; resync required before retry")
        .unwrap_or("");
    format!("{message} ({code}){suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::session::model::CutexSessionArchiveState;

    #[test]
    fn archive_view_is_secret_free_and_offline() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.archive".to_string(),
            Some("thread".to_string()),
            "host".to_string(),
            "/tmp/project".to_string(),
            Some("profile".to_string()),
            "2026-08-14T00:00:00Z".to_string(),
        )
        .expect("record");
        record.archive_state = CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-14T00:01:00Z".to_string());
        record.runtime_generation = 7;

        let view = archive_session_view(&record);

        assert_eq!(view.lifecycle, "archived");
        assert_eq!(view.status, "offline");
        assert_eq!(view.runtime_generation, 7);
        let json = serde_json::to_value(view).expect("view json");
        assert!(json.get("authToken").is_none());
        assert!(json.get("defaultCliArgs").is_none());
    }

    #[test]
    fn outcome_unknown_error_requires_resync() {
        let error = cutex::management::v2::user_input::UserInputExecutionError {
            stage: "persist".to_string(),
            code: "persistence_uncertain".to_string(),
            message: "unknown".to_string(),
            retryable: false,
            details: json!({ "resyncRequired": true }),
            outcome_unknown: true,
        };
        assert_eq!(
            format_archive_error(&error),
            "unknown (persistence_uncertain); resync required before retry"
        );
    }

    /// One-shot loopback Management stand-in answering the archive review.
    fn review_answering(status_line: &str, body: &str) -> (
        super::super::management_control_plane::ManagementControlClient,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let response = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = cutex::http::server::read_simple_http_request(&mut stream)
                .expect("request");
            assert_eq!(request.path, "/v2/agent-management/archive-review");
            stream.write_all(response.as_bytes()).expect("write");
        });
        (
            super::super::management_control_plane::ManagementControlClient::test_endpoint(
                format!("http://127.0.0.1:{port}"),
                "test-root".to_string(),
            ),
            server,
        )
    }

    fn guarded_error_for(status_line: &str, body: &str) -> ArchiveCommandError {
        let (client, server) = review_answering(status_line, body);
        let error = guarded_archive_with_client(
            &client,
            "cutex.archive",
            cutex::agent_management::AgentArchiveOperation::Archive,
            None,
        )
        .expect_err("review must fail");
        server.join().expect("server thread");
        guarded_archive_error(error, "archive")
    }

    #[test]
    fn management_missing_agent_maps_to_session_not_found() {
        let error = guarded_error_for(
            "HTTP/1.1 409 Conflict",
            r#"{"contractVersion":2,"error":{"source":"cutex","code":"conflict","message":"conflict: durable_agent_missing","retryable":false,"details":{}}}"#,
        );
        assert_eq!(error.code, "session_not_found");
        assert_eq!(error.stage, "route");
        assert_eq!(error.message, "cutex session is not known: archive");
        assert_eq!(error.details, json!({ "cutexSessionId": "archive" }));
        assert!(!error.retryable && !error.outcome_unknown);
    }

    #[test]
    fn genuine_guard_failures_stay_guarded_archive_failed() {
        let error = guarded_error_for(
            "HTTP/1.1 409 Conflict",
            r#"{"contractVersion":2,"error":{"source":"cutex","code":"conflict","message":"conflict: archive_requires_explicit_director_rotation","retryable":false,"details":{}}}"#,
        );
        assert_eq!(error.code, "guarded_archive_failed");
        assert!(
            error.message.contains("archive_requires_explicit_director_rotation"),
            "{}",
            error.message
        );
        let error = guarded_error_for(
            "HTTP/1.1 503 Service Unavailable",
            r#"{"contractVersion":2,"error":{"source":"cutex","code":"persistence_unavailable","message":"persistence unavailable","retryable":true,"details":{}}}"#,
        );
        assert_eq!(error.code, "guarded_archive_failed");
    }

    #[test]
    fn json_process_error_preserves_typed_command_fields() {
        let command_error = ArchiveCommandError {
            stage: "runtime".to_string(),
            code: "revision_conflict".to_string(),
            message: "runtime generation changed".to_string(),
            retryable: true,
            details: json!({
                "expectedRuntimeGeneration": 2,
                "currentRuntimeGeneration": 3,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        };
        let error = anyhow::Error::new(ArchiveJsonProcessError(command_error));

        assert_eq!(
            json_process_error(&error),
            Some(json!({
                "stage": "runtime",
                "code": "revision_conflict",
                "message": "runtime generation changed",
                "retryable": true,
                "details": {
                    "expectedRuntimeGeneration": 2,
                    "currentRuntimeGeneration": 3,
                    "resyncRequired": true,
                },
                "outcomeUnknown": false,
            }))
        );
    }
}
