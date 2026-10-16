//! Runtime safety checks the owner may grant once (capability
//! `claude-safety-grants`, docs/safety-grants.md).
//!
//! Under full access Claude Code still asks about a few commands it never
//! lets permission rules skip (an `rm` on a possibly empty variable, an `rm`
//! inside an unparsable `sh -c`). The Claude adapter declines them at once
//! (the turn never waits) and reports a *safety block*
//! (`thread/safetyCheck/blocked`). cutex keeps it as an owner notice
//! (`origin: system`, `systemKind: safetyBlock`, field `safetyBlock`;
//! notification event `system.safetyBlock`, 需要你 by default), so it reaches
//! the desktop's 需要你, the phone list and the live
//! `cutex/ownerNotify/created|updated` events like every other notice.
//!
//! The owner decides with two session cutex methods (owner only: the
//! Management root credential; non-owner users are refused by the access
//! gate, agents have no Management credential):
//!
//! - `cutex/safetyCheck/grant {blockId}`: the adapter stores a grant for
//!   exactly that (thread, tool, input), used once, valid 30 minutes; cutex
//!   then hands the agent one owner input
//!   「主人已批准一次：<command>（30 分钟内有效，仅此一次）」.
//! - `cutex/safetyCheck/dismiss {blockId}`: closes the item, no grant.
//!
//! `GET /v2/safety-checks?state=open|all&agentId=&limit=` lists the notices
//! that carry a block (owner only). Later block states from the runtime
//! (`thread/safetyCheck/updated`: granted, used) update the record.

use std::net::TcpStream;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::server::ValidatedCutexRequest;
use super::user_input::UserInputExecutionError;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;
use crate::notification_levels::Evaluator;
use crate::config::owner_notify::OwnerNotifySettings;
use crate::owner_notify::store::{OwnerError, OwnerNotification, OwnerNotifyState, OwnerNotifyStore, SafetyBlock};

/// Capability name (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "claude-safety-grants";
pub const PATH: &str = "/v2/safety-checks";
pub const GRANT_METHOD: &str = "cutex/safetyCheck/grant";
pub const DISMISS_METHOD: &str = "cutex/safetyCheck/dismiss";
pub const METHODS: &[&str] = &[GRANT_METHOD, DISMISS_METHOD];
/// Runtime (app-server) notifications.
pub const BLOCKED_NOTIFICATION: &str = "thread/safetyCheck/blocked";
pub const UPDATED_NOTIFICATION: &str = "thread/safetyCheck/updated";
const RUNTIME_GRANT: &str = "thread/safetyCheck/grant";
const RUNTIME_DISMISS: &str = "thread/safetyCheck/dismiss";
const CLIENT_ID: &str = "cutex-safety-grants";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_BLOCK_ID: usize = 128;

pub fn is_path(path: &str) -> bool {
    path.split('?').next() == Some(PATH)
}

pub fn is_command(method: &str) -> bool {
    METHODS.contains(&method)
}

fn error(code: &str, message: impl Into<String>, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: message.into(),
        retryable: matches!(code, "session_offline" | "owner_notify_unavailable" | "app_server_unavailable"),
        details,
        outcome_unknown: false,
    }
}

fn owner_error(value: OwnerError) -> UserInputExecutionError {
    error(value.code, value.message, json!({}))
}

fn unavailable(e: anyhow::Error) -> UserInputExecutionError {
    error("owner_notify_unavailable", format!("owner notify store: {e:#}"), json!({}))
}

/// The owner input a grant hands to the agent.
pub fn grant_text(block: &SafetyBlock) -> String {
    let minutes = (block.grant_seconds / 60).max(1);
    format!("主人已批准一次：{}（{minutes} 分钟内有效，仅此一次）", block.command)
}

/// A block as the runtime reports it (`block` of the notification or the
/// method result). `runtime` names the runtime family.
pub fn block_from_wire(runtime: &str, value: &Value) -> Option<SafetyBlock> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let block_id = text("blockId").filter(|id| !id.is_empty() && id.len() <= MAX_BLOCK_ID)?;
    Some(SafetyBlock {
        block_id,
        runtime: runtime.to_string(),
        thread_id: text("threadId")?,
        turn_id: text("turnId"),
        item_id: text("itemId"),
        tool_name: text("toolName")?,
        tool_input: value.get("toolInput").cloned().unwrap_or(Value::Null),
        command: text("command")?,
        reason: text("reason").unwrap_or_default(),
        state: text("state").filter(|state| {
            matches!(state.as_str(), "blocked" | "granted" | "used" | "dismissed" | "expired")
        })?,
        blocked_at: text("blockedAt").unwrap_or_default(),
        decided_at: text("decidedAt"),
        expires_at: text("expiresAt"),
        used_at: text("usedAt"),
        grant_seconds: value.get("grantSeconds").and_then(Value::as_u64).unwrap_or(1800),
    })
}

/// Fold one runtime notification into the store. Returns whether anything
/// changed (a notice was queued).
#[allow(clippy::too_many_arguments)]
pub fn apply_notification(
    state: &mut OwnerNotifyState,
    agent_id: &str,
    agent_name: Option<String>,
    project: Option<Option<&str>>,
    method: &str,
    params: &Value,
    settings: &OwnerNotifySettings,
    evaluator: &Evaluator,
    now: DateTime<Utc>,
) -> bool {
    let Some(block) = params.get("block").and_then(|block| block_from_wire("claude", block)) else {
        return false;
    };
    match method {
        BLOCKED_NOTIFICATION => {
            state.record_safety_block(agent_id, agent_name, block, settings, evaluator, project, now).1
        }
        UPDATED_NOTIFICATION => {
            if state.safety_record(agent_id, &block.block_id).is_none() {
                return false;
            }
            state.update_safety_block(agent_id, block, now).map(|(_, changed)| changed).unwrap_or(false)
        }
        _ => false,
    }
}

/// Fold a runtime's block list (`thread/safetyCheck/list`, newest first)
/// into the store: an unknown block that is still `blocked` is recorded,
/// a known one takes the runtime's state. Returns whether anything changed.
#[allow(clippy::too_many_arguments)]
pub fn apply_snapshot(
    state: &mut OwnerNotifyState,
    agent_id: &str,
    agent_name: Option<String>,
    thread_id: &str,
    blocks: &[Value],
    settings: &OwnerNotifySettings,
    evaluator: &Evaluator,
    now: DateTime<Utc>,
) -> bool {
    let mut changed = false;
    for block in blocks.iter().rev() {
        let Some(parsed) = block_from_wire("claude", block).filter(|b| b.thread_id == thread_id) else {
            continue;
        };
        let known = state.safety_record(agent_id, &parsed.block_id).is_some();
        let method = if known { UPDATED_NOTIFICATION } else if parsed.is_open() { BLOCKED_NOTIFICATION } else { continue };
        changed |= apply_notification(state, agent_id, agent_name.clone(), None, method,
            &json!({ "threadId": thread_id, "block": block }), settings, evaluator, now);
    }
    changed
}

/// After an observer (re)connects: blocks the runtime reported while no
/// cutex process was connected still reach the owner.
pub fn reconcile(cutex_session_id: &str, thread_id: &str) {
    if cfg!(test) {
        return;
    }
    let blocks = match crate::runtime::claude::safety_blocks(cutex_session_id) {
        Ok(blocks) if !blocks.is_empty() => blocks,
        Ok(_) => return,
        Err(error) => {
            eprintln!("safety checks: could not list blocks of {cutex_session_id}: {error:#}");
            return;
        }
    };
    let Ok(store) = OwnerNotifyStore::open_default() else {
        return;
    };
    let agent_name = agent_name(cutex_session_id);
    let (settings, evaluator) = Evaluator::current_with_owner();
    match store.with_state(|state| apply_snapshot(state, cutex_session_id, agent_name, thread_id, &blocks, &settings, &evaluator, Utc::now())) {
        Ok(true) if crate::app_server::single_writer::owner_role() => super::owner_notify::publish_pending_notices(&store),
        Ok(_) => {}
        Err(error) => eprintln!("safety checks: could not reconcile {cutex_session_id}: {error:#}"),
    }
}

fn agent_name(cutex_session_id: &str) -> Option<String> {
    crate::session::store::with_cutex_session_store(|sessions| {
        sessions.sessions.get(cutex_session_id).and_then(|record| {
            record.display_name_override.clone().or_else(|| record.display_name_hint.clone())
        })
    })
    .ok()
    .flatten()
}

/// Runtime observer hook (any process observing a Claude adapter). The
/// management process publishes the events at once; another process leaves
/// them queued for management's next tick (as the `owner_notify` tool does).
pub fn on_runtime_notification(cutex_session_id: &str, thread_id: &str, method: &str, params: &Value) {
    if cfg!(test) || !matches!(method, BLOCKED_NOTIFICATION | UPDATED_NOTIFICATION) {
        return;
    }
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return;
    }
    let Ok(store) = OwnerNotifyStore::open_default() else {
        return;
    };
    let agent_name = agent_name(cutex_session_id);
    let (settings, evaluator) = Evaluator::current_with_owner();
    let changed = store.with_state(|state| {
        apply_notification(state, cutex_session_id, agent_name, None, method, params, &settings, &evaluator, Utc::now())
    });
    match changed {
        Ok(true) if crate::app_server::single_writer::owner_role() => super::owner_notify::publish_pending_notices(&store),
        Ok(_) => {}
        Err(error) => eprintln!("safety checks: could not record {method} for {cutex_session_id}: {error:#}"),
    }
}

/// `GET /v2/safety-checks`.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/safety-checks; decide with cutex/safetyCheck/grant|dismiss", false, json!({}));
    }
    let bad = |stream: &mut TcpStream, message: &str| {
        super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", message, false, json!({}))
    };
    let filter = crate::http::query::query_value(&request.path, "state").unwrap_or_else(|| "open".into());
    if !matches!(filter.as_str(), "open" | "all") {
        return bad(stream, "state: open or all");
    }
    let limit = match crate::http::query::query_value(&request.path, "limit") {
        None => DEFAULT_LIMIT,
        Some(text) => match text.parse::<usize>() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
            _ => return bad(stream, "limit: 1-500"),
        },
    };
    let agent = crate::http::query::query_value(&request.path, "agentId").filter(|id| !id.is_empty());
    match OwnerNotifyStore::open_default().and_then(|store| store.snapshot()) {
        Ok(state) => write_json_response(stream, 200, "OK", &json!({ "blocks": list(&state, &filter, agent.as_deref(), limit) })),
        Err(error) => super::server::write_v2_error(stream, 503, "Service Unavailable", "owner_notify_unavailable",
            &format!("owner notify store: {error:#}"), true, json!({})),
    }
}

/// Notices with a safety block, newest first.
pub fn list(state: &OwnerNotifyState, filter: &str, agent: Option<&str>, limit: usize) -> Vec<OwnerNotification> {
    state
        .list("all", agent, usize::MAX)
        .into_iter()
        .filter(|record| record.safety_block.as_ref().is_some_and(|block| filter == "all" || block.is_open()))
        .take(limit)
        .collect()
}

fn block_id(params: &Value) -> Result<String, UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| super::server::invalid_user_input_error("params must be an object"))?;
    if let Some(key) = object.keys().find(|key| key.as_str() != "blockId") {
        return Err(super::server::invalid_user_input_error(&format!("unsupported field: {key}")));
    }
    object
        .get("blockId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_BLOCK_ID)
        .map(str::to_string)
        .ok_or_else(|| super::server::invalid_user_input_error("blockId is required"))
}

/// Why the runtime call failed, for [`dispatch_with`].
#[derive(Debug)]
pub enum RuntimeCallError {
    Offline,
    Unsupported,
    Refused { code: String, message: String, data: Value },
    Failed(String),
}

fn runtime_call(cutex_session_id: &str, block: &SafetyBlock, method: &str, params: Value) -> Result<Value, RuntimeCallError> {
    if block.runtime != "claude" {
        return Err(RuntimeCallError::Unsupported);
    }
    use crate::runtime::claude::SafetyDecisionError as E;
    crate::runtime::claude::safety_check_decision(cutex_session_id, method, params).map_err(|error| match error {
        E::Offline => RuntimeCallError::Offline,
        E::Unsupported => RuntimeCallError::Unsupported,
        E::Refused { code, message, data } => RuntimeCallError::Refused { code, message, data },
        E::Failed(error) => RuntimeCallError::Failed(format!("{error:#}")),
    })
}

/// Session cutex methods (`cutex/safetyCheck/*`).
pub(super) fn dispatch(
    event_repository: &super::repository::EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let store = OwnerNotifyStore::open_default().map_err(unavailable)?;
    dispatch_with(
        owner.is_some(),
        &store,
        cutex_session_id,
        session,
        request,
        |block, method, params| runtime_call(cutex_session_id, block, method, params),
        |submit| super::offline_submit::dispatch(event_repository, context, owner, cutex_session_id, session, submit),
    )
}

pub(super) fn dispatch_with(
    root: bool,
    store: &OwnerNotifyStore,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
    call_runtime: impl FnOnce(&SafetyBlock, &str, Value) -> Result<Value, RuntimeCallError>,
    submit: impl FnOnce(&ValidatedCutexRequest) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    if !root {
        return Err(error("cutex_method_denied", "safety check decisions are the owner's (Management root credential)", json!({})));
    }
    let grant = match request.method.as_str() {
        GRANT_METHOD => true,
        DISMISS_METHOD => false,
        _ => return Err(super::server::invalid_user_input_error("unknown safety check method")),
    };
    let id = block_id(&request.params)?;
    let record = store
        .snapshot()
        .map_err(unavailable)?
        .safety_record(cutex_session_id, &id)
        .cloned()
        .ok_or_else(|| error("safety_block_not_found", format!("no safety block {id} for this agent"), json!({ "blockId": id })))?;
    let current = record.safety_block.clone().expect("safety record has a block");
    if !current.is_open() {
        return Err(error("safety_block_closed", format!("safety block {id} was already {}", current.state),
            json!({ "notification": record })));
    }
    let now = Utc::now();
    let params = json!({ "threadId": current.thread_id, "blockId": id, "decidedBy": { "kind": "owner" } });
    let decided = match call_runtime(&current, if grant { RUNTIME_GRANT } else { RUNTIME_DISMISS }, params) {
        Ok(result) => result
            .get("block")
            .and_then(|block| block_from_wire(&current.runtime, block))
            .filter(|block| block.block_id == id)
            .ok_or_else(|| error("app_server_unavailable", "the runtime answered without the block", json!({})))?,
        Err(RuntimeCallError::Refused { code, message, data }) => {
            // Someone else decided first (the attached TUI): take the runtime's state.
            if let Some(block) = data.get("block").and_then(|block| block_from_wire(&current.runtime, block)) {
                let notification = store
                    .with_state(|state| state.update_safety_block(cutex_session_id, block, now))
                    .map_err(unavailable)?
                    .map_err(owner_error)?
                    .0;
                super::owner_notify::publish_pending_notices(store);
                return Err(error(if code == "safety_block_closed" { "safety_block_closed" } else { "runtime_refused" },
                    message, json!({ "notification": notification })));
            }
            return Err(error("runtime_refused", message, json!({ "runtimeCode": code })));
        }
        // Dismissing needs no runtime: the item just closes.
        Err(_) if !grant => SafetyBlock {
            state: "dismissed".into(),
            decided_at: Some(crate::asks::now_rfc3339(now)),
            ..current.clone()
        },
        Err(RuntimeCallError::Offline) => {
            return Err(error("session_offline", "the agent's runtime is not running; start the agent, then approve again",
                json!({ "blockId": id })));
        }
        Err(RuntimeCallError::Unsupported) => {
            return Err(error("safety_grants_unsupported",
                "this agent's runtime cannot take a one-time grant (update its Claude runtime)", json!({ "blockId": id })));
        }
        Err(RuntimeCallError::Failed(message)) => return Err(error("app_server_unavailable", message, json!({}))),
    };
    let notification = store
        .with_state(|state| state.update_safety_block(cutex_session_id, decided.clone(), now))
        .map_err(unavailable)?
        .map_err(owner_error)?
        .0;
    let mut result = json!({ "notification": notification, "block": decided });
    if grant {
        // The agent learns of the grant as the owner's next message (busy:
        // after the current turn, never steered into it).
        let busy = session.pointer("/runtime/activeTurnId").is_some_and(|turn| !turn.is_null());
        let input = ValidatedCutexRequest {
            request_id: request.request_id.clone(),
            method: "cutex/userInput/submit".into(),
            params: json!({
                "clientUserMessageId": format!("safety-grant-{id}"),
                "origin": {"kind": "other", "clientId": CLIENT_ID},
                "strategy": if busy { "queue" } else { "auto" },
                "input": [{"type": "text", "text": grant_text(&decided), "text_elements": []}],
                "startIfOffline": true,
            }),
        };
        match submit(&input) {
            Ok(submitted) => result["submit"] = submitted,
            // The grant stands (the runtime stored it); say the message failed.
            Err(failure) => {
                result["submitError"] = json!({ "code": failure.code, "message": failure.message, "retryable": failure.retryable })
            }
        }
    }
    super::owner_notify::publish_pending_notices(store);
    Ok(result)
}

#[cfg(test)]
#[path = "safety_checks_tests.rs"]
mod tests;
