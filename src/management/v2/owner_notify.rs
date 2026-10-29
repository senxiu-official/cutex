//! Owner notify on Management v2 (capability `owner-messages`, cutex-desktop
//! `docs/research/owner-notify.md` §5):
//!
//! - `GET /v2/owner-notifications?filter=pending|alerting|all&agentId=&limit=`
//!   → `{notifications}` (newest first; root credential, device tokens
//!   included).
//! - Session cutex methods on the sender's session:
//!   `cutex/ownerNotify/ack {notificationId}`,
//!   `cutex/ownerNotify/reply {notificationId, text}` (queues one owner
//!   input to the agent, like `cutex/agentBus/wake`),
//!   `cutex/ownerNotify/dismiss {notificationId}`,
//!   `cutex/ownerNotify/delete {notificationId}` (removes the record;
//!   capability `owner-notify-delete`). Reply refuses records that do not
//!   ask for one or that cutex wrote (`origin: system`): 409
//!   `notification_not_replyable`.
//! - Events `cutex/ownerNotify/created|updated {notification}` and
//!   `cutex/ownerNotify/deleted {notificationId, agentId}` on the sender's
//!   session stream ([`publish_pending_notices`]).

use std::net::TcpStream;

use serde_json::{json, Value};

use super::server::ValidatedCutexRequest;
use super::user_input::UserInputExecutionError;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;
use crate::owner_notify::store::{OwnerError, OwnerNotification, OwnerNotifyStore, Reply};

pub const PATH: &str = "/v2/owner-notifications";
const CLIENT_ID: &str = "cutex-owner-notify";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
/// Records of one agent in its bootstrap.
const BOOTSTRAP_RECORDS: usize = 50;

pub fn is_path(path: &str) -> bool {
    path.split('?').next() == Some(PATH)
}

pub fn is_command(method: &str) -> bool {
    crate::owner_notify::METHODS.contains(&method)
}

fn error(code: &str, message: impl Into<String>, retryable: bool) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: message.into(),
        retryable,
        details: json!({}),
        outcome_unknown: false,
    }
}

fn owner_error(error_value: OwnerError) -> UserInputExecutionError {
    error(error_value.code, error_value.message, false)
}

fn store() -> Result<OwnerNotifyStore, UserInputExecutionError> {
    OwnerNotifyStore::open_default().map_err(|e| error("owner_notify_unavailable", format!("{e:#}"), true))
}

fn unavailable(e: anyhow::Error) -> UserInputExecutionError {
    error("owner_notify_unavailable", format!("owner notify store: {e:#}"), true)
}

/// Publish queued notices as Management v2 events (management process).
/// Best effort: resources always reflect the store.
pub fn publish_pending_notices(store: &OwnerNotifyStore) {
    let notices = match store.with_state(|state| std::mem::take(&mut state.notices)) {
        Ok(notices) => notices,
        Err(error) => {
            eprintln!("owner notify: could not read notices: {error:#}");
            return;
        }
    };
    if notices.is_empty() {
        return;
    }
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    for notice in notices {
        if let Err(error) = super::server::append_cutex_event(
            repository,
            &notice.session,
            super::model::EventCorrelation::default(),
            &notice.method,
            notice.params,
        ) {
            eprintln!("owner notify: could not publish {}: {error:#}", notice.method);
        }
    }
}

/// Publish whatever another process queued (scheduler tick).
pub fn publish_default() {
    if let Ok(store) = OwnerNotifyStore::open_default() {
        publish_pending_notices(&store);
    }
}

/// `POST /v2/summary/read`: the owner read the agent; its open records are
/// acknowledged (alerts close everywhere). Returns how many.
pub fn mark_agent_read(agent_id: &str) -> usize {
    let Ok(store) = OwnerNotifyStore::open_default() else {
        return 0;
    };
    let count = store
        .with_state(|state| state.mark_agent_read(agent_id, chrono::Utc::now()))
        .unwrap_or(0);
    if count > 0 {
        publish_pending_notices(&store);
    }
    count
}

/// `cutexState.ownerNotifications` of one session: its last records, oldest
/// first.
pub fn bootstrap_records(cutex_session_id: &str) -> Value {
    let mut records = OwnerNotifyStore::open_default()
        .and_then(|store| store.snapshot())
        .map(|state| state.list("all", Some(cutex_session_id), BOOTSTRAP_RECORDS))
        .unwrap_or_default();
    records.reverse();
    json!(records)
}

/// Every record (summary needs and `messageToOwner`).
pub fn all_records() -> Vec<OwnerNotification> {
    OwnerNotifyStore::open_default()
        .and_then(|store| store.snapshot())
        .map(|state| state.notifications.into_values().collect())
        .unwrap_or_default()
}

/// `GET /v2/owner-notifications`.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/owner-notifications", false, json!({}));
    }
    let bad = |stream: &mut TcpStream, message: &str| {
        super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", message, false, json!({}))
    };
    let filter = crate::http::query::query_value(&request.path, "filter").unwrap_or_else(|| "all".into());
    if !matches!(filter.as_str(), "all" | "pending" | "alerting") {
        return bad(stream, "filter: all, pending or alerting");
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
        Ok(state) => write_json_response(
            stream,
            200,
            "OK",
            &json!({ "notifications": state.list(&filter, agent.as_deref(), limit) }),
        ),
        Err(error) => super::server::write_v2_error(stream, 503, "Service Unavailable", "owner_notify_unavailable",
            &format!("owner notify store: {error:#}"), true, json!({})),
    }
}

fn notification_id(params: &Value) -> Result<&str, UserInputExecutionError> {
    params
        .get("notificationId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or_else(|| super::server::invalid_user_input_error("notificationId is required"))
}

fn only_fields(params: &Value, allowed: &[&str]) -> Result<(), UserInputExecutionError> {
    let object = params
        .as_object()
        .ok_or_else(|| super::server::invalid_user_input_error("params must be an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(super::server::invalid_user_input_error(&format!("unsupported field: {key}")));
    }
    Ok(())
}

/// The owner input a reply sends.
pub fn reply_text(record: &OwnerNotification, reply: &str) -> String {
    format!(
        "Owner reply to your owner_notify message {} (\"{}\"):\n{}",
        record.notification_id,
        crate::asks::truncate_chars(&record.text.split_whitespace().collect::<Vec<_>>().join(" "), 80),
        reply
    )
}

/// Session cutex methods (`cutex/ownerNotify/*`).
pub(super) fn dispatch(
    event_repository: &super::repository::EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    dispatch_with(owner.is_some(), &store()?, cutex_session_id, session, request, |submit| {
        super::offline_submit::dispatch(event_repository, context, owner, cutex_session_id, session, submit)
    })
}

pub(super) fn dispatch_with(
    root: bool,
    store: &OwnerNotifyStore,
    cutex_session_id: &str,
    _session: &Value,
    request: &ValidatedCutexRequest,
    submit: impl FnOnce(&ValidatedCutexRequest) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    if !root {
        return Err(error("cutex_method_denied", "owner notify methods require the Management root credential", false));
    }
    let now = chrono::Utc::now();
    let method = request.method.as_str();
    let result = match method {
        crate::owner_notify::ACK_METHOD | crate::owner_notify::DISMISS_METHOD => {
            only_fields(&request.params, &["notificationId"])?;
            let id = notification_id(&request.params)?;
            let outcome = store
                .with_state(|state| {
                    if method == crate::owner_notify::ACK_METHOD {
                        state.acknowledge(cutex_session_id, id, now)
                    } else {
                        state.dismiss(cutex_session_id, id, now)
                    }
                })
                .map_err(unavailable)?
                .map_err(owner_error)?;
            Ok(json!({ "notification": outcome.0, "changed": outcome.1 }))
        }
        crate::owner_notify::DELETE_METHOD => {
            only_fields(&request.params, &["notificationId"])?;
            let id = notification_id(&request.params)?;
            let removed = store
                .with_state(|state| state.delete(cutex_session_id, id))
                .map_err(unavailable)?
                .map_err(owner_error)?;
            Ok(json!({ "notification": removed, "deleted": true }))
        }
        crate::owner_notify::REPLY_METHOD => {
            only_fields(&request.params, &["notificationId", "text"])?;
            let id = notification_id(&request.params)?;
            let text = request
                .params
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty() && text.chars().count() <= crate::owner_notify::store::MAX_REPLY_CHARS)
                .ok_or_else(|| super::server::invalid_user_input_error("text: 1-20000 characters"))?;
            let record = store
                .snapshot()
                .map_err(unavailable)?
                .check_reply(cutex_session_id, id)
                .map_err(owner_error)?;
            let client_user_message_id = format!("owner-notify-reply-{id}");
            // The owner's own words: `auto` like typed input (023 Q2). Busy: read at
            // the next step (Codex steer; Claude same-turn steering, else after the
            // turn on older adapters). Idle: now.
            let input = ValidatedCutexRequest {
                request_id: request.request_id.clone(),
                method: "cutex/userInput/submit".into(),
                params: json!({
                    "clientUserMessageId": client_user_message_id,
                    "origin": {"kind": "other", "clientId": CLIENT_ID},
                    "strategy": "auto",
                    "input": [{"type": "text", "text": reply_text(&record, text), "text_elements": []}],
                    "startIfOffline": true,
                }),
            };
            let submitted = submit(&input)?;
            let reply = Reply {
                text: text.to_string(),
                replied_at: crate::asks::now_rfc3339(now),
                client_user_message_id,
            };
            let notification = store
                .with_state(|state| state.answered(cutex_session_id, id, reply, now))
                .map_err(unavailable)?
                .map_err(owner_error)?;
            Ok(json!({ "notification": notification, "submit": submitted }))
        }
        _ => Err(super::server::invalid_user_input_error("unknown owner notify method")),
    };
    publish_pending_notices(store);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::owner_notify::OwnerNotifySettings;
    use crate::owner_notify::store::{Level, NewNotification};

    fn temp() -> (OwnerNotifyStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-owner-notify-mgmt-{}", uuid::Uuid::new_v4().simple()));
        (OwnerNotifyStore::at(dir.join("owner-notify")), dir)
    }

    fn create(store: &OwnerNotifyStore, agent: &str, needs_reply: bool) -> OwnerNotification {
        store
            .with_state(|state| {
                state.create(
                    NewNotification {
                        agent_id: agent.into(),
                        agent_name: None,
                        text: "Need   a decision\non the schema".into(),
                        level: Level::Attention,
                        needs_reply,
                        delegation: None,
                        job: None,
                        turn_id: None,
                    },
                    &OwnerNotifySettings::default(),
                    chrono::Utc::now(),
                )
            })
            .unwrap()
    }

    fn request(method: &str, params: Value) -> ValidatedCutexRequest {
        ValidatedCutexRequest { request_id: "req-1".into(), method: method.into(), params }
    }

    #[test]
    fn ack_reply_dismiss_are_root_only_scoped_and_submit_one_input() {
        let (store, dir) = temp();
        let record = create(&store, "agent-a", true);
        let id = record.notification_id.clone();
        let session = json!({"runtime": {"activeTurnId": "turn-9"}});
        let never = |_: &ValidatedCutexRequest| -> Result<Value, UserInputExecutionError> { unreachable!() };
        let denied = dispatch_with(false, &store, "agent-a", &session, &request(crate::owner_notify::ACK_METHOD, json!({"notificationId": id})), never).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied");
        let other = dispatch_with(true, &store, "agent-b", &session, &request(crate::owner_notify::ACK_METHOD, json!({"notificationId": id})), never).unwrap_err();
        assert_eq!(other.code, "notification_not_found");
        let acked = dispatch_with(true, &store, "agent-a", &session, &request(crate::owner_notify::ACK_METHOD, json!({"notificationId": id})), never).unwrap();
        assert_eq!(acked["notification"]["state"], "acknowledged");
        assert_eq!(acked["changed"], true);
        let mut seen = None;
        let replied = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::REPLY_METHOD, json!({"notificationId": id, "text": "Use v2"})),
            |input| {
                seen = Some(input.clone());
                Ok(json!({"disposition": "queued"}))
            })
            .unwrap();
        assert_eq!(replied["notification"]["state"], "answered");
        assert_eq!(replied["notification"]["reply"]["text"], "Use v2");
        let input = seen.unwrap();
        assert_eq!(input.method, "cutex/userInput/submit");
        // Busy requester (activeTurnId set): still `auto`, read at its next step (023 Q2).
        assert_eq!(input.params["strategy"], "auto");
        assert_eq!(input.params["clientUserMessageId"], format!("owner-notify-reply-{id}"));
        let text = input.params["input"][0]["text"].as_str().unwrap();
        assert!(text.starts_with(&format!("Owner reply to your owner_notify message {id} (\"Need a decision on the schema\"):\n")));
        assert!(text.ends_with("Use v2"));
        let again = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::REPLY_METHOD, json!({"notificationId": id, "text": "x"})), never).unwrap_err();
        assert_eq!(again.code, "notification_closed");
        let second = create(&store, "agent-a", true);
        let dismissed = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::DISMISS_METHOD, json!({"notificationId": second.notification_id})), never).unwrap();
        assert_eq!(dismissed["notification"]["state"], "dismissed");
        let bad = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::ACK_METHOD, json!({"notificationId": id, "extra": 1})), never).unwrap_err();
        assert_eq!(bad.code, "invalid_request");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_is_root_only_and_reply_refuses_system_and_plain_notices() {
        let (store, dir) = temp();
        let session = json!({"runtime": {"activeTurnId": null}});
        let never = |_: &ValidatedCutexRequest| -> Result<Value, UserInputExecutionError> { unreachable!() };
        let system = store
            .with_state(|state| {
                state.create_with_origin(
                    NewNotification {
                        agent_id: "agent-a".into(),
                        agent_name: None,
                        text: "消息没能送达（tsa_1）：event_too_large".into(),
                        level: Level::Attention,
                        needs_reply: false,
                        delegation: None,
                        job: None,
                        turn_id: None,
                    },
                    crate::owner_notify::store::Origin::System,
                    &OwnerNotifySettings::default(),
                    chrono::Utc::now(),
                )
            })
            .unwrap();
        let id = system.notification_id.clone();
        let reply = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::REPLY_METHOD, json!({"notificationId": id, "text": "ok"})), never).unwrap_err();
        assert_eq!(reply.code, "notification_not_replyable");
        assert_eq!(super::super::server::cutex_operation_error_status(&reply.code, false), 409);
        let plain = create(&store, "agent-a", false);
        let reply = dispatch_with(true, &store, "agent-a", &session,
            &request(crate::owner_notify::REPLY_METHOD, json!({"notificationId": plain.notification_id, "text": "ok"})), never).unwrap_err();
        assert_eq!(reply.code, "notification_not_replyable");
        let delete = |root: bool, agent: &str, params: Value| {
            dispatch_with(root, &store, agent, &session, &request(crate::owner_notify::DELETE_METHOD, params), never)
        };
        assert_eq!(delete(false, "agent-a", json!({"notificationId": id})).unwrap_err().code, "cutex_method_denied");
        assert_eq!(delete(true, "agent-b", json!({"notificationId": id})).unwrap_err().code, "notification_not_found");
        assert_eq!(delete(true, "agent-a", json!({"notificationId": id, "x": 1})).unwrap_err().code, "invalid_request");
        let deleted = delete(true, "agent-a", json!({"notificationId": id})).unwrap();
        assert_eq!(deleted["deleted"], true);
        assert_eq!(deleted["notification"]["origin"], "system");
        assert_eq!(delete(true, "agent-a", json!({"notificationId": id})).unwrap_err().code, "notification_not_found");
        let left = store.snapshot().unwrap().list("all", Some("agent-a"), 10);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].notification_id, plain.notification_id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paths_and_methods() {
        assert!(is_path("/v2/owner-notifications"));
        assert!(is_path("/v2/owner-notifications?filter=pending"));
        assert!(!is_path("/v2/owner-notifications/x"));
        for method in crate::owner_notify::METHODS {
            assert!(is_command(method));
            assert!(super::super::session::cutex_method_is_registered(method), "{method}");
        }
    }
}
