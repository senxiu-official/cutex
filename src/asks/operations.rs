//! Durable receipts for delegation control operations. This is an AskStore
//! projection, not a second action database. Reads never retry work.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use super::store::{ActionLookup, AskError, AskState};

pub const PREFIX: &str = "delegation-control:";
pub const SCHEMA: &str = "cutex.delegation-operation.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectState { None, Partial, Committed, Unknown }

/// Mutation is atomic within the existing AskStore lock. The caller must hold
/// the authority fence and rebuild authenticated scope before entering here.
/// Failed preconditions do not leave a misleading pending receipt.
pub fn execute(
    state: &mut AskState, caller: &str, operation: &str, action: &str,
    request: &Value, now: DateTime<Utc>,
    mutate: impl FnOnce(&mut AskState) -> Result<(Value, EffectState), AskError>,
) -> Result<Value, AskError> {
    if caller.is_empty() || action.is_empty() || action.len() > 256
        || !action.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)) {
        return Err(AskError::new("invalid_arguments", "nonempty bounded actionId required"));
    }
    let tool = format!("{PREFIX}{operation}");
    let digest = crate::approvals::request_digest(request);
    // Replays and conflicts need no speculative copy of the entire store.
    // New mutations still use a private draft so failed preconditions are atomic.
    let key = format!("{caller}\u{1f}{action}");
    if let Some(record) = state.actions.get(&key) {
        if record.tool != tool || record.request_sha256 != digest {
            return Err(AskError::new("idempotency_conflict", "actionId belongs to a different request"));
        }
        return record.result.clone().ok_or_else(|| AskError::new("outcome_unknown", "inspect the original action; never replay with a new ID"));
    }
    let mut next = state.clone();
    match next.begin_action(caller, action, &tool, &digest, now) {
        ActionLookup::Replay(value) => return Ok(value),
        ActionLookup::Conflict => return Err(AskError::new("idempotency_conflict", "actionId belongs to a different request")),
        ActionLookup::InFlight => return Err(AskError::new("outcome_unknown", "inspect the original action; never replay with a new ID")),
        ActionLookup::New => {}
    }
    let (mut result, effect) = mutate(&mut next)?;
    if !result.is_object() {
        return Err(AskError::new("invalid_operation_result", "operation result must be an object"));
    }
    let receipt = json!({
        "schema": SCHEMA, "actionId": action,
        "operationId": super::new_id("operation"), "operation": operation,
        "requestDigest": digest,
        "status": if matches!(effect, EffectState::Partial | EffectState::Unknown) { "pending" } else { "completed" },
        "effectState": effect, "outcomeUnknown": effect == EffectState::Unknown,
        "revision": 1, "updatedAt": super::now_rfc3339(now)
    });
    if operation=="delegation.stop" {result["operationId"]=receipt["operationId"].clone();}
    result["receipt"] = receipt;
    next.finish_action(caller, action, &result);
    *state = next;
    Ok(result)
}

/// Exact caller/action lookup. Caller is authenticated server context, never a
/// request field. A miss cannot prove that an operation did not run.
pub fn lookup(state: &AskState, caller: &str, action: &str) -> Value {
    let key = format!("{caller}\u{1f}{action}");
    match state.actions.get(&key).filter(|r| r.tool.starts_with(PREFIX)) {
        Some(record) => match &record.result {
            Some(result) => json!({"schema":SCHEMA,"actionId":action,"status":"found","outcomeUnknown":result.pointer("/receipt/outcomeUnknown").and_then(Value::as_bool).unwrap_or(true),"result":result}),
            None => json!({"schema":SCHEMA,"actionId":action,"status":"unknown","outcomeUnknown":true,"result":null}),
        },
        None => json!({"schema":SCHEMA,"actionId":action,"status":"notFound","outcomeUnknown":true,"result":null}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_conflict_and_failed_precondition_are_atomic() {
        let mut state = AskState::default();
        let now = Utc::now();
        let req = json!({"expectedEpoch":0});
        let first = execute(&mut state,"owner","pause","a",&req,now,|s| {
            s.owner_offline.insert("evidence".into());
            Ok((json!({"changed":true}),EffectState::Committed))
        }).unwrap();
        assert_eq!(execute(&mut state,"owner","pause","a",&req,now,|_| panic!("replayed mutation")).unwrap(), first);
        assert!(execute(&mut state,"owner","pause","a",&json!({}),now,|_| panic!("conflicting mutation")).is_err());
        let before = state.clone();
        assert!(execute(&mut state,"owner","pause","b",&req,now,|s| {
            s.owner_offline.clear();
            Err(AskError::new("conflict", "stale epoch"))
        }).is_err());
        assert_eq!(state,before);
        assert_eq!(lookup(&state,"another","a")["status"],"notFound");
        assert_eq!(lookup(&state,"another","a")["outcomeUnknown"],true);
    }
}
