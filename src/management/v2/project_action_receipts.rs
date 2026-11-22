//! r290 P10 (A1): one owner receipt lookup for every lifecycle action the
//! project panel issues, by its ORIGINAL action id. Answers committed / none /
//! running / unknown / notFound plus the affected entity, so a client fences
//! only that entity and never replays a possibly committed mutation.
//!
//! `GET  /v2/project-actions/{actionId}?kind=..&entity=..`        read-only
//! `POST /v2/project-actions/{actionId}/settle?kind=..&entity=..` owner settle:
//!     a missing receipt is fenced against late arrival. Control history is
//!     retained; missing lifecycle history stays unknown because of retention.
//!     Existing receipts are reported
//!     unchanged; nothing is replayed, retried or reversed.
//!
//! Reached only through `delegations::parse_path`: root + owner principal, and
//! a Full device scope for the POST (server.rs gate, access.rs OwnerOnly).
use std::net::TcpStream;

use chrono::Utc;
use serde_json::{json, Value};

use crate::asks::{operations, store::AskStore};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use super::request_idempotency::{request_idempotency_repository, OwnerRequestLookup};

pub const FEATURE: &str = "project-action-receipts-v1";
pub const SCHEMA: &str = "cutex.project-action-receipt.v1";
/// Only the owner HTTP namespace of AskStore receipts is ever read here.
const OWNER_CALLER: &str = "owner";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// AskStore receipt (`cutex.delegation-operation.v1`): operation, entity type.
    Control(&'static str, &'static str),
    /// `/v2/sessions/{entity}/cutex/requests` request-id store: method.
    Lifecycle(&'static str),
    /// Permanent retirement receipts (`agent_archive_actions`).
    Retire,
}

fn family(kind: &str) -> Option<Family> {
    use Family::*;
    Some(match kind {
        "stop" => Control("delegation.stop", "delegation"),
        "resumeContinuation" => Control("continuation.resume", "delegation"),
        "pause" => Control("project.pause", "projectRunControl"),
        "resume" => Control("project.resume", "projectRunControl"),
        "policy" => Control("policy.update", "projectPolicy"),
        "grant" => Control("grant.create", "projectGrants"),
        "revoke" => Control("grant.revoke", "grant"),
        "decision" => Control("approval.decide", "approval"),
        "offline" => Lifecycle("cutex/runtime/offline"),
        "archive" => Lifecycle("cutex/session/retire"),
        "restore" => Lifecycle("cutex/session/restore"),
        "retire" => Retire,
        _ => return None,
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.starts_with("browser:")
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}

struct Failure { status: u16, reason: &'static str, code: String, message: String }
fn bad(message: impl Into<String>) -> Failure {
    Failure { status: 400, reason: "Bad Request", code: "invalid_request".into(), message: message.into() }
}
fn conflict(code: impl Into<String>, message: impl Into<String>) -> Failure {
    Failure { status: 409, reason: "Conflict", code: code.into(), message: message.into() }
}
fn unavailable(error: impl std::fmt::Display) -> Failure {
    Failure { status: 503, reason: "Service Unavailable", code: "persistence_unavailable".into(), message: format!("{error:#}") }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, action: &str, settle: bool) -> anyhow::Result<()> {
    match resolve(request, action, settle) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(f) => super::server::write_v2_error(stream, f.status, f.reason, &f.code, &f.message, f.status == 503, json!({"actionId": action})),
    }
}

fn resolve(request: &SimpleHttpRequest, action: &str, settle: bool) -> Result<Value, Failure> {
    let wanted = if settle { "POST" } else { "GET" };
    if request.method != wanted { return Err(bad(format!("{wanted} required"))); }
    if !valid_id(action) { return Err(bad("invalid actionId")); }
    let query = request.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let (mut kind, mut entity) = (None, None);
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match name.as_ref() { "kind" => &mut kind, "entity" => &mut entity, _ => return Err(bad(format!("unsupported query parameter: {name}"))) };
        if slot.replace(value.into_owned()).is_some() { return Err(bad(format!("{name} must not be repeated"))); }
    }
    let kind = kind.ok_or_else(|| bad("kind required"))?;
    let family = family(&kind).ok_or_else(|| bad(format!("unsupported kind: {kind}")))?;
    if entity.as_deref().is_some_and(|e| !valid_id(e)) { return Err(bad("invalid entity")); }
    let mut out = match family {
        Family::Control(operation, entity_type) => control(action, operation, entity_type, entity, settle)?,
        Family::Lifecycle(method) => {
            let session = entity.ok_or_else(|| bad("entity (the member cutexSessionId) required"))?;
            lifecycle(action, method, session, settle)?
        }
        Family::Retire => {
            let session = entity.ok_or_else(|| bad("entity (the member cutexSessionId) required"))?;
            retire(action, session)?
        }
    };
    let state = out["state"].as_str().unwrap_or("unknown").to_owned();
    out["schema"] = json!(SCHEMA);
    out["actionId"] = json!(action);
    out["kind"] = json!(kind);
    out["final"] = json!(matches!(state.as_str(), "committed" | "none"));
    Ok(out)
}

fn control(action: &str, operation: &str, entity_type: &str, entity: Option<String>, settle: bool) -> Result<Value, Failure> {
    let store = AskStore::open_default().map_err(unavailable)?;
    let found = if settle {
        // No delegation/history resources are selected: this metadata-only
        // transaction changes just the caller/action receipt under the existing lock.
        store.with_records(&[], |s| operations::settle_none(s, OWNER_CALLER, operation, action, Utc::now())).map_err(unavailable)?
            .map_err(|e| conflict(e.code, e.detail))?
    } else {
        store.inspect(|s| operations::lookup(s, OWNER_CALLER, action)).map_err(unavailable)?
    };
    let result = found.get("result").filter(|r| r.is_object());
    let receipt = result.and_then(|r| r.get("receipt")).cloned();
    if let Some(receipt) = &receipt {
        if receipt["operation"] != operation {
            return Err(conflict("action_kind_mismatch", format!("actionId belongs to {}", receipt["operation"])));
        }
    }
    let state = match (found["status"].as_str(), &receipt) {
        (Some("found"), Some(r)) if r["outcomeUnknown"] == true || r["effectState"] == "unknown" => "unknown",
        (Some("found"), Some(r)) if r["status"] == "pending" => "running",
        (Some("found"), Some(r)) if r["effectState"] == "committed" => "committed",
        (Some("found"), Some(r)) if r["effectState"] == "none" => "none",
        (Some("found"), _) => "unknown",
        // A reserved record without a result: its call still holds the work.
        (Some("unknown"), _) => "running",
        _ => "notFound",
    };
    // The server-derived id wins over the client's hint where the receipt names it.
    let derived = result.and_then(|r| match operation {
        "grant.create" | "grant.revoke" => r.pointer("/grant/id"),
        "approval.decide" => r.pointer("/routingApproval/id"),
        _ => None,
    }).and_then(Value::as_str).map(str::to_owned);
    let entity_type = if operation == "grant.create" && derived.is_some() { "grant" } else { entity_type };
    Ok(json!({
        "operation": operation, "state": state,
        "settled": result.is_some_and(|r| r["settled"] == true),
        "entity": {"type": entity_type, "id": derived.or(entity)},
        "receipt": receipt,
    }))
}

fn lifecycle(action: &str, method: &str, session: String, settle: bool) -> Result<Value, Failure> {
    let repository = request_idempotency_repository().map_err(unavailable)?;
    let found = if settle { repository.owner_settle_none(&session, action) } else { repository.owner_lookup(&session, action) }
        .map_err(unavailable)?;
    let (state, settled, response) = match found {
        OwnerRequestLookup::NotFound => ("notFound", false, Value::Null),
        OwnerRequestLookup::InProgress => ("running", false, Value::Null),
        OwnerRequestLookup::OutcomeUnknown => ("unknown", false, Value::Null),
        // Completed lifecycle receipts may expire or be evicted. Fencing a
        // missing id stops a late original but cannot prove its past outcome.
        OwnerRequestLookup::FencedMissing => ("unknown", true, Value::Null),
        OwnerRequestLookup::Completed(response) => {
            if let Some(actual) = response.body.pointer("/cutex/method").and_then(Value::as_str) {
                if actual != method {
                    return Err(conflict("action_kind_mismatch", format!("actionId belongs to {actual}")));
                }
            }
            // A completed row is final: success, or a definite refusal (outcome-
            // unknown results are never stored as completed).
            let status = response.body.pointer("/error/details/originalStatus").and_then(Value::as_u64)
                .filter(|_| response.body.pointer("/error/code") == Some(&json!("request_result_not_retained")))
                .unwrap_or(response.status as u64);
            let state = if (200..300).contains(&status) { "committed" } else { "none" };
            (state, false, json!({"httpStatus": response.status, "body": response.body}))
        }
    };
    Ok(json!({
        "operation": method, "state": state, "settled": settled,
        "entity": {"type": "agent", "id": session},
        "receipt": response,
    }))
}

fn retire(action: &str, session: String) -> Result<Value, Failure> {
    use crate::agent_management::{AgentActionId, AgentArchiveStage, AgentManagementStore, PermanentRetirementActionStatus as S};
    let action_id = AgentActionId::new(action).map_err(|_| bad("invalid actionId"))?;
    let target = crate::role_revision::CutexSessionId::new(session.clone()).map_err(|_| bad("invalid entity"))?;
    let roster = AgentManagementStore::open_default().map_err(unavailable)?.snapshot().map_err(|e| unavailable(format!("{e:?}")))?;
    let receipt = crate::session::store::archive_receipt_at(
        &crate::session::store::cutex_sessions_path().map_err(unavailable)?, action,
    ).map_err(unavailable)?;
    let status = crate::agent_management::permanent_retirement_action_status(
        &roster, receipt.as_ref(), &action_id, &target, Utc::now().timestamp(),
    ).map_err(|e| conflict("action_kind_mismatch", format!("{e:?}")))?;
    let (state, retry_after, receipt) = match status {
        S::Receipt(r) => {
            let state = match r.stage {
                AgentArchiveStage::Committed | AgentArchiveStage::PermanentRetirementCommitted => "committed",
                // Final: the runtime was stopped, the agent was NOT retired.
                AgentArchiveStage::StoppedNotArchived => "none",
                AgentArchiveStage::Prepared | AgentArchiveStage::Stopped | AgentArchiveStage::Uncertain => "unknown",
            };
            (state, None, serde_json::to_value(&r).unwrap_or(Value::Null))
        }
        S::PossiblyPending { until_unix } => ("running", Some(until_unix), Value::Null),
        S::Unknown => ("unknown", None, Value::Null),
        S::None => ("none", None, Value::Null),
    };
    Ok(json!({
        "operation": "agent.permanentRetire", "state": state, "settled": false,
        "entity": {"type": "agent", "id": session},
        "retryAfterUnix": retry_after,
        "receipt": receipt,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_panel_kind_has_a_receipt_family() {
        for kind in ["stop","pause","resume","decision","policy","grant","revoke","offline","archive","restore","retire","resumeContinuation"] {
            assert!(family(kind).is_some(), "{kind}");
        }
        assert!(family("delete").is_none());
        assert!(!valid_id("browser:x"));
        assert!(!valid_id("a/b"));
    }
}
