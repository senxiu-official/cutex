//! Asks (委托) for the owner's clients (root credential only), asks design
//! §10:
//!
//! - `GET /v2/asks?project=&requester=&target=&involving=&state=&limit=` →
//!   `{hostId, asks: [AskSummary], hasMore}`, newest first (limit 200, max 1000). `requester` is a session
//!   id or `owner`; `state` is `open`, `closed` or an execution state.
//! - `GET /v2/asks/{askId}` → `{ask}`: the record with attempts, the three
//!   states, questions, answer metadata, queue position.
//! - `GET /v2/asks/{askId}/answer?cursor=&revision=` →
//!   `{askId, answerId, revision, status, cursor, nextCursor, bytes, text,
//!   metadata}` (pages of 8 KiB).
//! - `GET /v2/asks/provenance` → `{agents: [{cutexSessionId, createdBy,
//!   rootCreator, creationDepth, originDomain, displayParent,
//!   lifecycleManager, kind}]}` (§8.2; not yet on the session resource).
//! - `GET /v2/artifacts/{artifactId}` → `{artifact, encoding: "utf8"|"base64",
//!   content}`.
//!
//! Events (`cutex/ask/created|stateChanged|answered|closed`,
//! `cutex/ask/question`) are published on both sessions' streams by the
//! management process from the ask store's notice queue
//! ([`publish_pending_notices`]).

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::asks::store::{AskFilter, AskStore};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const ASKS_PATH: &str = "/v2/asks";
pub const ARTIFACTS_PATH: &str = "/v2/artifacts";
/// Answer page size.
pub const ANSWER_PAGE_BYTES: usize = crate::asks::INLINE_ANSWER_MAX_BYTES;
/// `GET /v2/asks` page size: default and maximum `limit` (r208).
pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Ask(String),
    Answer(String),
    Artifact(String),
    /// `GET /v2/asks/provenance`: child-agent provenance of every agent.
    Provenance,
}

fn valid_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix)
        && id.len() <= 80
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == "/v2/asks/provenance" {
        return Some(Route::Provenance);
    }
    if path == ASKS_PATH {
        return Some(Route::Collection);
    }
    if let Some(id) = path.strip_prefix("/v2/artifacts/") {
        return valid_id(id, "art-").then(|| Route::Artifact(id.to_string()));
    }
    let rest = path.strip_prefix("/v2/asks/")?;
    match rest.split_once('/') {
        None => valid_id(rest, "ask-").then(|| Route::Ask(rest.to_string())),
        Some((id, "answer")) if valid_id(id, "ask-") => Some(Route::Answer(id.to_string())),
        _ => None,
    }
}

pub fn is_asks_path(path: &str) -> bool {
    path == ASKS_PATH
        || path.starts_with("/v2/asks/")
        || path.starts_with("/v2/artifacts/")
}

/// An error answer: (status, code, message, details).
pub struct RouteError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

fn not_found(code: &'static str, message: &str, details: Value) -> RouteError {
    RouteError { status: 404, code, message: message.into(), details }
}

fn bad_request(message: impl Into<String>) -> RouteError {
    RouteError { status: 400, code: "invalid_request", message: message.into(), details: json!({}) }
}

/// GET routes as pure JSON (testable without a socket).
pub fn route_json(store: &AskStore, full_path: &str, route: &Route) -> Result<Value, RouteError> {
    let unavailable = |error: anyhow::Error| RouteError {
        status: 503,
        code: "asks_unavailable",
        message: "the ask store could not be read".into(),
        details: json!({ "diagnostic": format!("{error:#}") }),
    };
    let query = |key: &str| crate::http::query::query_value(full_path, key);
    match route {
        Route::Collection => {
            let state = store.snapshot().map_err(unavailable)?;
            let filter = AskFilter {
                project: query("project"),
                requester: query("requester"),
                target: query("target"),
                state: query("state"),
                involving: query("involving"),
            };
            let limit = match query("limit") {
                None => DEFAULT_LIMIT,
                Some(value) => value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| bad_request(format!("limit must be 1..={MAX_LIMIT}")))?,
            };
            let peer = query("peer");
            let mut asks: Vec<_> = state.asks.values().filter(|ask| filter.matches(ask) && peer.as_ref().is_none_or(|id|ask.remote.as_ref().is_some_and(|r|&r.peer_id==id)||matches!(&ask.requester,crate::asks::model::Requester::Peer{peer_id,..}if peer_id==id))).collect();
            asks.sort_by_key(|ask| std::cmp::Reverse(ask.seq));
            let has_more = asks.len() > limit;
            asks.truncate(limit);
            Ok(json!({
                "hostId": crate::platform::host::current_host_name(),
                "asks": asks
                    .into_iter()
                    .map(|ask| crate::asks::view::summary(&state, ask))
                    .collect::<Vec<_>>(),
                "hasMore": has_more,
            }))
        }
        Route::Ask(ask_id) => {
            let state = store.snapshot().map_err(unavailable)?;
            let ask = state
                .asks
                .get(ask_id)
                .ok_or_else(|| not_found("ask_not_found", "no ask has this id", json!({"askId": ask_id})))?;
            Ok(json!({ "ask": crate::asks::view::owner_view(&state, ask) }))
        }
        Route::Answer(ask_id) => {
            let state = store.snapshot().map_err(unavailable)?;
            let ask = state
                .asks
                .get(ask_id)
                .ok_or_else(|| not_found("ask_not_found", "no ask has this id", json!({"askId": ask_id})))?;
            let cursor = match query("cursor") {
                None => 0,
                Some(value) => value.parse::<usize>().map_err(|_| bad_request("cursor must be a byte offset"))?,
            };
            let revision = match query("revision") {
                None => None,
                Some(value) => Some(value.parse::<u32>().map_err(|_| bad_request("revision must be a number"))?),
            };
            let answer = state
                .answers
                .values()
                .filter(|answer| answer.ask_id == *ask_id)
                .filter(|answer| revision.is_none_or(|revision| answer.revision == revision))
                .max_by_key(|answer| answer.revision)
                .ok_or_else(|| not_found("answer_not_found", "the ask has no answer (yet)", json!({"askId": ask_id})))?;
            let text = match answer.artifact_id.as_deref() {
                None => String::new(),
                Some(artifact) => store
                    .read_artifact(artifact, None)
                    .map_err(unavailable)?
                    .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default(),
            };
            let page = crate::asks::answer::page(&text, cursor, ANSWER_PAGE_BYTES);
            Ok(json!({
                "askId": ask.ask_id,
                "answerId": answer.answer_id,
                "revision": answer.revision,
                "status": answer.status,
                "hasMessage": answer.artifact_id.is_some(),
                "cursor": page.cursor,
                "nextCursor": page.next_cursor,
                "bytes": answer.bytes,
                "text": page.text,
                "metadata": answer.metadata,
            }))
        }
        Route::Provenance => crate::asks::children::provenance_json().map_err(unavailable),
        Route::Artifact(artifact_id) => {
            let found = store.read_artifact(artifact_id, None).map_err(unavailable)?;
            let (record, bytes) = found.ok_or_else(|| {
                not_found("artifact_not_found", "no artifact has this id", json!({"artifactId": artifact_id}))
            })?;
            let (encoding, content) = match String::from_utf8(bytes) {
                Ok(text) => ("utf8", text),
                Err(error) => {
                    use base64::Engine as _;
                    ("base64", base64::engine::general_purpose::STANDARD.encode(error.into_bytes()))
                }
            };
            Ok(json!({ "artifact": record, "encoding": encoding, "content": content }))
        }
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_path(path) else {
        return super::server::write_v2_error(stream, 404, "Not Found", "route_not_found",
            "no such ask route", false, json!({}));
    };
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "asks are read with GET; changes are cutex/ask/* requests", false, json!({}));
    }
    let store = match AskStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            return super::server::write_v2_error(stream, 503, "Service Unavailable", "asks_unavailable",
                "the ask store is unavailable", true, json!({"diagnostic": format!("{error:#}")}))
        }
    };
    match route_json(&store, &request.path, &route) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            let reason = match error.status {
                400 => "Bad Request",
                404 => "Not Found",
                _ => "Service Unavailable",
            };
            super::server::write_v2_error(stream, error.status, reason, error.code, &error.message,
                error.status == 503, error.details)
        }
    }
}

/// Publish queued ask notices as Management v2 events (management process).
/// Best effort: a notice that cannot be published is dropped after logging;
/// resources always reflect the store.
fn cutex_delivery_failed() -> &'static str {
    crate::asks::scheduler::DELIVERY_FAILED_EVENT
}

/// A delivery failure also waits on the owner (需要你 `ownerMessage`): the
/// requester agent's card, or the target's when the owner asked.
fn owner_need_for_delivery_failure(params: &Value) {
    let agent = params["requester"].as_str().or(params["target"].as_str());
    let Some(agent) = agent else {
        return;
    };
    let asks: Vec<&str> = params["askIds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let what = match params["kind"].as_str() {
        Some("results") => "委托结果没能作为消息送达",
        _ => "委托没能送达",
    };
    let text = format!("{what}（{}）：{}", asks.join(", "), params["detail"].as_str().unwrap_or_default());
    crate::documents::notify_owner("deliveryFailed", agent, None, text, asks.first().map(|ask| ask.to_string()));
}

pub fn publish_pending_notices(store: &AskStore) {
    let notices = match store.with_state(|state| std::mem::take(&mut state.notices)) {
        Ok(notices) => notices,
        Err(error) => {
            eprintln!("asks: could not read notices: {error:#}");
            return;
        }
    };
    if notices.is_empty() {
        return;
    }
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    for notice in notices.iter().filter(|notice| notice.method == cutex_delivery_failed()) {
        owner_need_for_delivery_failure(&notice.params);
    }
    for notice in notices {
        for session in &notice.sessions {
            if let Err(error) = super::server::append_cutex_event(
                repository,
                session,
                super::model::EventCorrelation::default(),
                &notice.method,
                notice.params.clone(),
            ) {
                eprintln!("asks: could not publish {}: {error:#}", notice.method);
            }
        }
    }
}

// ---- Commands (owner as requester / owner control), asks design §10 ----
//
// On `POST /v2/sessions/{id}/cutex/requests`, root credential only; a
// non-owner user with `send` may run stop / close / retry / respond /
// resume / reopen on an ask it sees whose requester or target is `{id}`
// (P2 access, `access::ask_command_target`); create and pause / resume
// stay the owner's:
// - `cutex/ask/create {text, actionId, followUp?, review?, ifBusy?,
//   attachments?}`: an owner ask to session `{id}` (never gated) →
//   `{ask, created}`.
// - `cutex/ask/stop {askId}`: dequeue, or interrupt the running attempt
//   with the gate-preserving interrupt → `{ask, stopping}`.
// - `cutex/ask/close {askId, outcome: accepted|abandoned}` → `{ask}`.
// - `cutex/ask/retry {askId}`: a new attempt of an interrupted ask → `{ask}`.
// - `cutex/ask/respond {questionId, answers}`: the owner answers a question
//   (first response wins) → `{question}`.

pub const CREATE_METHOD: &str = "cutex/ask/create";
pub const STOP_METHOD: &str = "cutex/ask/stop";
pub const CLOSE_METHOD: &str = "cutex/ask/close";
pub const RETRY_METHOD: &str = "cutex/ask/retry";
pub const RESPOND_METHOD: &str = "cutex/ask/respond";
/// Wake a delegation that waits for jobs now (delegation await §2.5):
/// `{askId, note?}` → `{ask}`. Not `cutex/asks/resume` (admission pause).
pub const ASK_RESUME_METHOD: &str = "cutex/ask/resume";
/// Reopen an answered delegation (§4): `{askId, note}` → `{ask}`.
pub const REOPEN_METHOD: &str = "cutex/ask/reopen";
/// Owner delegation pause of session `{id}` (r208, `asks-pause-v2`): `{}` →
/// `{asksPaused: {since, by, origin}, changed}`. The session admits no asks
/// and receives no ANSWER/QUESTION events until `cutex/asks/resume` or the
/// owner's next message to it.
pub const PAUSE_METHOD: &str = "cutex/asks/pause";
/// `{}` → `{asksPaused: null, changed}`.
pub const RESUME_METHOD: &str = "cutex/asks/resume";

pub fn is_command(method: &str) -> bool {
    matches!(
        method,
        CREATE_METHOD
            | STOP_METHOD
            | CLOSE_METHOD
            | RETRY_METHOD
            | RESPOND_METHOD
            | ASK_RESUME_METHOD
            | REOPEN_METHOD
            | PAUSE_METHOD
            | RESUME_METHOD
    )
}

/// An ask refusal as a Management v2 operation error.
pub(super) fn operation_error(error: crate::asks::store::AskError) -> super::user_input::UserInputExecutionError {
    super::user_input::UserInputExecutionError {
        stage: "route".into(),
        code: error.code.into(),
        message: error.detail,
        retryable: matches!(error.code, "persistence_unavailable" | "asks_unavailable" | "approvals_unavailable"),
        details: if error.extra.is_null() { json!({}) } else { error.extra },
        outcome_unknown: false,
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, crate::asks::store::AskError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| crate::asks::store::AskError::new("invalid_arguments", format!("{key} is required")))
}

/// Pure command execution against a store and world (testable).
pub fn execute_command(
    store: &AskStore,
    world: &dyn crate::asks::scheduler::SchedulerWorld,
    cutex_session_id: &str,
    method: &str,
    params: &Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Value, crate::asks::store::AskError> {
    use crate::asks::model::*;
    use crate::asks::store::AskError;
    let unavailable = |error: anyhow::Error| AskError::new("asks_unavailable", format!("{error:#}"));
    let ask_view = |ask_id: &str| -> Result<Value, AskError> {
        let state = store.snapshot().map_err(unavailable)?;
        let ask = state.asks.get(ask_id).ok_or_else(|| AskError::new("ask_not_found", "no such ask"))?;
        Ok(crate::asks::view::owner_view(&state, ask))
    };
    match method {
        CREATE_METHOD => {
            let directory = world.directory()?;
            let target = directory.resolve(None, cutex_session_id)?.clone();
            let text = str_param(params, "text")?;
            let attachments: Vec<String> = params
                .get("attachments")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let options = AskOptions {
                if_busy: params
                    .get("ifBusy")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_default(),
                review: params.get("review").and_then(Value::as_bool).unwrap_or(false),
                questions: QuestionRoute::Owner,
                notify: Notify::Turn,
                context: ContextOption::Brief,
            };
            let action_id = str_param(params, "actionId")?;
            if options.if_busy == IfBusy::Fail {
                // Refuse at once like agent_ask (a retried actionId replays).
                let state = store.snapshot().map_err(unavailable)?;
                if state.find_by_action(&Requester::Owner, action_id).is_none() {
                    if let Some(refusal) = crate::asks::tools::busy_refusal(&state, &target, "ifBusy \"queue\"") {
                        return Err(refusal);
                    }
                }
            }
            let body = crate::asks::tools::ask_body(store, text, attachments, &[target.cutex_session_id.clone()], now)?;
            let new = crate::asks::store::NewAsk {
                ask_id: None,
                action_id: action_id.to_string(),
                requester: Requester::Owner,
                target_cutex_session_id: target.cutex_session_id.clone(),
                target_name: target.name.clone(),
                project_id: target.project.clone(),
                body,
                title: None,
                options,
                follow_up_of: params.get("followUp").and_then(Value::as_str).map(str::to_string),
                parent_ask: None,
                gate: None,
            };
            let (ask, created) = crate::asks::tools::create_ask(store, world, new, now)?;
            Ok(json!({"ask": ask_view(&ask.ask_id)?, "created": created}))
        }
        STOP_METHOD => {
            let ask_id = str_param(params, "askId")?;
            let stop_jobs = params.get("stopJobs").and_then(Value::as_bool).unwrap_or(false);
            let result = crate::asks::tools::close_ask_with(store, None, ask_id, "stop", stop_jobs, now)?;
            let mut out = json!({"ask": ask_view(ask_id)?, "stopping": result["stopping"]});
            if stop_jobs {
                out["cancellingJobs"] = result["cancellingJobs"].clone();
            }
            Ok(out)
        }
        CLOSE_METHOD => {
            let ask_id = str_param(params, "askId")?;
            let outcome = str_param(params, "outcome")?;
            if outcome == "changesRequested" {
                let note = params.get("note").and_then(Value::as_str);
                store
                    .with_state(|state| crate::asks::awaits::reopen(state, ask_id, None, note, now))
                    .map_err(unavailable)??;
                return Ok(json!({"ask": ask_view(ask_id)?}));
            }
            if !matches!(outcome, "accepted" | "abandoned") {
                return Err(AskError::new(
                    "invalid_arguments",
                    "outcome: accepted, abandoned or changesRequested (cutex/ask/stop stops)",
                ));
            }
            crate::asks::tools::close_ask(store, None, ask_id, outcome, now)?;
            Ok(json!({"ask": ask_view(ask_id)?}))
        }
        ASK_RESUME_METHOD => {
            let ask_id = str_param(params, "askId")?;
            let note = params.get("note").and_then(Value::as_str);
            store
                .with_state(|state| crate::asks::awaits::resume_now(state, ask_id, None, note, now))
                .map_err(unavailable)??;
            Ok(json!({"ask": ask_view(ask_id)?}))
        }
        REOPEN_METHOD => {
            let ask_id = str_param(params, "askId")?;
            let note = params.get("note").and_then(Value::as_str);
            store
                .with_state(|state| crate::asks::awaits::reopen(state, ask_id, None, note, now))
                .map_err(unavailable)??;
            Ok(json!({"ask": ask_view(ask_id)?}))
        }
        RETRY_METHOD => {
            let ask_id = str_param(params, "askId")?;
            store
                .with_state(|state| crate::asks::lifecycle::retry(state, ask_id, None, now))
                .map_err(unavailable)??;
            Ok(json!({"ask": ask_view(ask_id)?}))
        }
        RESPOND_METHOD => {
            let question_id = str_param(params, "questionId")?;
            let answers = params.get("answers").cloned().unwrap_or(Value::Null);
            let question = crate::asks::questions::respond_in_store(
                store,
                question_id,
                &crate::asks::questions::Responder::Owner,
                &answers,
                now,
            )?;
            Ok(json!({"question": question}))
        }
        PAUSE_METHOD | RESUME_METHOD => {
            if !params.as_object().is_some_and(|params| params.is_empty()) {
                return Err(AskError::new("invalid_arguments", "params must be an empty object"));
            }
            if method == PAUSE_METHOD {
                crate::asks::control::owner_pause_in_store(store, cutex_session_id, now).map_err(unavailable)
            } else {
                crate::asks::control::owner_resume_in_store(store, cutex_session_id, now).map_err(unavailable)
            }
        }
        other => Err(AskError::new("invalid_arguments", format!("unknown ask method {other}"))),
    }
}

/// Dispatch from `handle_cutex_request` (root credential required).
pub(super) fn dispatch_command(
    owner: Option<&crate::management::control_plane::HumanManagementPrincipal>,
    cutex_session_id: &str,
    method: &str,
    params: &Value,
) -> Result<Value, super::user_input::UserInputExecutionError> {
    if owner.is_none() {
        return Err(super::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: "cutex_method_denied".into(),
            message: "asks require the Management root credential".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    }
    let store = AskStore::open_default().map_err(|error| {
        operation_error(crate::asks::store::AskError::new("asks_unavailable", format!("{error:#}")))
    })?;
    let world = crate::asks::scheduler::HostWorld::load();
    let result = execute_command(&store, &world, cutex_session_id, method, params, chrono::Utc::now())
        .map_err(operation_error);
    publish_pending_notices(&store);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::model::*;
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER};

    #[test]
    fn paths_parse_strictly() {
        assert_eq!(parse_path("/v2/asks"), Some(Route::Collection));
        assert_eq!(parse_path("/v2/asks/ask-abc"), Some(Route::Ask("ask-abc".into())));
        assert_eq!(parse_path("/v2/asks/ask-abc/answer"), Some(Route::Answer("ask-abc".into())));
        assert_eq!(parse_path("/v2/artifacts/art-00"), Some(Route::Artifact("art-00".into())));
        assert_eq!(parse_path("/v2/asks/../x"), None);
        assert_eq!(parse_path("/v2/asks/ask-a/other"), None);
        assert_eq!(parse_path("/v2/artifacts/x"), None);
        assert!(is_asks_path("/v2/asks/ask-x"));
        assert_eq!(parse_path("/v2/asks/provenance"), Some(Route::Provenance));
    }

    #[test]
    fn list_detail_answer_pages_and_artifacts() {
        let (store, dir) = temp_store("v2");
        let now = chrono::Utc::now();
        let text = "a".repeat(ANSWER_PAGE_BYTES + 10);
        let artifact = store.put_artifact(text.as_bytes(), "text/plain", &[], None, now).unwrap();
        let ask_id = store
            .with_state(|state| {
                let (ask, _) = state.create_ask(new_ask("x", "Review"), now).unwrap();
                state.answers.insert(
                    "ans-1".into(),
                    AnswerRecord {
                        answer_id: "ans-1".into(),
                        ask_id: ask.ask_id.clone(),
                        attempt: 1,
                        revision: 1,
                        status: AnswerStatus::Completed,
                        artifact_id: Some(artifact.clone()),
                        bytes: text.len(),
                        metadata: AnswerMetadata::default(),
                        created_at: "t".into(),
                        preview: None,
                    },
                );
                state.update_ask(&ask.ask_id, now, |ask| {
                    ask.answer_id = Some("ans-1".into());
                    ask.execution = ExecutionState::Answered;
                });
                state.create_ask(new_ask("y", "Other"), now).unwrap();
                ask.ask_id
            })
            .unwrap();
        let list = route_json(&store, "/v2/asks?state=answered", &Route::Collection).ok().unwrap();
        assert_eq!(list["asks"].as_array().unwrap().len(), 1);
        let all = route_json(&store, &format!("/v2/asks?requester={REQUESTER}"), &Route::Collection).ok().unwrap();
        assert_eq!(all["asks"][0]["title"], "Other");
        let detail = route_json(&store, "", &Route::Ask(ask_id.clone())).ok().unwrap();
        assert_eq!(detail["ask"]["answer"]["answerId"], "ans-1");
        let first = route_json(&store, "", &Route::Answer(ask_id.clone())).ok().unwrap();
        assert_eq!(first["nextCursor"], ANSWER_PAGE_BYTES);
        let second = route_json(&store, &format!("/x?cursor={ANSWER_PAGE_BYTES}"), &Route::Answer(ask_id.clone())).ok().unwrap();
        assert_eq!(second["text"].as_str().unwrap().len(), 10);
        assert!(second["nextCursor"].is_null());
        assert_eq!(route_json(&store, "", &Route::Ask("ask-none".into())).err().unwrap().code, "ask_not_found");
        let art = route_json(&store, "", &Route::Artifact(artifact)).ok().unwrap();
        assert_eq!(art["encoding"], "utf8");
        let _ = std::fs::remove_dir_all(dir);
    }


    /// r208: `involving` matches either party; `limit` pages newest first
    /// with `hasMore`; a bad limit is a 400.
    #[test]
    fn list_involving_limit_and_has_more() {
        use crate::asks::store::tests::TARGET;
        let (store, dir) = temp_store("v2-limit");
        let now = chrono::Utc::now();
        store
            .with_state(|state| {
                for n in 0..3 {
                    state.create_ask(new_ask(&format!("a{n}"), &format!("Ask {n}")), now).unwrap();
                }
            })
            .unwrap();
        let page = route_json(&store, "/v2/asks?limit=2", &Route::Collection).ok().unwrap();
        assert_eq!(page["asks"].as_array().unwrap().len(), 2);
        assert_eq!(page["hasMore"], true);
        let all = route_json(&store, "/v2/asks", &Route::Collection).ok().unwrap();
        assert_eq!(all["asks"].as_array().unwrap().len(), 3);
        assert_eq!(all["hasMore"], false);
        for party in [REQUESTER, TARGET] {
            let found = route_json(&store, &format!("/v2/asks?involving={party}"), &Route::Collection).ok().unwrap();
            assert_eq!(found["asks"].as_array().unwrap().len(), 3, "{party}");
        }
        let none = route_json(&store, "/v2/asks?involving=cutex.00000000-0000-0000-0000-0000000000ff", &Route::Collection).ok().unwrap();
        assert!(none["asks"].as_array().unwrap().is_empty());
        for bad in ["0", "1001", "x"] {
            let error = route_json(&store, &format!("/v2/asks?limit={bad}"), &Route::Collection).err().unwrap();
            assert_eq!(error.status, 400, "{bad}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The owner as requester: create (never gated, deduplicated on
    /// actionId), stop a queued ask, answer a question, retry.
    #[test]
    fn owner_commands() {
        let (world, dir) = crate::asks::scheduler::tests::world("v2cmd");
        world.inputs.borrow_mut().target = crate::asks::gate::tests::party(
            crate::asks::store::tests::TARGET,
            crate::agent_management::envelope::SandboxLevel::FullAccess,
            &[],
            "x",
        );
        let (store, store_dir) = temp_store("v2cmd");
        let now = chrono::Utc::now();
        let target = crate::asks::store::tests::TARGET;
        let params = json!({"text": "Review the parser", "actionId": "owner-1"});
        let created = execute_command(&store, &world, target, CREATE_METHOD, &params, now).unwrap();
        assert_eq!(created["ask"]["execution"], "queued");
        assert_eq!(created["ask"]["requester"]["kind"], "owner");
        let again = execute_command(&store, &world, target, CREATE_METHOD, &params, now).unwrap();
        assert_eq!(again["created"], false);
        // ifBusy "fail" refuses at once while the target has queued work,
        // and creates nothing.
        let fail_fast = json!({"text": "Now or never", "actionId": "owner-2", "ifBusy": "fail"});
        let busy = execute_command(&store, &world, target, CREATE_METHOD, &fail_fast, now).unwrap_err();
        assert_eq!(busy.code, "busy");
        assert!(store.snapshot().unwrap().find_by_action(&Requester::Owner, "owner-2").is_none());
        let ask_id = created["ask"]["askId"].as_str().unwrap().to_string();
        let stopped = execute_command(&store, &world, target, STOP_METHOD, &json!({"askId": ask_id}), now).unwrap();
        assert_eq!(stopped["ask"]["execution"], "cancelled");
        let missing = execute_command(&store, &world, target, RETRY_METHOD, &json!({"askId": "ask-x"}), now).unwrap_err();
        assert_eq!(missing.code, "ask_not_found");
        let bad = execute_command(&store, &world, target, CLOSE_METHOD, &json!({"askId": ask_id, "outcome": "stop"}), now).unwrap_err();
        assert_eq!(bad.code, "invalid_arguments");
        assert_eq!(operation_error(bad).code, "invalid_arguments");
        // Delegation await owner commands (cutex/ask/resume, reopen,
        // changesRequested): refused on a cancelled ask with plain codes.
        assert!(is_command(ASK_RESUME_METHOD) && is_command(REOPEN_METHOD));
        assert!(crate::management::v2::session::cutex_method_is_registered(ASK_RESUME_METHOD));
        assert_eq!(execute_command(&store, &world, target, ASK_RESUME_METHOD, &json!({"askId": ask_id}), now).unwrap_err().code, "ask_closed");
        assert_eq!(
            execute_command(&store, &world, target, REOPEN_METHOD, &json!({"askId": ask_id, "note": "more"}), now).unwrap_err().code,
            "ask_not_reopenable"
        );
        assert_eq!(
            execute_command(&store, &world, target, CLOSE_METHOD, &json!({"askId": ask_id, "outcome": "changesRequested", "note": "x"}), now)
                .unwrap_err()
                .code,
            "ask_not_reopenable"
        );
        // Owner delegation pause / resume (r208).
        let paused = execute_command(&store, &world, target, PAUSE_METHOD, &json!({}), now).unwrap();
        assert_eq!(paused["changed"], true);
        assert_eq!(paused["asksPaused"]["origin"], "ownerPause");
        assert!(store.snapshot().unwrap().pauses.contains_key(target));
        let again = execute_command(&store, &world, target, PAUSE_METHOD, &json!({}), now).unwrap();
        assert_eq!(again["changed"], false);
        assert_eq!(
            execute_command(&store, &world, target, PAUSE_METHOD, &json!({"x": 1}), now).unwrap_err().code,
            "invalid_arguments"
        );
        let resumed = execute_command(&store, &world, target, RESUME_METHOD, &json!({}), now).unwrap();
        assert_eq!(resumed, json!({"asksPaused": null, "changed": true}));
        assert!(store.snapshot().unwrap().pauses.is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[test]
    fn command_requests_and_responses_match_the_contract() {
        use super::super::contract_validation::{validate_cutex_request, validate_cutex_response};
        let request = |method: &str, params: Value| json!({"requestId": "r-1", "method": method, "params": params});
        validate_cutex_request(&request(CREATE_METHOD, json!({"text": "hi", "actionId": "a-1", "review": true}))).unwrap();
        validate_cutex_request(&request(STOP_METHOD, json!({"askId": "ask-1"}))).unwrap();
        validate_cutex_request(&request(RETRY_METHOD, json!({"askId": "ask-1"}))).unwrap();
        validate_cutex_request(&request(CLOSE_METHOD, json!({"askId": "ask-1", "outcome": "accepted"}))).unwrap();
        validate_cutex_request(&request(RESPOND_METHOD, json!({"questionId": "q-1", "answers": {}}))).unwrap();
        assert!(validate_cutex_request(&request(CREATE_METHOD, json!({"text": "hi"}))).is_err());
        assert!(validate_cutex_request(&request(CLOSE_METHOD, json!({"askId": "a", "outcome": "stop"}))).is_err());
        validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r-1", "cutexSessionId": "cutex.x",
            "cutex": {"method": CREATE_METHOD, "result": {"ask": {}, "created": true}}})).unwrap();
        for method in [PAUSE_METHOD, RESUME_METHOD] {
            validate_cutex_request(&request(method, json!({}))).unwrap();
            assert!(validate_cutex_request(&request(method, json!({"x": 1}))).is_err());
        }
        validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r-1", "cutexSessionId": "cutex.x",
            "cutex": {"method": PAUSE_METHOD, "result": {"asksPaused": {"since": "2026-10-08T00:00:00.000Z", "by": "owner", "origin": "ownerPause"}, "changed": true}}})).unwrap();
        validate_cutex_response(&json!({"contractVersion": 2, "requestId": "r-1", "cutexSessionId": "cutex.x",
            "cutex": {"method": RESUME_METHOD, "result": {"asksPaused": null, "changed": false}}})).unwrap();
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": crate::asks::control::PAUSE_CHANGED_EVENT,
            "params": {"asksPaused": null, "reason": "ownerMessage"},
        }))
        .unwrap();
        assert!(super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": crate::asks::control::PAUSE_CHANGED_EVENT,
            "params": {"asksPaused": null, "reason": "stopped"},
        }))
        .is_err());
        for method in [CREATE_METHOD, STOP_METHOD, CLOSE_METHOD, RETRY_METHOD, RESPOND_METHOD, PAUSE_METHOD, RESUME_METHOD] {
            assert!(super::super::session::cutex_method_is_registered(method), "{method}");
        }
    }
}
